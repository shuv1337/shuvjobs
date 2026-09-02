//! The scheduler-agnostic write model: what the operator asked for, what
//! that costs on disk, and how to apply it.
//!
//! A [`JobWriter`] never touches the host directly. It reads through the
//! [`Host`], turns the request into a [`Vec<Change>`] — file writes that
//! carry both `before` and `after`, and commands that carry their own
//! failure policy — and hands that plan to [`execute`], which either
//! renders it ([`ApplyMode::DryRun`]) or applies it in order with
//! best-effort rollback of the files it already touched. That split is
//! what makes `--dry-run`, the TUI confirm popup, and the real apply the
//! same code path with one flag flipped.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::host::shell::{shell_safe_filename, shell_safe_username};
use crate::host::{CmdOutput, Host, Privilege};
use crate::{Error, Result, ScheduleType, TaskSourceKind};

/// Whether a job belongs to the machine or to the invoking user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobScope {
    /// `/etc/cron.d`, `/etc/systemd/system`, `/Library/LaunchDaemons`.
    System,
    /// The invoking user's crontab, `~/.config/systemd/user`, LaunchAgents.
    User,
}

/// A job as the operator described it, before any scheduler decides what
/// file it lives in. Every writer takes this shape; the per-scheduler
/// knobs that do not generalise live in `options`
/// (for example `anacron.delay`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub source: TaskSourceKind,
    /// Unit name, `cron.d` filename, anacron job id, launchd label.
    /// `None` lets the writer derive one, or means the source has no name.
    pub name: Option<String>,
    pub schedule: ScheduleType,
    pub command: String,
    /// Run the job as this user. Only meaningful in system scope.
    pub user: Option<String>,
    pub scope: JobScope,
    pub enabled: bool,
    pub description: Option<String>,
    /// Backing file, when the caller already knows it (edit of a job we
    /// just read). Writers may use it to locate the job again.
    pub location: Option<String>,
    pub options: BTreeMap<String, String>,
}

impl JobSpec {
    pub fn new(source: TaskSourceKind, schedule: ScheduleType, command: String) -> Self {
        Self {
            source,
            name: None,
            schedule,
            command,
            user: None,
            scope: JobScope::User,
            enabled: true,
            description: None,
            location: None,
            options: BTreeMap::new(),
        }
    }

    /// Reject anything that would corrupt a line-oriented config file or
    /// escape a shell word. Everything here is a caller mistake, so it is
    /// [`Error::Validation`] and never reaches the host.
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            return Err(Error::Validation("command must not be empty".to_string()));
        }
        // crontab, anacrontab, and unit files are all line-oriented: a
        // newline anywhere in a field would forge extra jobs.
        let fields: [(&str, Option<&str>); 5] = [
            ("command", Some(self.command.as_str())),
            ("name", self.name.as_deref()),
            ("user", self.user.as_deref()),
            ("description", self.description.as_deref()),
            ("location", self.location.as_deref()),
        ];
        for (label, value) in fields {
            if let Some(value) = value {
                if value.contains('\n') || value.contains('\r') {
                    return Err(Error::Validation(format!(
                        "{label} must not contain a line break"
                    )));
                }
            }
        }
        if let Some(user) = self.user.as_deref() {
            if !shell_safe_username(user) {
                return Err(Error::Validation(format!("invalid user name: {user}")));
            }
        }
        if let Some(name) = self.name.as_deref() {
            if !shell_safe_filename(name) {
                return Err(Error::Validation(format!("invalid job name: {name}")));
            }
        }
        Ok(())
    }
}

/// Cron's `@` shorthands, which several sources borrow.
const CRON_ALIASES: &[&str] = &[
    "@reboot",
    "@hourly",
    "@daily",
    "@weekly",
    "@monthly",
    "@yearly",
    "@annually",
];

/// Anacron periods: the same shorthands minus `@reboot` and `@hourly`,
/// neither of which anacron can express.
const ANACRON_ALIASES: &[&str] = &["@daily", "@weekly", "@monthly", "@yearly", "@annually"];

/// `30m`, `2h`, `1d`, `45s` — the one schedule syntax every source
/// accepts from the operator, whatever it turns into on disk.
fn parse_interval_suffix(text: &str) -> Option<Duration> {
    let (digits, unit) = text.split_at_checked(text.len().checked_sub(1)?)?;
    let secs_per = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(value.checked_mul(secs_per)?))
}

/// Exactly five whitespace-separated fields, normalised to single spaces.
fn parse_five_field_cron(text: &str) -> Option<String> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    (fields.len() == 5).then(|| fields.join(" "))
}

fn parse_one_shot(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    // A bare wall-clock time is held as UTC here; the `at` writer
    // re-renders it in the host's own offset, which is the only place
    // that knows what that offset is.
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").ok()?;
    Utc.from_local_datetime(&naive).single()
}

