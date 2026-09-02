//! Everything the CLI does once the arguments are parsed: opening a
//! session against a machine, turning an id into a task, rendering a
//! plan, backing up what a mutation is about to overwrite, and shaping
//! the report and the exit code.
//!
//! The session owns exactly one [`Host`]. Over SSH that host and the
//! collector are the same object behind one `Arc`, so a whole
//! `add`/`list` round trip shares a single multiplex master.

use std::collections::HashMap;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use shuvjobs_adapters::ids::{parse_cron_id, parse_systemd_id, CronId};
use shuvjobs_adapters::systemd::Scope;
use shuvjobs_adapters::write::writers_for;
use shuvjobs_adapters::{
    AnacronAdapter, AtAdapter, CronAdapter, LaunchdAdapter, LocalHost, SystemdAdapter,
};
use shuvjobs_core::host::{privileged_command, Host, PrivilegePolicy};
use shuvjobs_core::manage::{self, ApplyMode, JobScope, JobSpec, JobWriter, MutationOutcome};
use shuvjobs_core::{
    Change, Error, Op, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind, TaskStatus,
};
use similar::TextDiff;

use crate::cli::{EditArgs, Global};
use crate::remote::RemoteCollector;

/// The failures the CLI itself produces, as opposed to the ones a host
/// or a writer produces. Kept separate so [`exit_code`] and the JSON
/// error report can name them without string matching.
#[derive(Debug)]
pub enum CliError {
    /// The invocation was wrong in a way clap could not catch.
    Usage(String),
    /// No job carries that id.
    NotFound(String),
    /// More than one source claims that id.
    Ambiguous(String),
    /// The operator answered the confirmation prompt with no.
    Aborted,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(msg) | CliError::NotFound(msg) | CliError::Ambiguous(msg) => {
                f.write_str(msg)
            }
            CliError::Aborted => f.write_str("aborted"),
        }
    }
}

impl std::error::Error for CliError {}

/// One machine, its writers, and the flags that change what a mutation
/// is allowed to do to it.
pub struct Session {
    pub host: Arc<dyn Host>,
    /// `Some` for `--host`: the same object as `host`, kept typed so
    /// collection can use the SSH read path.
    pub remote: Option<Arc<RemoteCollector>>,
    pub writers: HashMap<TaskSourceKind, Box<dyn JobWriter>>,
    pub dry_run: bool,
    /// The `--host` argument, or `None` for this machine.
    pub label: Option<String>,
}

impl Session {
    pub fn open(global: &Global) -> Result<Session> {
        let (host, remote): (Arc<dyn Host>, Option<Arc<RemoteCollector>>) = match &global.host {
            Some(target) => {
                // One collector, one Arc: the SSH master lives as long
                // as the session and is shared by reads and writes.
                let collector = Arc::new(
                    RemoteCollector::new(target.clone(), global.port, global.key.clone())
                        .with_sudo(global.sudo),
                );
                (Arc::clone(&collector) as Arc<dyn Host>, Some(collector))
            }
            None => (Arc::new(LocalHost::new(global.sudo)) as Arc<dyn Host>, None),
        };
        Ok(Session {
            writers: writers_for(Arc::clone(&host)),
            host,
            remote,
            dry_run: global.dry_run,
            label: global.host.clone(),
        })
    }