/// Our own rendering of a launchd `StartCalendarInterval`, as the read
/// path formats it: `Hour=9 Minute=0` for one entry, entries joined by
/// ` | `. Accepted back so an edit can round-trip what `list` showed.
fn looks_like_calendar_interval(text: &str) -> bool {
    const KEYS: &[&str] = &["Minute", "Hour", "Day", "Weekday", "Month"];
    let mut any = false;
    for segment in text.split('|') {
        let mut fields = 0;
        for field in segment.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                return false;
            };
            if !KEYS.contains(&key) {
                return false;
            }
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            fields += 1;
            any = true;
        }
        if fields == 0 {
            return false;
        }
    }
    any
}

fn invalid_schedule(kind: TaskSourceKind, text: &str) -> Error {
    Error::Validation(format!("{kind}: cannot understand schedule `{text}`"))
}

/// Turn the operator's schedule text into the source's own schedule type.
///
/// `30m`/`2h`/`1d` is accepted for every source; everything else is
/// per-source, because `0 9 * * *` means one thing to cron and nothing to
/// launchd. Expressions the source itself validates (systemd
/// `OnCalendar=`, `at`'s free-text times) pass through as text and are
/// verified later by the writer, against the host.
pub fn parse_schedule(kind: TaskSourceKind, text: &str) -> Result<ScheduleType> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Validation("schedule must not be empty".to_string()));
    }
    if let Some(interval) = parse_interval_suffix(text) {
        return Ok(ScheduleType::Interval(interval));
    }
    match kind {
        TaskSourceKind::Cron => {
            let lowered = text.to_ascii_lowercase();
            if CRON_ALIASES.contains(&lowered.as_str()) {
                return Ok(ScheduleType::Cron(lowered));
            }
            parse_five_field_cron(text)
                .map(ScheduleType::Cron)
                .ok_or_else(|| invalid_schedule(kind, text))
        }
        TaskSourceKind::Anacron => {
            let lowered = text.to_ascii_lowercase();
            if ANACRON_ALIASES.contains(&lowered.as_str()) {
                return Ok(ScheduleType::Calendar(lowered));
            }
            // A bare number is a period in days, which is what the
            // anacrontab's first column holds.
            if let Ok(days) = text.parse::<u64>() {
                if days > 0 {
                    return Ok(ScheduleType::Interval(Duration::from_secs(days * 86400)));
                }
            }
            Err(invalid_schedule(kind, text))
        }
        TaskSourceKind::At => {
            if let Some(when) = parse_one_shot(text) {
                return Ok(ScheduleType::OneShot(when));
            }
            // `at` has its own English-ish grammar (`now + 5 minutes`,
            // `tomorrow 09:00`); the host's `at` is the only authority.
            Ok(ScheduleType::Calendar(text.to_string()))
        }
        TaskSourceKind::Systemd => {
            if let Some(cron) = parse_five_field_cron(text) {
                return Ok(ScheduleType::Cron(cron));
            }
            // Anything else is an `OnCalendar=` expression, verified by
            // `systemd-analyze calendar` when the writer runs.
            Ok(ScheduleType::Calendar(text.to_string()))
        }
        TaskSourceKind::Launchd => {
            // Bare seconds are `StartInterval`, launchd's own unit.
            if let Ok(secs) = text.parse::<u64>() {
                return Ok(ScheduleType::Interval(Duration::from_secs(secs)));
            }
            // Checked before cron: `Hour=9 Minute=0 | Weekday=1 Hour=3`
            // also happens to be five whitespace-separated words.
            if looks_like_calendar_interval(text) {
                return Ok(ScheduleType::Calendar(text.to_string()));
            }
            if let Some(cron) = parse_five_field_cron(text) {
                return Ok(ScheduleType::Cron(cron));
            }
            Err(invalid_schedule(kind, text))
        }
    }
}

/// Render the plan, or carry it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    DryRun,
    Apply,
}

/// What a non-zero exit from a planned command means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailPolicy {
    /// Any non-zero exit aborts the plan.
    Error,
    /// The command is advisory (`systemctl reset-failed`, `bootout` of a
    /// job that was never loaded).
    Ignore,
    /// These exit codes are expected (`launchctl bootout` exits 3 when
    /// the service is not loaded).
    AllowExit(Vec<i32>),
}

/// One step of a plan. File steps carry `before` so the executor can put
/// the file back, and so the CLI can render a unified diff without
/// re-reading the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    WriteFile {
        path: String,
        /// `None` when the file is being created.
        before: Option<String>,
        after: String,
        mode: u32,
        privilege: Privilege,
    },
    RemoveFile {
        path: String,
        before: Option<String>,
        privilege: Privilege,
    },
    Command {
        cmd: String,
        stdin: Option<String>,
        privilege: Privilege,
        /// Operator-facing phrasing: "reload the systemd manager".
        description: String,
        on_fail: FailPolicy,
    },
}