    /// `local`, or the `user@host` the operator named.
    pub fn host_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| "local".to_string())
    }

    pub fn collect(&self) -> Result<Vec<ScheduledTask>> {
        match &self.remote {
            Some(remote) => remote
                .collect()
                .with_context(|| format!("collecting from {}", remote.label())),
            None => Ok(collect_local()),
        }
    }

    /// The one task carrying `id`, or an error that says what to do next.
    pub fn resolve(&self, id: &str, source: Option<TaskSourceKind>) -> Result<ScheduledTask> {
        let tasks = self.collect()?;
        let matches: Vec<ScheduledTask> = tasks
            .into_iter()
            .filter(|task| task.id == id && source.is_none_or(|kind| task.source == kind))
            .collect();
        match matches.len() {
            0 => Err(CliError::NotFound(format!(
                "no job with id `{id}` on {}: run `shuvjobs list` to see the ids",
                self.host_label()
            ))
            .into()),
            1 => Ok(matches.into_iter().next().expect("one match")),
            _ => {
                let sources: Vec<&str> = matches.iter().map(|t| t.source.as_str()).collect();
                Err(CliError::Ambiguous(format!(
                    "id `{id}` exists under {}: pass --source",
                    sources.join(", ")
                ))
                .into())
            }
        }
    }

    fn writer(&self, kind: TaskSourceKind, id: &str) -> Result<&dyn JobWriter> {
        self.writers
            .get(&kind)
            .map(|writer| writer.as_ref())
            .ok_or_else(|| {
                Error::Unsupported {
                    source_kind: kind,
                    id: id.to_string(),
                    reason: "this build has no writer for that source".to_string(),
                }
                .into()
            })
    }

    fn dispatch(&self, op: &Op, mode: ApplyMode) -> Result<MutationOutcome> {
        let writer = self.writer(op.source(), op.id().unwrap_or("<new>"))?;
        let outcome = match op {
            Op::Create(spec) => writer.create(spec, mode),
            Op::Update { id, spec, .. } => writer.update(id, spec, mode),
            Op::Delete { id, .. } => writer.delete(id, mode),
            Op::SetEnabled { id, enabled, .. } => writer.set_enabled(id, *enabled, mode),
        }?;
        Ok(outcome)
    }

    /// What the mutation would do. Makes no writes at all, so it is safe
    /// against a machine the operator cannot write to.
    pub fn plan(&self, op: &Op) -> Result<MutationOutcome> {
        self.dispatch(op, ApplyMode::DryRun)
    }

    /// Carry the mutation out, after copying every file it is about to
    /// overwrite or remove into the operator's own state directory.
    ///
    /// `backups` is filled in before anything is written, so the caller
    /// can point at the copies even when the apply fails.
    pub fn apply(&self, op: &Op, backups: &mut HashMap<String, String>) -> Result<MutationOutcome> {
        let planned = self.dispatch(op, ApplyMode::DryRun)?;
        for (path, before) in planned.backup_targets() {
            let saved = write_backup(&self.host_label(), path, before)?;
            backups.insert(path.to_string(), saved);
        }
        self.dispatch(op, ApplyMode::Apply)
    }

    /// How this session may satisfy [`Privilege::Root`]: what the sudo
    /// wrapper in a rendered plan is decided by.
    pub fn policy(&self) -> PrivilegePolicy {
        self.host.policy()
    }

    pub fn render_plan(&self, outcome: &MutationOutcome) -> String {
        render_plan(outcome, self.host.policy())
    }
}

/// Collect from every scheduler on this machine, skipping the ones that
/// are not installed and warning about the ones that broke.
pub fn collect_local() -> Vec<ScheduledTask> {
    let sources: Vec<Box<dyn TaskSource>> = vec![
        Box::new(SystemdAdapter::new()),
        Box::new(CronAdapter::new()),
        Box::new(AtAdapter::new()),
        Box::new(AnacronAdapter::new()),
        Box::new(LaunchdAdapter::new()),
    ];

    let mut tasks: Vec<ScheduledTask> = Vec::new();
    for source in &sources {
        match source.collect() {
            Ok(mut found) => tasks.append(&mut found),
            Err(Error::Unavailable(_)) => continue,
            Err(e) => eprintln!("warning: {} adapter failed: {e}", source.kind().as_str()),
        }
    }
    tasks
}

/// The operator's own state directory: backups live on the machine
/// running shuvjobs, never on the machine being changed.
pub fn backup_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Where a backup of `path` goes, under `root`. Never next to the
/// target: a stray `/etc/cron.d/x.bak` would itself be read by cron.
pub fn backup_path(root: &Path, host_label: &str, path: &str, stamp: u64) -> PathBuf {
    root.join("shuvjobs/backups")
        .join(sanitize_component(host_label))
        .join(format!("{}.{stamp}", sanitize_component(path)))
}