impl Change {
    /// One line for the plan listing and for the success report.
    pub fn summary(&self) -> String {
        match self {
            Change::WriteFile { path, mode, .. } => format!("write {path} ({mode:o})"),
            Change::RemoveFile { path, .. } => format!("remove {path}"),
            Change::Command { description, .. } => format!("run: {description}"),
        }
    }
}

/// What one mutation did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutationOutcome {
    /// The job's id, once it is known. Cron per-user jobs and `at` jobs
    /// only get one after the fact, so the writer fills this in last.
    pub id: Option<String>,
    pub changes: Vec<Change>,
    pub applied: bool,
    /// One entry per planned [`Change::Command`], in order.
    pub outputs: Vec<CmdOutput>,
    pub notes: Vec<String>,
}

impl MutationOutcome {
    /// `(path, previous contents)` for every file this plan overwrites or
    /// removes, so the session can back them up before applying.
    pub fn backup_targets(&self) -> Vec<(&str, &str)> {
        self.changes
            .iter()
            .filter_map(|change| match change {
                Change::WriteFile { path, before, .. }
                | Change::RemoveFile { path, before, .. } => {
                    before.as_deref().map(|before| (path.as_str(), before))
                }
                Change::Command { .. } => None,
            })
            .collect()
    }
}

/// One scheduler's write path. Implementations plan against the host
/// (reads only) and return what [`execute`] produced.
pub trait JobWriter: Send + Sync {
    fn kind(&self) -> TaskSourceKind;
    fn create(&self, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome>;
    fn update(&self, id: &str, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome>;
    fn delete(&self, id: &str, mode: ApplyMode) -> Result<MutationOutcome>;
    fn set_enabled(&self, id: &str, enabled: bool, mode: ApplyMode) -> Result<MutationOutcome>;
}

/// The plan listing: one line per change, in order.
pub fn render_changes(changes: &[Change]) -> String {
    changes
        .iter()
        .map(Change::summary)
        .collect::<Vec<_>>()
        .join("\n")
}

fn tolerates(policy: &FailPolicy, output: &CmdOutput) -> bool {
    match policy {
        FailPolicy::Ignore => true,
        FailPolicy::Error => output.success(),
        FailPolicy::AllowExit(codes) => {
            output.success() || output.code.is_some_and(|code| codes.contains(&code))
        }
    }
}

/// Undo the file changes already applied, newest first. Commands are not
/// undone — there is no general inverse for `daemon-reload` — so this is
/// explicitly best effort and every failure becomes a note.
fn roll_back(host: &dyn Host, applied: &[Change]) -> Vec<String> {
    let mut notes = Vec::new();
    for change in applied.iter().rev() {
        let result = match change {
            Change::WriteFile {
                path,
                before: Some(before),
                mode,
                privilege,
                ..
            } => host.write_file(path, before.as_bytes(), *mode, *privilege),
            Change::WriteFile {
                path,
                before: None,
                privilege,
                ..
            } => host.remove_file(path, *privilege).map(|_| ()),
            Change::RemoveFile {
                path,
                before: Some(before),
                privilege,
            } => host.write_file(path, before.as_bytes(), 0o644, *privilege),
            Change::RemoveFile { before: None, .. } | Change::Command { .. } => Ok(()),
        };
        if let Err(err) = result {
            notes.push(format!("could not roll back {}: {err}", change.summary()));
        }
    }
    notes
}

/// Render or carry out a plan.
///
/// [`ApplyMode::DryRun`] makes no host calls at all, so a dry run is safe
/// against a machine the operator cannot write to. [`ApplyMode::Apply`]
/// runs the changes in order; the first failure rolls the already-written
/// files back and returns the error.
pub fn execute(host: &dyn Host, changes: Vec<Change>, mode: ApplyMode) -> Result<MutationOutcome> {
    let mut outcome = MutationOutcome {
        changes,
        ..MutationOutcome::default()
    };
    if mode == ApplyMode::DryRun {
        return Ok(outcome);
    }

    let mut applied: Vec<Change> = Vec::new();
    for change in &outcome.changes {
        match change {
            Change::WriteFile {
                path,
                after,
                mode,
                privilege,
                ..
            } => {
                if let Err(err) = host.write_file(path, after.as_bytes(), *mode, *privilege) {
                    return Err(fail(host, &applied, err));
                }
                applied.push(change.clone());
            }
            Change::RemoveFile {
                path, privilege, ..
            } => {
                if let Err(err) = host.remove_file(path, *privilege) {
                    return Err(fail(host, &applied, err));
                }
                applied.push(change.clone());
            }
            Change::Command {
                cmd,
                stdin,
                privilege,
                on_fail,
                ..
            } => {
                let output = match host.run(cmd, stdin.as_deref().map(str::as_bytes), *privilege) {
                    Ok(output) => output,
                    Err(err) => return Err(fail(host, &applied, err)),
                };
                outcome.outputs.push(output.clone());
                if !tolerates(on_fail, &output) {
                    let err = output
                        .require_success(cmd)
                        .err()
                        .unwrap_or_else(|| Error::Command {
                            command: cmd.clone(),
                            message: "command failed".to_string(),
                        });
                    return Err(fail(host, &applied, err));
                }
            }
        }
    }

    outcome.applied = true;
    Ok(outcome)
}

/// Roll back and shape the error the caller sees. A rollback that itself
/// failed leaves the host in a state neither the plan nor the backup
/// describes, so it is folded into the message rather than dropped.
fn fail(host: &dyn Host, applied: &[Change], err: Error) -> Error {
    let notes = roll_back(host, applied);
    if notes.is_empty() {
        return err;
    }
    Error::Other(format!("{err}; rollback incomplete: {}", notes.join("; ")))
}

/// One requested mutation, as the CLI and the TUI both express it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Create(JobSpec),
    Update {
        id: String,
        source: TaskSourceKind,
        spec: JobSpec,
    },
    Delete {
        id: String,
        source: TaskSourceKind,
    },
    SetEnabled {
        id: String,
        source: TaskSourceKind,
        enabled: bool,
    },
}

impl Op {
    pub fn source(&self) -> TaskSourceKind {
        match self {
            Op::Create(spec) => spec.source,
            Op::Update { source, .. }
            | Op::Delete { source, .. }
            | Op::SetEnabled { source, .. } => *source,
        }
    }