/// Flatten a path or a `user@host` into one safe file name component.
fn sanitize_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

fn write_backup(host_label: &str, path: &str, contents: &str) -> Result<String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = backup_path(&backup_root(), host_label, path, stamp);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating backup directory {}", parent.display()))?;
    }
    std::fs::write(&target, contents)
        .with_context(|| format!("writing backup {}", target.display()))?;
    Ok(target.display().to_string())
}

/// A unified diff for one planned file change, with `/dev/null` on the
/// side where the file does not exist.
pub fn file_diff(change: &Change) -> Option<(String, String)> {
    let (path, before, after) = match change {
        Change::WriteFile {
            path,
            before,
            after,
            ..
        } => (path, before.as_deref().unwrap_or(""), after.as_str()),
        Change::RemoveFile { path, before, .. } => (path, before.as_deref().unwrap_or(""), ""),
        Change::Command { .. } => return None,
    };
    let from = if before.is_empty() { "/dev/null" } else { path };
    let to = if after.is_empty() { "/dev/null" } else { path };
    let diff = TextDiff::from_lines(before, after)
        .unified_diff()
        .header(from, to)
        .to_string();
    Some((path.clone(), diff))
}

/// Every command in the plan, rendered exactly as it will be run —
/// including the sudo wrapper this session's policy would add.
pub fn plan_commands(outcome: &MutationOutcome, policy: PrivilegePolicy) -> Vec<String> {
    outcome
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::Command {
                cmd,
                privilege,
                description,
                ..
            } => Some(
                privileged_command(cmd, *privilege, policy, description)
                    .map(|rendered| rendered.into_owned())
                    .unwrap_or_else(|_| cmd.clone()),
            ),
            _ => None,
        })
        .collect()
}

/// The `--dry-run` output: a diff per file, then the commands, then any
/// notes the writer attached.
pub fn render_plan(outcome: &MutationOutcome, policy: PrivilegePolicy) -> String {
    let mut out = String::new();
    for change in &outcome.changes {
        if let Some((_, diff)) = file_diff(change) {
            out.push_str(&diff);
            if !diff.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    let commands = plan_commands(outcome, policy);
    if !commands.is_empty() {
        out.push_str("commands:\n");
        for cmd in &commands {
            out.push_str("  ");
            out.push_str(cmd);
            out.push('\n');
        }
    }
    if !outcome.notes.is_empty() {
        out.push_str("notes:\n");
        for note in &outcome.notes {
            out.push_str("  ");
            out.push_str(note);
            out.push('\n');
        }
    }
    if out.is_empty() {
        out.push_str("no changes\n");
    }
    out
}

/// Fold the operator's `--schedule`/`--command`/`--name`/`--user` into
/// the job as it is today. Everything unspecified keeps its current
/// value, and scope comes from the id, which is the only thing that
/// actually knows whether a job is the user's or the machine's.
pub fn merge_edit(existing: &ScheduledTask, args: &EditArgs) -> Result<JobSpec> {
    let source = existing.source;
    let schedule = match args.schedule.as_deref() {
        Some(text) => manage::parse_schedule(source, text)?,
        None => existing.schedule.clone(),
    };
    let command = args
        .command
        .clone()
        .unwrap_or_else(|| existing.command.clone());
    let (scope, user) = scope_and_user(existing);
    let mut spec = JobSpec::new(source, schedule, command);
    spec.name = args.name.clone().or_else(|| derive_name(existing));
    spec.user = args.user.clone().or(user);
    spec.scope = scope;
    spec.enabled = existing.enabled.unwrap_or(true);
    spec.location = existing.location.clone();
    spec.validate()?;
    Ok(spec)
}

/// Where the job lives, read back off the id (and, for launchd, the
/// plist path, which is the only place the domain shows up).
fn scope_and_user(task: &ScheduledTask) -> (JobScope, Option<String>) {
    match task.source {
        TaskSourceKind::Systemd => match parse_systemd_id(&task.id) {
            Ok(id) if id.scope == Scope::User => (JobScope::User, None),
            _ => (JobScope::System, None),
        },
        TaskSourceKind::Cron => match parse_cron_id(&task.id) {
            Ok(CronId::UserCrontab { user, .. }) => (JobScope::User, Some(user)),
            _ => (JobScope::System, None),
        },
        TaskSourceKind::Launchd => {
            let system = task
                .location
                .as_deref()
                .is_some_and(|path| path.contains("/Library/LaunchDaemons"));
            if system {
                (JobScope::System, None)
            } else {
                (JobScope::User, None)
            }
        }
        // The anacrontab is always the machine's; `at` jobs are always
        // the queueing user's own.
        TaskSourceKind::Anacron => (JobScope::System, None),
        TaskSourceKind::At => (JobScope::User, None),
    }
}

/// The name a writer would use for this job, recovered from its id.
fn derive_name(task: &ScheduledTask) -> Option<String> {
    match task.source {
        TaskSourceKind::Systemd => parse_systemd_id(&task.id)
            .ok()
            .map(|id| id.unit.trim_end_matches(".timer").to_string()),
        TaskSourceKind::Anacron => Some(
            task.id
                .strip_prefix("anacron:")
                .unwrap_or(&task.id)
                .to_string(),
        ),
        TaskSourceKind::Launchd => Some(task.id.clone()),
        TaskSourceKind::Cron => match parse_cron_id(&task.id) {
            Ok(CronId::SystemFile { path, .. }) | Ok(CronId::RunParts { path }) => path
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            _ => None,
        },
        TaskSourceKind::At => None,
    }
}

/// Ask before deleting. A pipeline that did not pass `--yes` is a
/// mistake, not a silent yes.
pub fn confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Err(CliError::Usage(
            "refusing to delete without --yes: stdin is not a terminal".to_string(),
        )
        .into());
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    stdin.read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// 0 ok, 2 usage, 3 needs root, 4 conflict, 5 unsupported, 1 everything else.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    if let Some(cli) = err.downcast_ref::<CliError>() {
        return match cli {
            CliError::Usage(_) => 2,
            CliError::NotFound(_) | CliError::Ambiguous(_) | CliError::Aborted => 1,
        };
    }
    match err.downcast_ref::<Error>() {
        Some(Error::Validation(_)) => 2,
        Some(Error::NeedsRoot { .. }) => 3,
        Some(Error::Conflict(_)) => 4,
        Some(Error::Unsupported { .. }) => 5,
        _ => 1,
    }
}