    /// The subcommand that produced this op, and the word used in the
    /// success line and the JSON report.
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Create(_) => "add",
            Op::Update { .. } => "edit",
            Op::Delete { .. } => "rm",
            Op::SetEnabled { enabled: true, .. } => "enable",
            Op::SetEnabled { enabled: false, .. } => "disable",
        }
    }

    /// `None` for a create: the id does not exist yet.
    pub fn id(&self) -> Option<&str> {
        match self {
            Op::Create(_) => None,
            Op::Update { id, .. } | Op::Delete { id, .. } | Op::SetEnabled { id, .. } => {
                Some(id.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::Call;
    use crate::host::{FakeHost, PrivilegePolicy};

    fn spec() -> JobSpec {
        JobSpec::new(
            TaskSourceKind::Cron,
            ScheduleType::Cron("0 9 * * *".to_string()),
            "echo hi".to_string(),
        )
    }

    fn write(path: &str, before: Option<&str>, after: &str) -> Change {
        Change::WriteFile {
            path: path.to_string(),
            before: before.map(str::to_string),
            after: after.to_string(),
            mode: 0o644,
            privilege: Privilege::User,
        }
    }

    fn command(cmd: &str, on_fail: FailPolicy) -> Change {
        Change::Command {
            cmd: cmd.to_string(),
            stdin: None,
            privilege: Privilege::User,
            description: format!("run {cmd}"),
            on_fail,
        }
    }

    #[test]
    fn new_spec_defaults_to_an_enabled_user_job() {
        let spec = spec();
        assert_eq!(spec.scope, JobScope::User);
        assert!(spec.enabled);
        assert_eq!(spec.name, None);
        assert!(spec.options.is_empty());
        spec.validate().unwrap();
    }

    #[test]
    fn scope_serialises_lowercase() {
        assert_eq!(
            serde_json::to_string(&JobScope::System).unwrap(),
            "\"system\""
        );
        let scope: JobScope = serde_json::from_str("\"user\"").unwrap();
        assert_eq!(scope, JobScope::User);
    }

    #[test]
    fn validate_rejects_bad_specs() {
        let cases: Vec<(&str, JobSpec)> = vec![
            ("empty command", {
                let mut s = spec();
                s.command = "   ".to_string();
                s
            }),
            ("newline in command", {
                let mut s = spec();
                s.command = "echo hi\n0 9 * * * root evil".to_string();
                s
            }),
            ("carriage return in command", {
                let mut s = spec();
                s.command = "echo hi\rmore".to_string();
                s
            }),
            ("newline in name", {
                let mut s = spec();
                s.name = Some("a\nb".to_string());
                s
            }),
            ("newline in user", {
                let mut s = spec();
                s.user = Some("root\nx".to_string());
                s
            }),
            ("newline in description", {
                let mut s = spec();
                s.description = Some("a\nb".to_string());
                s
            }),
            ("newline in location", {
                let mut s = spec();
                s.location = Some("/etc/cron.d/a\nb".to_string());
                s
            }),
            ("unsafe user", {
                let mut s = spec();
                s.user = Some("root; rm -rf /".to_string());
                s
            }),
            ("empty user", {
                let mut s = spec();
                s.user = Some(String::new());
                s
            }),
            ("unsafe name", {
                let mut s = spec();
                s.name = Some("../escape".to_string());
                s
            }),
        ];
        for (label, spec) in cases {
            let err = spec.validate().expect_err(label);
            assert!(matches!(err, Error::Validation(_)), "{label}: got {err:?}");
        }
    }

    #[test]
    fn validate_accepts_a_full_spec() {
        let mut spec = spec();
        spec.name = Some("nightly-backup.job".to_string());
        spec.user = Some("alice".to_string());
        spec.description = Some("nightly backup".to_string());
        spec.location = Some("/etc/cron.d/nightly".to_string());
        spec.scope = JobScope::System;
        spec.validate().unwrap();
    }

    #[test]
    fn parse_schedule_rejects_blank_text() {
        for kind in [
            TaskSourceKind::Cron,
            TaskSourceKind::Systemd,
            TaskSourceKind::At,
            TaskSourceKind::Anacron,
            TaskSourceKind::Launchd,
        ] {
            let err = parse_schedule(kind, "   ").expect_err("blank");
            assert!(matches!(err, Error::Validation(_)), "{kind}: {err:?}");
        }
    }

    #[test]
    fn parse_schedule_accepts_interval_suffixes_for_every_source() {
        let cases = [
            ("45s", 45),
            ("30m", 1800),
            ("2h", 7200),
            ("1d", 86400),
            (" 15m ", 900),
        ];
        for kind in [
            TaskSourceKind::Cron,
            TaskSourceKind::Systemd,
            TaskSourceKind::At,
            TaskSourceKind::Anacron,
            TaskSourceKind::Launchd,
        ] {
            for (text, secs) in cases {
                assert_eq!(
                    parse_schedule(kind, text).unwrap(),
                    ScheduleType::Interval(Duration::from_secs(secs)),
                    "{kind} {text}"
                );
            }
        }
    }

    #[test]
    fn parse_schedule_cron() {
        let ok = [
            ("@reboot", ScheduleType::Cron("@reboot".to_string())),
            ("@Daily", ScheduleType::Cron("@daily".to_string())),
            ("@weekly", ScheduleType::Cron("@weekly".to_string())),
            ("@monthly", ScheduleType::Cron("@monthly".to_string())),
            ("@yearly", ScheduleType::Cron("@yearly".to_string())),
            ("@annually", ScheduleType::Cron("@annually".to_string())),
            ("@hourly", ScheduleType::Cron("@hourly".to_string())),
            (
                "0   9 * *   1-5",
                ScheduleType::Cron("0 9 * * 1-5".to_string()),
            ),
            ("*/5 * * * *", ScheduleType::Cron("*/5 * * * *".to_string())),
        ];
        for (text, expected) in ok {
            assert_eq!(
                parse_schedule(TaskSourceKind::Cron, text).unwrap(),
                expected
            );
        }
        for text in ["0 9 * *", "0 9 * * * *", "@never", "tomorrow"] {
            assert!(
                parse_schedule(TaskSourceKind::Cron, text).is_err(),
                "{text} should not parse"
            );
        }
    }

    #[test]
    fn parse_schedule_anacron() {
        let ok = [
            ("@daily", ScheduleType::Calendar("@daily".to_string())),
            ("@WEEKLY", ScheduleType::Calendar("@weekly".to_string())),
            ("@monthly", ScheduleType::Calendar("@monthly".to_string())),
            ("@yearly", ScheduleType::Calendar("@yearly".to_string())),
            ("@annually", ScheduleType::Calendar("@annually".to_string())),
            ("7", ScheduleType::Interval(Duration::from_secs(604_800))),
            ("1", ScheduleType::Interval(Duration::from_secs(86_400))),
        ];
        for (text, expected) in ok {
            assert_eq!(
                parse_schedule(TaskSourceKind::Anacron, text).unwrap(),
                expected,
                "{text}"
            );
        }
        for text in ["0", "-3", "@reboot", "@hourly", "0 9 * * *", "nightly"] {
            assert!(
                parse_schedule(TaskSourceKind::Anacron, text).is_err(),
                "{text} should not parse"
            );
        }
    }

    #[test]
    fn parse_schedule_at() {
        let rfc = parse_schedule(TaskSourceKind::At, "2026-03-01T09:30:00Z").unwrap();
        assert_eq!(
            rfc,
            ScheduleType::OneShot(
                DateTime::parse_from_rfc3339("2026-03-01T09:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        // An offset is honoured, not ignored.
        assert_eq!(
            parse_schedule(TaskSourceKind::At, "2026-03-01T09:30:00+02:00").unwrap(),
            ScheduleType::OneShot(
                DateTime::parse_from_rfc3339("2026-03-01T07:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(
            parse_schedule(TaskSourceKind::At, "2026-03-01 09:30").unwrap(),
            ScheduleType::OneShot(
                DateTime::parse_from_rfc3339("2026-03-01T09:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        // Free text is `at`'s own grammar and passes through.
        assert_eq!(
            parse_schedule(TaskSourceKind::At, "now + 5 minutes").unwrap(),
            ScheduleType::Calendar("now + 5 minutes".to_string())
        );
        assert_eq!(
            parse_schedule(TaskSourceKind::At, "teatime tomorrow").unwrap(),
            ScheduleType::Calendar("teatime tomorrow".to_string())
        );
    }

    #[test]
    fn parse_schedule_systemd() {
        assert_eq!(
            parse_schedule(TaskSourceKind::Systemd, "0 9 * * 1-5").unwrap(),
            ScheduleType::Cron("0 9 * * 1-5".to_string())
        );
        for text in ["*-*-* 03:00:00", "daily", "Mon *-*-* 09:00:00"] {
            assert_eq!(
                parse_schedule(TaskSourceKind::Systemd, text).unwrap(),
                ScheduleType::Calendar(text.to_string()),
                "{text}"
            );
        }
    }

    #[test]
    fn parse_schedule_launchd() {
        assert_eq!(
            parse_schedule(TaskSourceKind::Launchd, "3600").unwrap(),
            ScheduleType::Interval(Duration::from_secs(3600))
        );
        assert_eq!(
            parse_schedule(TaskSourceKind::Launchd, "0 9 * * 1").unwrap(),
            ScheduleType::Cron("0 9 * * 1".to_string())
        );
        assert_eq!(
            parse_schedule(TaskSourceKind::Launchd, "Hour=9 Minute=0").unwrap(),
            ScheduleType::Calendar("Hour=9 Minute=0".to_string())
        );
        assert_eq!(
            parse_schedule(
                TaskSourceKind::Launchd,
                "Hour=9 Minute=0 | Weekday=1 Hour=3"
            )
            .unwrap(),
            ScheduleType::Calendar("Hour=9 Minute=0 | Weekday=1 Hour=3".to_string())
        );
        for text in ["*-*-* 03:00:00", "Hour=nine", "Second=1", "Hour=9 |", "|"] {
            assert!(
                parse_schedule(TaskSourceKind::Launchd, text).is_err(),
                "{text} should not parse"
            );
        }
    }

    #[test]
    fn change_summaries_are_one_liners() {
        assert_eq!(
            write("/etc/cron.d/x", None, "job\n").summary(),
            "write /etc/cron.d/x (644)"
        );
        assert_eq!(
            Change::RemoveFile {
                path: "/etc/cron.d/x".to_string(),
                before: Some("job\n".to_string()),
                privilege: Privilege::Root,
            }
            .summary(),
            "remove /etc/cron.d/x"
        );
        assert_eq!(
            command("systemctl daemon-reload", FailPolicy::Error).summary(),
            "run: run systemctl daemon-reload"
        );
        assert_eq!(
            render_changes(&[
                write("/a", None, "x"),
                command("systemctl daemon-reload", FailPolicy::Error),
            ]),
            "write /a (644)\nrun: run systemctl daemon-reload"
        );
    }

    #[test]
    fn backup_targets_lists_only_replaced_content() {
        let outcome = MutationOutcome {
            changes: vec![
                write("/etc/cron.d/new", None, "job\n"),
                write("/etc/cron.d/old", Some("was\n"), "job\n"),
                Change::RemoveFile {
                    path: "/etc/cron.d/gone".to_string(),
                    before: Some("bye\n".to_string()),
                    privilege: Privilege::Root,
                },
                Change::RemoveFile {
                    path: "/etc/cron.d/absent".to_string(),
                    before: None,
                    privilege: Privilege::Root,
                },
                command("true", FailPolicy::Error),
            ],
            ..MutationOutcome::default()
        };
        assert_eq!(
            outcome.backup_targets(),
            vec![("/etc/cron.d/old", "was\n"), ("/etc/cron.d/gone", "bye\n")]
        );
    }

    #[test]
    fn dry_run_makes_no_host_calls() {
        let host = FakeHost::new().file("/etc/crontab", "old\n");
        let changes = vec![
            write("/etc/crontab", Some("old\n"), "new\n"),
            command("systemctl daemon-reload", FailPolicy::Error),
        ];
        let outcome = execute(&host, changes.clone(), ApplyMode::DryRun).unwrap();
        assert!(!outcome.applied);
        assert_eq!(outcome.changes, changes);
        assert!(outcome.outputs.is_empty());
        assert!(host.calls().is_empty());
        assert_eq!(
            host.file_contents("/etc/crontab"),
            Some("old\n".to_string())
        );
    }

    #[test]
    fn apply_writes_files_and_records_command_outputs_in_order() {
        let host = FakeHost::new()
            .file("/etc/anacrontab", "old\n")
            .script("first", 0, "one\n", "")
            .script("second", 0, "two\n", "");
        let outcome = execute(
            &host,
            vec![
                write("/etc/anacrontab", Some("old\n"), "new\n"),
                command("first", FailPolicy::Error),
                Change::RemoveFile {
                    path: "/var/spool/anacron/job".to_string(),
                    before: Some("stamp\n".to_string()),
                    privilege: Privilege::User,
                },
                Change::Command {
                    cmd: "second".to_string(),
                    stdin: Some("payload\n".to_string()),
                    privilege: Privilege::User,
                    description: "second".to_string(),
                    on_fail: FailPolicy::Error,
                },
            ],
            ApplyMode::Apply,
        )
        .unwrap();

        assert!(outcome.applied);
        assert_eq!(
            host.file_contents("/etc/anacrontab"),
            Some("new\n".to_string())
        );
        assert_eq!(host.file_contents("/var/spool/anacron/job"), None);
        assert_eq!(
            outcome
                .outputs
                .iter()
                .map(|o| o.stdout_str().into_owned())
                .collect::<Vec<_>>(),
            vec!["one\n".to_string(), "two\n".to_string()]
        );
        assert_eq!(
            host.calls(),
            vec![
                Call {
                    cmd: "first".to_string(),
                    stdin: None,
                    privilege: Privilege::User,
                },
                Call {
                    cmd: "second".to_string(),
                    stdin: Some(b"payload\n".to_vec()),
                    privilege: Privilege::User,
                },
            ]
        );
    }

    #[test]
    fn allow_exit_accepts_the_listed_code() {
        let host = FakeHost::new().script("launchctl bootout gui/501/x", 3, "", "not loaded");
        let outcome = execute(
            &host,
            vec![command(
                "launchctl bootout gui/501/x",
                FailPolicy::AllowExit(vec![3]),
            )],
            ApplyMode::Apply,
        )
        .unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.outputs[0].code, Some(3));

        // A code that is not on the list still fails.
        let host = FakeHost::new().script("launchctl bootout gui/501/x", 5, "", "boom");
        assert!(execute(
            &host,
            vec![command(
                "launchctl bootout gui/501/x",
                FailPolicy::AllowExit(vec![3]),
            )],
            ApplyMode::Apply,
        )
        .is_err());
    }

    #[test]
    fn ignore_never_fails() {
        // Unscripted commands exit 127 on the fake host.
        let host = FakeHost::new();
        let outcome = execute(
            &host,
            vec![command("systemctl reset-failed x", FailPolicy::Ignore)],
            ApplyMode::Apply,
        )
        .unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.outputs[0].code, Some(127));
    }

    #[test]
    fn a_failing_command_restores_files_and_returns_the_error() {
        let host = FakeHost::new()
            .file("/etc/systemd/system/a.timer", "old timer\n")
            .file("/var/spool/anacron/job", "stamp\n")
            .script(
                "systemctl enable --now a.timer",
                1,
                "",
                "Failed to enable\n",
            );
        let err = execute(
            &host,
            vec![
                // Overwritten: must come back as `before`.
                write("/etc/systemd/system/a.timer", Some("old timer\n"), "new\n"),
                // Newly created: must be removed.
                write("/etc/systemd/system/a.service", None, "service\n"),
                // Removed: must come back.
                Change::RemoveFile {
                    path: "/var/spool/anacron/job".to_string(),
                    before: Some("stamp\n".to_string()),
                    privilege: Privilege::User,
                },
                command("systemctl enable --now a.timer", FailPolicy::Error),
            ],
            ApplyMode::Apply,
        )
        .expect_err("the command failed");

        match err {
            Error::Command { command, message } => {
                assert_eq!(command, "systemctl enable --now a.timer");
                assert_eq!(message, "Failed to enable");
            }
            other => panic!("expected Command, got {other:?}"),
        }
        assert_eq!(
            host.file_contents("/etc/systemd/system/a.timer"),
            Some("old timer\n".to_string())
        );
        assert_eq!(host.file_contents("/etc/systemd/system/a.service"), None);
        assert_eq!(
            host.file_contents("/var/spool/anacron/job"),
            Some("stamp\n".to_string())
        );
    }

    #[test]
    fn a_file_error_stops_the_plan_and_rolls_back() {
        let host = FakeHost::new()
            .file("/etc/cron.d/a", "old\n")
            .root_only("/etc/cron.d/locked");
        let err = execute(
            &host,
            vec![
                write("/etc/cron.d/a", Some("old\n"), "new\n"),
                write("/etc/cron.d/locked", None, "nope\n"),
                command("never runs", FailPolicy::Error),
            ],
            ApplyMode::Apply,
        )
        .expect_err("the second write is refused");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
        assert_eq!(
            host.file_contents("/etc/cron.d/a"),
            Some("old\n".to_string())
        );
        assert!(host.calls().is_empty());
    }

    /// A host whose files go read-only the moment a command has run, so
    /// the write succeeds and its rollback does not.
    #[derive(Debug)]
    struct RollbackFailsHost {
        inner: FakeHost,
        ran: std::sync::atomic::AtomicBool,
    }

    impl Host for RollbackFailsHost {
        fn policy(&self) -> PrivilegePolicy {
            self.inner.policy()
        }
        fn label(&self) -> String {
            self.inner.label()
        }
        fn current_user(&self) -> Result<String> {
            self.inner.current_user()
        }
        fn current_uid(&self) -> Result<u32> {
            self.inner.current_uid()
        }
        fn home_dir(&self) -> Result<String> {
            self.inner.home_dir()
        }
        fn os(&self) -> Result<crate::host::HostOs> {
            self.inner.os()
        }
        fn utc_offset(&self) -> Result<chrono::FixedOffset> {
            self.inner.utc_offset()
        }
        fn run(&self, cmd: &str, stdin: Option<&[u8]>, privilege: Privilege) -> Result<CmdOutput> {
            let output = self.inner.run(cmd, stdin, privilege);
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            output
        }
        fn read_file(&self, path: &str, privilege: Privilege) -> Result<Option<Vec<u8>>> {
            self.inner.read_file(path, privilege)
        }
        fn write_file(
            &self,
            path: &str,
            contents: &[u8],
            mode: u32,
            privilege: Privilege,
        ) -> Result<()> {
            if self.ran.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::Other("read-only file system".to_string()));
            }
            self.inner.write_file(path, contents, mode, privilege)
        }
        fn remove_file(&self, path: &str, privilege: Privilege) -> Result<bool> {
            self.inner.remove_file(path, privilege)
        }
        fn exists(&self, path: &str, privilege: Privilege) -> Result<bool> {
            self.inner.exists(path, privilege)
        }
        fn list_dir(&self, path: &str, privilege: Privilege) -> Result<Vec<String>> {
            self.inner.list_dir(path, privilege)
        }
        fn create_dir_all(&self, path: &str, privilege: Privilege) -> Result<()> {
            self.inner.create_dir_all(path, privilege)
        }
    }

    #[test]
    fn a_failed_rollback_is_reported_alongside_the_original_error() {
        let host = RollbackFailsHost {
            inner: FakeHost::new()
                .file("/etc/cron.d/a", "old\n")
                .script("boom", 1, "", "no\n"),
            ran: std::sync::atomic::AtomicBool::new(false),
        };
        let err = execute(
            &host,
            vec![
                write("/etc/cron.d/a", Some("old\n"), "new\n"),
                command("boom", FailPolicy::Error),
            ],
            ApplyMode::Apply,
        )
        .expect_err("the command failed");
        // The file really is left in the state the plan wrote.
        assert_eq!(
            host.inner.file_contents("/etc/cron.d/a"),
            Some("new\n".to_string())
        );
        match err {
            Error::Other(message) => {
                assert!(message.contains("rollback incomplete"), "{message}");
                assert!(message.contains("/etc/cron.d/a"), "{message}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn op_reports_its_source_verb_and_id() {
        let create = Op::Create(spec());
        assert_eq!(create.source(), TaskSourceKind::Cron);
        assert_eq!(create.verb(), "add");
        assert_eq!(create.id(), None);

        let update = Op::Update {
            id: "user:alice:4".to_string(),
            source: TaskSourceKind::Cron,
            spec: spec(),
        };
        assert_eq!(update.verb(), "edit");
        assert_eq!(update.id(), Some("user:alice:4"));

        let delete = Op::Delete {
            id: "12".to_string(),
            source: TaskSourceKind::At,
        };
        assert_eq!(delete.source(), TaskSourceKind::At);
        assert_eq!(delete.verb(), "rm");

        let enable = Op::SetEnabled {
            id: "user/x.timer".to_string(),
            source: TaskSourceKind::Systemd,
            enabled: true,
        };
        assert_eq!(enable.verb(), "enable");
        let disable = Op::SetEnabled {
            id: "user/x.timer".to_string(),
            source: TaskSourceKind::Systemd,
            enabled: false,
        };
        assert_eq!(disable.verb(), "disable");
        assert_eq!(disable.id(), Some("user/x.timer"));
    }
}