/// The `kind` field of the JSON error report.
pub fn error_kind(err: &anyhow::Error) -> &'static str {
    if let Some(cli) = err.downcast_ref::<CliError>() {
        return match cli {
            CliError::Usage(_) => "validation",
            CliError::NotFound(_) => "not_found",
            CliError::Ambiguous(_) => "ambiguous",
            CliError::Aborted => "aborted",
        };
    }
    match err.downcast_ref::<Error>() {
        Some(Error::Validation(_)) => "validation",
        Some(Error::NeedsRoot { .. }) => "needs_root",
        Some(Error::Conflict(_)) => "conflict",
        Some(Error::Unsupported { .. }) => "unsupported",
        _ => "other",
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct FileReport {
    pub path: String,
    /// Where the previous contents were saved, when there were any.
    pub backup: Option<String>,
    pub diff: String,
}

/// The `--json` result of one mutation, dry run or not.
#[derive(Debug, Serialize, PartialEq)]
pub struct Report {
    pub ok: bool,
    pub op: &'static str,
    pub source: TaskSourceKind,
    pub id: Option<String>,
    pub host: String,
    pub dry_run: bool,
    pub files: Vec<FileReport>,
    pub commands: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(
        op: &Op,
        outcome: &MutationOutcome,
        host: String,
        dry_run: bool,
        policy: PrivilegePolicy,
        backups: &HashMap<String, String>,
    ) -> Report {
        let files = outcome
            .changes
            .iter()
            .filter_map(file_diff)
            .map(|(path, diff)| FileReport {
                backup: backups.get(&path).cloned(),
                path,
                diff,
            })
            .collect();
        Report {
            ok: true,
            op: op.verb(),
            source: op.source(),
            id: outcome.id.clone().or_else(|| op.id().map(str::to_string)),
            host,
            dry_run,
            files,
            commands: plan_commands(outcome, policy),
            notes: outcome.notes.clone(),
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ErrorDetail {
    pub kind: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ErrorReport {
    pub ok: bool,
    pub op: Option<&'static str>,
    pub id: Option<String>,
    pub error: ErrorDetail,
}

impl ErrorReport {
    pub fn new(err: &anyhow::Error, op: Option<&'static str>, id: Option<String>) -> ErrorReport {
        ErrorReport {
            ok: false,
            op,
            id,
            error: ErrorDetail {
                kind: error_kind(err),
                message: format!("{err:#}"),
            },
        }
    }
}

/// The source's own schedule text, as the read path would show it.
pub fn schedule_text(schedule: &ScheduleType) -> String {
    match schedule {
        ScheduleType::Cron(expr) | ScheduleType::Calendar(expr) => expr.clone(),
        ScheduleType::Interval(d) => format_duration(d.as_secs()),
        ScheduleType::OneShot(dt) => local_time(*dt),
    }
}

fn format_duration(secs: u64) -> String {
    match secs {
        0 => "0s".to_string(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn local_time(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn enabled_text(enabled: Option<bool>) -> &'static str {
    match enabled {
        Some(true) => "yes",
        Some(false) => "no",
        None => "-",
    }
}

/// `list` without `--json`: one aligned row per job.
pub fn print_table(tasks: &[ScheduledTask]) {
    const HEADERS: [&str; 6] = ["SOURCE", "NAME", "SCHEDULE", "ENABLED", "NEXT", "ID"];
    let mut rows: Vec<[String; 6]> = vec![HEADERS.map(str::to_string)];
    for task in tasks {
        rows.push([
            task.source.as_str().to_string(),
            task.name.clone(),
            schedule_text(&task.schedule),
            enabled_text(task.enabled).to_string(),
            task.next_run.map(local_time).unwrap_or_else(|| "-".into()),
            task.id.clone(),
        ]);
    }
    let mut widths = [0usize; 6];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    for row in &rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                let pad = widths[i] - cell.chars().count() + 2;
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        println!("{}", line.trim_end());
    }
}

/// `show` without `--json`: one key per line, in reading order.
pub fn print_task(task: &ScheduledTask) {
    let status = match &task.last_status {
        Some(TaskStatus::Success) => "success".to_string(),
        Some(TaskStatus::Failed(msg)) if msg.is_empty() => "failed".to_string(),
        Some(TaskStatus::Failed(msg)) => format!("failed: {msg}"),
        Some(TaskStatus::Running) => "running".to_string(),
        None => "-".to_string(),
    };
    let fields: [(&str, String); 11] = [
        ("Id", task.id.clone()),
        ("Name", task.name.clone()),
        ("Source", task.source.as_str().to_string()),
        ("Enabled", enabled_text(task.enabled).to_string()),
        (
            "Location",
            task.location.clone().unwrap_or_else(|| "-".into()),
        ),
        ("Schedule", schedule_text(&task.schedule)),
        ("Command", task.command.clone()),
        (
            "Last run",
            task.last_run.map(local_time).unwrap_or_else(|| "-".into()),
        ),
        (
            "Next run",
            task.next_run.map(local_time).unwrap_or_else(|| "-".into()),
        ),
        ("Status", status),
        (
            "Duration",
            task.last_duration
                .map(|d| format!("{:.3}s", d.as_secs_f64()))
                .unwrap_or_else(|| "-".into()),
        ),
    ];
    let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in fields {
        println!("{key:<width$}  {value}", width = width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuvjobs_core::host::Privilege;
    use shuvjobs_core::manage::FailPolicy;
    use std::time::Duration;

    fn task(id: &str, source: TaskSourceKind) -> ScheduledTask {
        ScheduledTask {
            id: id.into(),
            name: "job".into(),
            source,
            schedule: ScheduleType::Cron("0 9 * * *".into()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "echo hi".into(),
            location: None,
            enabled: Some(true),
        }
    }

    fn edit_args(id: &str) -> EditArgs {
        use crate::cli::IdArgs;
        EditArgs {
            target: IdArgs {
                id: id.to_string(),
                source: None,
            },
            schedule: None,
            command: None,
            name: None,
            user: None,
        }
    }

    fn fixture_outcome() -> MutationOutcome {
        MutationOutcome {
            id: Some("user:alice:4".into()),
            changes: vec![
                Change::WriteFile {
                    path: "/etc/cron.d/new".into(),
                    before: None,
                    after: "# managed by shuvjobs\n0 9 * * * root echo hi\n".into(),
                    mode: 0o644,
                    privilege: Privilege::Root,
                },
                Change::WriteFile {
                    path: "/etc/crontab".into(),
                    before: Some("0 1 * * * root old\n".into()),
                    after: "0 2 * * * root new\n".into(),
                    mode: 0o644,
                    privilege: Privilege::Root,
                },
                Change::RemoveFile {
                    path: "/etc/cron.d/gone".into(),
                    before: Some("0 3 * * * root gone\n".into()),
                    privilege: Privilege::Root,
                },
                Change::Command {
                    cmd: "systemctl daemon-reload".into(),
                    stdin: None,
                    privilege: Privilege::Root,
                    description: "reload the systemd manager".into(),
                    on_fail: FailPolicy::Error,
                },
            ],
            applied: false,
            outputs: Vec::new(),
            notes: vec!["line moved".into()],
        }
    }

    #[test]
    fn render_plan_shows_a_diff_per_file_then_commands_then_notes() {
        let policy = PrivilegePolicy {
            is_root: false,
            sudo: true,
        };
        let text = render_plan(&fixture_outcome(), policy);
        assert!(text.contains("--- /dev/null\n+++ /etc/cron.d/new"));
        assert!(text.contains("+0 9 * * * root echo hi"));
        assert!(text.contains("--- /etc/crontab\n+++ /etc/crontab"));
        assert!(text.contains("-0 1 * * * root old"));
        assert!(text.contains("--- /etc/cron.d/gone\n+++ /dev/null"));
        assert!(text.contains("commands:\n  sudo -n -- systemctl daemon-reload\n"));
        assert!(text.contains("notes:\n  line moved\n"));
    }

    #[test]
    fn render_plan_leaves_commands_bare_when_already_root() {
        let policy = PrivilegePolicy {
            is_root: true,
            sudo: false,
        };
        let text = render_plan(&fixture_outcome(), policy);
        assert!(text.contains("commands:\n  systemctl daemon-reload\n"));
    }

    #[test]
    fn render_plan_says_so_when_nothing_changes() {
        let text = render_plan(&MutationOutcome::default(), PrivilegePolicy::default());
        assert_eq!(text, "no changes\n");
    }

    #[test]
    fn exit_codes_follow_the_error() {
        let cases: Vec<(anyhow::Error, i32)> = vec![
            (CliError::Usage("bad".into()).into(), 2),
            (CliError::NotFound("gone".into()).into(), 1),
            (CliError::Ambiguous("two".into()).into(), 1),
            (CliError::Aborted.into(), 1),
            (Error::Validation("nope".into()).into(), 2),
            (
                Error::NeedsRoot {
                    operation: "write".into(),
                }
                .into(),
                3,
            ),
            (Error::Conflict("moved".into()).into(), 4),
            (
                Error::Unsupported {
                    source_kind: TaskSourceKind::At,
                    id: "at:1".into(),
                    reason: "no".into(),
                }
                .into(),
                5,
            ),
            (Error::Other("boom".into()).into(), 1),
        ];
        for (err, code) in cases {
            assert_eq!(exit_code(&err), code, "{err}");
        }
    }

    #[test]
    fn exit_code_sees_through_context() {
        let err = anyhow::Error::from(Error::Conflict("moved".into())).context("editing job");
        assert_eq!(exit_code(&err), 4);
        assert_eq!(error_kind(&err), "conflict");
    }

    #[test]
    fn error_kinds_cover_every_variant() {
        assert_eq!(error_kind(&CliError::Aborted.into()), "aborted");
        assert_eq!(
            error_kind(&CliError::NotFound("x".into()).into()),
            "not_found"
        );
        assert_eq!(
            error_kind(&CliError::Ambiguous("x".into()).into()),
            "ambiguous"
        );
        assert_eq!(
            error_kind(&CliError::Usage("x".into()).into()),
            "validation"
        );
        assert_eq!(
            error_kind(
                &Error::NeedsRoot {
                    operation: "x".into()
                }
                .into()
            ),
            "needs_root"
        );
        assert_eq!(error_kind(&Error::Other("x".into()).into()), "other");
    }

    #[test]
    fn merge_edit_keeps_every_unspecified_field() {
        let mut existing = task("user:alice:4", TaskSourceKind::Cron);
        existing.command = "echo original".into();
        let mut args = edit_args("user:alice:4");
        args.schedule = Some("*/5 * * * *".into());

        let spec = merge_edit(&existing, &args).unwrap();
        assert_eq!(spec.command, "echo original");
        assert_eq!(spec.schedule, ScheduleType::Cron("*/5 * * * *".into()));
        assert_eq!(spec.scope, JobScope::User);
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert!(spec.enabled);
    }

    #[test]
    fn merge_edit_derives_system_scope_for_a_file_backed_cron_job() {
        let mut existing = task("/etc/cron.d/backup:3", TaskSourceKind::Cron);
        existing.location = Some("/etc/cron.d/backup".into());
        let mut args = edit_args("/etc/cron.d/backup:3");
        args.command = Some("echo new".into());

        let spec = merge_edit(&existing, &args).unwrap();
        assert_eq!(spec.scope, JobScope::System);
        assert_eq!(spec.user, None);
        assert_eq!(spec.name.as_deref(), Some("backup"));
        assert_eq!(spec.location.as_deref(), Some("/etc/cron.d/backup"));
        assert_eq!(spec.command, "echo new");
    }

    #[test]
    fn merge_edit_derives_user_scope_for_a_user_timer() {
        let mut existing = task("user/radar-daily.timer", TaskSourceKind::Systemd);
        existing.schedule = ScheduleType::Calendar("*-*-* 03:00:00".into());
        existing.enabled = Some(false);
        let mut args = edit_args("user/radar-daily.timer");
        args.schedule = Some("hourly".into());

        let spec = merge_edit(&existing, &args).unwrap();
        assert_eq!(spec.scope, JobScope::User);
        assert_eq!(spec.name.as_deref(), Some("radar-daily"));
        assert!(!spec.enabled);
    }

    #[test]
    fn merge_edit_derives_system_scope_for_a_system_timer() {
        let existing = task("logrotate.timer", TaskSourceKind::Systemd);
        let mut args = edit_args("logrotate.timer");
        args.command = Some("/usr/bin/true".into());
        let spec = merge_edit(&existing, &args).unwrap();
        assert_eq!(spec.scope, JobScope::System);
    }

    #[test]
    fn merge_edit_rejects_a_schedule_the_source_cannot_express() {
        let existing = task("anacron:backup", TaskSourceKind::Anacron);
        let mut args = edit_args("anacron:backup");
        args.schedule = Some("@reboot".into());
        assert!(merge_edit(&existing, &args).is_err());
    }

    #[test]
    fn merge_edit_takes_the_new_user_over_the_old_one() {
        let existing = task("user:alice:4", TaskSourceKind::Cron);
        let mut args = edit_args("user:alice:4");
        args.user = Some("bob".into());
        let spec = merge_edit(&existing, &args).unwrap();
        assert_eq!(spec.user.as_deref(), Some("bob"));
    }

    #[test]
    fn report_json_has_the_documented_shape() {
        let op = Op::Delete {
            id: "user:alice:4".into(),
            source: TaskSourceKind::Cron,
        };
        let mut backups = HashMap::new();
        backups.insert(
            "/etc/crontab".to_string(),
            "/state/etc_crontab.7".to_string(),
        );
        let report = Report::new(
            &op,
            &fixture_outcome(),
            "local".into(),
            true,
            PrivilegePolicy {
                is_root: false,
                sudo: true,
            },
            &backups,
        );
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(value["op"], serde_json::json!("rm"));
        assert_eq!(value["source"], serde_json::json!("cron"));
        assert_eq!(value["id"], serde_json::json!("user:alice:4"));
        assert_eq!(value["host"], serde_json::json!("local"));
        assert_eq!(value["dry_run"], serde_json::json!(true));
        assert_eq!(value["files"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["files"][0]["path"],
            serde_json::json!("/etc/cron.d/new")
        );
        assert_eq!(value["files"][0]["backup"], serde_json::Value::Null);
        assert_eq!(
            value["files"][1]["backup"],
            serde_json::json!("/state/etc_crontab.7")
        );
        assert!(value["files"][1]["diff"]
            .as_str()
            .unwrap()
            .contains("--- /etc/crontab"));
        assert_eq!(
            value["commands"],
            serde_json::json!(["sudo -n -- systemctl daemon-reload"])
        );
        assert_eq!(value["notes"], serde_json::json!(["line moved"]));
    }

    #[test]
    fn error_report_json_has_the_documented_shape() {
        let err: anyhow::Error = Error::Conflict("changed since it was read".into()).into();
        let report = ErrorReport::new(&err, Some("edit"), Some("user:alice:4".into()));
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["ok"], serde_json::json!(false));
        assert_eq!(value["op"], serde_json::json!("edit"));
        assert_eq!(value["id"], serde_json::json!("user:alice:4"));
        assert_eq!(value["error"]["kind"], serde_json::json!("conflict"));
        assert_eq!(
            value["error"]["message"],
            serde_json::json!("conflict: changed since it was read")
        );
    }

    #[test]
    fn backup_paths_are_sanitised_and_stamped() {
        let root = Path::new("/state");
        assert_eq!(
            backup_path(
                root,
                "alice@host.example",
                "/etc/cron.d/my job",
                1_712_345_678
            )
            .display()
            .to_string(),
            "/state/shuvjobs/backups/alice_host.example/etc_cron.d_my_job.1712345678"
        );
        assert_eq!(
            backup_path(root, "local", "/etc/crontab", 7)
                .display()
                .to_string(),
            "/state/shuvjobs/backups/local/etc_crontab.7"
        );
        // A label with nothing usable in it still yields one component.
        assert_eq!(
            backup_path(root, "//", "/", 1).display().to_string(),
            "/state/shuvjobs/backups/_/_.1"
        );
    }

    #[test]
    fn schedule_text_renders_each_variant() {
        assert_eq!(
            schedule_text(&ScheduleType::Cron("0 9 * * *".into())),
            "0 9 * * *"
        );
        assert_eq!(
            schedule_text(&ScheduleType::Calendar("daily".into())),
            "daily"
        );
        assert_eq!(
            schedule_text(&ScheduleType::Interval(Duration::from_secs(3600))),
            "1h"
        );
        assert_eq!(
            schedule_text(&ScheduleType::Interval(Duration::from_secs(90))),
            "90s"
        );
        assert_eq!(
            schedule_text(&ScheduleType::Interval(Duration::from_secs(86_400))),
            "1d"
        );
    }
}
