//! The anacron writer.
//!
//! anacron has one file, `/etc/anacrontab`, one line per job, and no
//! tooling: there is no `anacrontab` binary the way cron has `crontab`,
//! so every mutation is an in-place edit of a root-owned file through
//! [`LineFile`]. The job identifier in the third column is the whole
//! addressing scheme — it names the line *and* the timestamp file under
//! `/var/spool/anacron` — so it has to stay unique, and renaming a job
//! means the old spool file is orphaned and has to go with it.

use std::sync::Arc;

use shuvjobs_core::host::{Host, Privilege};
use shuvjobs_core::manage::{execute, ApplyMode, Change, JobSpec, JobWriter, MutationOutcome};
use shuvjobs_core::{Error, Result, ScheduleType, TaskSourceKind};

use crate::anacron::ANACRONTAB_PATH;
use crate::ids::parse_anacron_id;
use crate::lineedit::{strip_disabled_marker, LineFile};
use crate::write::{validate_command, validate_simple_name, NameKind, DISABLED_MARKER};

/// Where anacron records the date it last ran each job.
pub const SPOOL_DIR: &str = "/var/spool/anacron";

/// The spec option carrying the second column: how many minutes after
/// anacron starts this job waits, so a boot does not run everything at
/// once.
pub const DELAY_OPTION: &str = "anacron.delay";

/// What Debian's own anacrontab uses for the first job of the day.
const DEFAULT_DELAY: u32 = 5;

/// The period shorthands cronie's anacron understands. `@hourly` and
/// `@reboot` are missing on purpose: anacron only counts whole days.
const ANACRON_PERIODS: &[&str] = &["@daily", "@weekly", "@monthly", "@yearly", "@annually"];

fn unsupported(id: &str, reason: &str) -> Error {
    Error::Unsupported {
        source_kind: TaskSourceKind::Anacron,
        id: id.to_string(),
        reason: reason.to_string(),
    }
}

fn spool_path(job: &str) -> String {
    format!("{SPOOL_DIR}/{job}")
}

/// One `/etc/anacrontab` job line: `period delay job-id command…`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    delay: u32,
    job: String,
}

/// The job half of a line, if the line is a job at all. Comments,
/// blanks, `SHELL=/bin/sh`, and truncated lines are not.
fn entry_of(line: &str) -> Option<Entry> {
    let trimmed = line.trim();
    let payload = strip_disabled_marker(trimmed).unwrap_or(trimmed).trim();
    if payload.is_empty() || payload.starts_with('#') {
        return None;
    }
    let mut fields = payload.split_whitespace();
    let period = fields.next()?;
    if period.contains('=') {
        return None;
    }
    let delay = fields.next()?.parse::<u32>().ok()?;
    let job = fields.next()?.to_string();
    // A job with no command is not a job.
    fields.next()?;
    Some(Entry { delay, job })
}

/// The first column: whole days, or one of anacron's shorthands.
fn render_period(schedule: &ScheduleType) -> Result<String> {
    match schedule {
        ScheduleType::Interval(interval) => {
            let secs = interval.as_secs();
            if secs == 0 || !secs.is_multiple_of(86_400) {
                return Err(Error::Validation(format!(
                    "anacron measures periods in whole days: {secs}s is not one"
                )));
            }
            Ok((secs / 86_400).to_string())
        }
        ScheduleType::Calendar(text) => {
            let alias = text.trim().to_ascii_lowercase();
            if !ANACRON_PERIODS.contains(&alias.as_str()) {
                return Err(Error::Validation(format!(
                    "anacron cannot express `{text}`: give a number of days or one of {}",
                    ANACRON_PERIODS.join(", ")
                )));
            }
            Ok(alias)
        }
        ScheduleType::Cron(expr) => Err(Error::Validation(format!(
            "anacron has no cron fields: `{expr}` must be a number of days or one of {}",
            ANACRON_PERIODS.join(", ")
        ))),
        ScheduleType::OneShot(_) => Err(Error::Validation(
            "anacron only runs recurring jobs; use `at` for a one-shot".into(),
        )),
    }
}

/// One anacrontab line, ready to append or replace.
pub fn render_line(
    schedule: &ScheduleType,
    delay: u32,
    job: &str,
    command: &str,
) -> Result<String> {
    validate_simple_name(job, NameKind::Anacron)?;
    validate_command(command)?;
    let period = render_period(schedule)?;
    Ok(format!("{period} {delay} {job} {}", command.trim()))
}

/// The delay column the caller asked for, if any.
fn delay_option(spec: &JobSpec) -> Result<Option<u32>> {
    let Some(text) = spec.options.get(DELAY_OPTION) else {
        return Ok(None);
    };
    text.trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|_| Error::Validation(format!("{DELAY_OPTION} must be whole minutes: `{text}`")))
}

/// anacron runs everything as root; there is no user column to put
/// anybody else in.
fn check_user(spec: &JobSpec) -> Result<()> {
    match spec.user.as_deref() {
        None | Some("root") => Ok(()),
        Some(user) => Err(unsupported(
            spec.name.as_deref().unwrap_or_default(),
            &format!(
                "anacron runs every job as root and has no user column, so it cannot run this \
                 as `{user}`; wrap the command in `su` or use cron"
            ),
        )),
    }
}

fn decorate(body: &str, enabled: bool) -> String {
    if enabled {
        body.to_string()
    } else {
        format!("{DISABLED_MARKER}{body}")
    }
}

/// Writes `/etc/anacrontab` on one [`Host`]. Every operation is root's.
pub struct AnacronWriter {
    host: Arc<dyn Host>,
}

impl AnacronWriter {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }

    fn read_anacrontab(&self) -> Result<String> {
        self.host
            .read_to_string(ANACRONTAB_PATH, Privilege::Root)?
            .ok_or_else(|| {
                Error::Unavailable(format!(
                    "{ANACRONTAB_PATH} is not there: anacron does not look installed"
                ))
            })
    }

    fn write_change(path: &str, before: String, after: String) -> Change {
        Change::WriteFile {
            path: path.to_string(),
            before: Some(before),
            after,
            mode: 0o644,
            privilege: Privilege::Root,
        }
    }

    /// anacron only ever forgets a job's history when we tell it to, so
    /// the stale timestamp file is removed alongside the line. It is
    /// routinely absent (a job that never ran), which `remove_file`
    /// reports as `false` rather than failing.
    fn drop_spool(job: &str) -> Change {
        Change::RemoveFile {
            path: spool_path(job),
            before: None,
            privilege: Privilege::Root,
        }
    }

    /// The one line naming `job`. Zero or several is drift, not a target.
    fn locate(file: &LineFile, job: &str) -> Result<usize> {
        match file.find_unique(|line| entry_of(line).is_some_and(|entry| entry.job == job)) {
            Ok(Some(line)) => Ok(line),
            Ok(None) => Err(Error::Conflict(format!(
                "no anacron job `{job}` in {ANACRONTAB_PATH} any more; \
                 it changed since it was read, refresh and retry"
            ))),
            Err(lines) => Err(Error::Conflict(format!(
                "anacron job `{job}` is on more than one line of {ANACRONTAB_PATH} \
                 ({lines:?}); fix the file by hand first"
            ))),
        }
    }

    /// Refuse a job id that is already taken. Disabled lines count: the
    /// operator meant to keep that job, and re-enabling it would create
    /// the very collision anacron cannot resolve.
    fn require_unused(file: &LineFile, job: &str, except: Option<usize>) -> Result<()> {
        let taken = file.lines().iter().enumerate().any(|(idx, line)| {
            Some(idx + 1) != except && entry_of(line).is_some_and(|entry| entry.job == job)
        });
        if taken {
            return Err(Error::Conflict(format!(
                "{ANACRONTAB_PATH} already has a job called `{job}`"
            )));
        }
        Ok(())
    }
}

impl JobWriter for AnacronWriter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Anacron
    }

    fn create(&self, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        check_user(spec)?;
        let job = spec.name.clone().ok_or_else(|| {
            Error::Validation(
                "an anacron job needs a name: it is the job id anacron tracks it by".into(),
            )
        })?;
        let delay = delay_option(spec)?.unwrap_or(DEFAULT_DELAY);
        let body = render_line(&spec.schedule, delay, &job, &spec.command)?;

        let before = self.read_anacrontab()?;
        let mut file = LineFile::parse(&before);
        Self::require_unused(&file, &job, None)?;
        file.append(&decorate(&body, spec.enabled));

        let change = Self::write_change(ANACRONTAB_PATH, before, file.render());
        let mut outcome = execute(&*self.host, vec![change], mode)?;
        outcome.id = Some(format!("anacron:{job}"));
        Ok(outcome)
    }

    fn update(&self, id: &str, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        check_user(spec)?;
        let old = parse_anacron_id(id)?;
        let before = self.read_anacrontab()?;
        let mut file = LineFile::parse(&before);
        let number = Self::locate(&file, &old)?;
        let raw = file
            .line(number)
            .expect("the located line exists")
            .to_string();
        let current = entry_of(&raw).expect("the located line is a job");

        let job = spec.name.clone().unwrap_or_else(|| old.clone());
        if job != old {
            Self::require_unused(&file, &job, Some(number))?;
        }
        // An edit that says nothing about the delay keeps the one the
        // file already has, rather than resetting it to our default.
        let delay = delay_option(spec)?.unwrap_or(current.delay);
        let body = render_line(&spec.schedule, delay, &job, &spec.command)?;
        file.replace(number, &raw, &decorate(&body, spec.enabled))?;

        let mut changes = vec![Self::write_change(ANACRONTAB_PATH, before, file.render())];
        if job != old {
            changes.push(Self::drop_spool(&old));
        }
        let mut outcome = execute(&*self.host, changes, mode)?;
        outcome.id = Some(format!("anacron:{job}"));
        Ok(outcome)
    }

    fn delete(&self, id: &str, mode: ApplyMode) -> Result<MutationOutcome> {
        let job = parse_anacron_id(id)?;
        let before = self.read_anacrontab()?;
        let mut file = LineFile::parse(&before);
        let number = Self::locate(&file, &job)?;
        let raw = file
            .line(number)
            .expect("the located line exists")
            .to_string();
        file.delete(number, &raw)?;

        let changes = vec![
            Self::write_change(ANACRONTAB_PATH, before, file.render()),
            Self::drop_spool(&job),
        ];
        let mut outcome = execute(&*self.host, changes, mode)?;
        outcome.id = Some(format!("anacron:{job}"));
        Ok(outcome)
    }

    fn set_enabled(&self, id: &str, enabled: bool, mode: ApplyMode) -> Result<MutationOutcome> {
        let job = parse_anacron_id(id)?;
        let before = self.read_anacrontab()?;
        let mut file = LineFile::parse(&before);
        let number = Self::locate(&file, &job)?;
        let raw = file
            .line(number)
            .expect("the located line exists")
            .to_string();

        let is_disabled = strip_disabled_marker(&raw).is_some();
        let mut note = None;
        let mut changes = Vec::new();
        if enabled == is_disabled {
            if enabled {
                file.uncomment(number, DISABLED_MARKER)?;
            } else {
                file.comment_out(number, &raw, DISABLED_MARKER)?;
            }
            changes.push(Self::write_change(ANACRONTAB_PATH, before, file.render()));
        } else {
            note = Some(format!(
                "anacron:{job} is already {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }

        let mut outcome = execute(&*self.host, changes, mode)?;
        outcome.notes.extend(note);
        outcome.id = Some(format!("anacron:{job}"));
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuvjobs_core::host::{FakeHost, PrivilegePolicy};
    use shuvjobs_core::manage::JobScope;
    use std::time::Duration;

    const ANACRONTAB: &str = "\
# /etc/anacrontab: configuration file for anacron
SHELL=/bin/sh
PATH=/sbin:/bin:/usr/sbin:/usr/bin

1\t5\tcron.daily\trun-parts --report /etc/cron.daily
7 10 cron.weekly run-parts --report /etc/cron.weekly
@monthly 15 cron.monthly run-parts --report /etc/cron.monthly
";

    fn sudoer() -> PrivilegePolicy {
        PrivilegePolicy {
            is_root: false,
            sudo: true,
        }
    }

    fn host_with(text: &str) -> Arc<FakeHost> {
        Arc::new(
            FakeHost::new()
                .with_policy(sudoer())
                .root_only("/etc/")
                .root_only("/var/spool/")
                .file(ANACRONTAB_PATH, text),
        )
    }

    fn writer(host: &Arc<FakeHost>) -> AnacronWriter {
        AnacronWriter::new(Arc::clone(host) as Arc<dyn Host>)
    }

    fn spec(name: &str, days: u64, command: &str) -> JobSpec {
        let mut spec = JobSpec::new(
            TaskSourceKind::Anacron,
            ScheduleType::Interval(Duration::from_secs(days * 86_400)),
            command.to_string(),
        );
        spec.name = Some(name.to_string());
        spec.scope = JobScope::System;
        spec
    }

    fn contents(host: &FakeHost) -> String {
        host.file_contents(ANACRONTAB_PATH)
            .expect("the anacrontab is there")
    }

    // -- render ----------------------------------------------------------

    #[test]
    fn a_line_is_period_delay_job_then_command() {
        assert_eq!(
            render_line(
                &ScheduleType::Interval(Duration::from_secs(7 * 86_400)),
                10,
                "backup",
                "/usr/local/bin/backup --all"
            )
            .unwrap(),
            "7 10 backup /usr/local/bin/backup --all"
        );
    }

    #[test]
    fn periods_are_whole_days_or_one_of_anacrons_own_words() {
        let days = |secs: u64| {
            render_line(
                &ScheduleType::Interval(Duration::from_secs(secs)),
                5,
                "j",
                "x",
            )
        };
        assert_eq!(days(86_400).unwrap(), "1 5 j x");
        assert_eq!(days(30 * 86_400).unwrap(), "30 5 j x");
        for secs in [0, 3_600, 90_000] {
            assert!(
                matches!(days(secs), Err(Error::Validation(_))),
                "{secs}s was accepted"
            );
        }
        for alias in ["@daily", "@weekly", "@MONTHLY", "@yearly", "@annually"] {
            let line = render_line(&ScheduleType::Calendar(alias.into()), 5, "j", "x").unwrap();
            assert_eq!(line, format!("{} 5 j x", alias.to_ascii_lowercase()));
        }
        for bad in ["@hourly", "@reboot", "Mon *-*-* 09:00:00"] {
            assert!(
                matches!(
                    render_line(&ScheduleType::Calendar(bad.into()), 5, "j", "x"),
                    Err(Error::Validation(_))
                ),
                "{bad} was accepted"
            );
        }
        assert!(matches!(
            render_line(&ScheduleType::Cron("0 9 * * *".into()), 5, "j", "x"),
            Err(Error::Validation(_))
        ));
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn creating_a_job_appends_one_line_as_root() {
        let host = host_with(ANACRONTAB);
        let outcome = writer(&host)
            .create(
                &spec("backup", 1, "/usr/local/bin/backup"),
                ApplyMode::Apply,
            )
            .unwrap();

        assert_eq!(outcome.id.as_deref(), Some("anacron:backup"));
        assert_eq!(
            contents(&host),
            format!("{ANACRONTAB}1 5 backup /usr/local/bin/backup\n")
        );
        assert_eq!(host.file_mode(ANACRONTAB_PATH), Some(0o644));
        match &outcome.changes[0] {
            Change::WriteFile { privilege, .. } => assert_eq!(*privilege, Privilege::Root),
            other => panic!("expected a file write, got {other:?}"),
        }
    }

    #[test]
    fn the_delay_column_comes_from_the_options_and_defaults_to_five() {
        let host = host_with(ANACRONTAB);
        let mut job = spec("backup", 1, "/usr/local/bin/backup");
        job.options
            .insert(DELAY_OPTION.to_string(), "45".to_string());
        writer(&host).create(&job, ApplyMode::Apply).unwrap();
        assert!(contents(&host).ends_with("1 45 backup /usr/local/bin/backup\n"));

        job.options
            .insert(DELAY_OPTION.to_string(), "half an hour".to_string());
        assert!(matches!(
            writer(&host).create(&job, ApplyMode::DryRun),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn a_job_id_that_is_already_taken_is_a_conflict() {
        let host = host_with(ANACRONTAB);
        let err = writer(&host)
            .create(&spec("cron.weekly", 7, "x"), ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert_eq!(contents(&host), ANACRONTAB);
    }

    #[test]
    fn a_disabled_job_still_owns_its_id() {
        let host = host_with(&format!(
            "{ANACRONTAB}#shuvjobs-disabled# 1 5 backup /usr/local/bin/backup\n"
        ));
        assert!(matches!(
            writer(&host).create(&spec("backup", 1, "x"), ApplyMode::DryRun),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn writing_the_anacrontab_without_sudo_asks_for_root() {
        let host = Arc::new(
            FakeHost::new()
                .root_only("/etc/")
                .file(ANACRONTAB_PATH, ANACRONTAB),
        );
        let err = writer(&host)
            .create(&spec("backup", 1, "x"), ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
        assert_eq!(contents(&host), ANACRONTAB);
    }

    #[test]
    fn a_job_needs_a_name_and_anacron_has_no_user_column() {
        let host = host_with(ANACRONTAB);
        let mut job = spec("backup", 1, "x");
        job.name = None;
        assert!(matches!(
            writer(&host).create(&job, ApplyMode::DryRun),
            Err(Error::Validation(_))
        ));

        let mut job = spec("backup", 1, "x");
        job.user = Some("nobody".to_string());
        match writer(&host).create(&job, ApplyMode::DryRun) {
            Err(Error::Unsupported { reason, .. }) => assert!(reason.contains("root"), "{reason}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // Saying `root` out loud is what anacron does anyway.
        let mut job = spec("backup", 1, "x");
        job.user = Some("root".to_string());
        writer(&host).create(&job, ApplyMode::DryRun).unwrap();
    }

    #[test]
    fn a_host_without_an_anacrontab_has_no_anacron() {
        let host = Arc::new(FakeHost::new().with_policy(sudoer()).root_only("/etc/"));
        let err = writer(&host)
            .create(&spec("backup", 1, "x"), ApplyMode::DryRun)
            .expect_err("must refuse");
        assert!(matches!(err, Error::Unavailable(_)), "got {err:?}");
    }

    #[test]
    fn a_dry_run_touches_nothing() {
        let host = host_with(ANACRONTAB);
        let outcome = writer(&host)
            .create(&spec("backup", 1, "x"), ApplyMode::DryRun)
            .unwrap();
        assert!(!outcome.applied);
        assert_eq!(outcome.changes.len(), 1);
        assert_eq!(contents(&host), ANACRONTAB);
    }

    // -- update ----------------------------------------------------------

    #[test]
    fn updating_a_job_replaces_its_line_and_keeps_the_rest() {
        let host = host_with(ANACRONTAB);
        let outcome = writer(&host)
            .update(
                "anacron:cron.weekly",
                &spec("cron.weekly", 14, "run-parts --report /etc/cron.weekly"),
                ApplyMode::Apply,
            )
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("anacron:cron.weekly"));
        assert_eq!(
            contents(&host),
            ANACRONTAB.replace(
                "7 10 cron.weekly run-parts --report /etc/cron.weekly",
                "14 10 cron.weekly run-parts --report /etc/cron.weekly"
            )
        );
        // The delay the file already had survives an edit that says
        // nothing about it.
        assert_eq!(outcome.changes.len(), 1);
    }

    #[test]
    fn renaming_a_job_takes_its_stale_spool_file_with_it() {
        let host = host_with(ANACRONTAB);
        let host = Arc::new(
            FakeHost::new()
                .with_policy(sudoer())
                .root_only("/etc/")
                .root_only("/var/spool/")
                .file(ANACRONTAB_PATH, &contents(&host))
                .file("/var/spool/anacron/cron.weekly", "20240101\n"),
        );
        let outcome = writer(&host)
            .update(
                "anacron:cron.weekly",
                &spec("weekly-jobs", 7, "run-parts --report /etc/cron.weekly"),
                ApplyMode::Apply,
            )
            .unwrap();

        assert_eq!(outcome.id.as_deref(), Some("anacron:weekly-jobs"));
        assert!(contents(&host).contains("7 10 weekly-jobs run-parts --report /etc/cron.weekly\n"));
        assert_eq!(host.file_contents("/var/spool/anacron/cron.weekly"), None);
        match &outcome.changes[1] {
            Change::RemoveFile {
                path,
                before,
                privilege,
            } => {
                assert_eq!(path, "/var/spool/anacron/cron.weekly");
                assert_eq!(*before, None);
                assert_eq!(*privilege, Privilege::Root);
            }
            other => panic!("expected a spool removal, got {other:?}"),
        }
    }

    #[test]
    fn renaming_onto_an_existing_job_is_a_conflict() {
        let host = host_with(ANACRONTAB);
        let err = writer(&host)
            .update(
                "anacron:cron.weekly",
                &spec("cron.daily", 7, "x"),
                ApplyMode::Apply,
            )
            .expect_err("must refuse");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert_eq!(contents(&host), ANACRONTAB);
    }

    #[test]
    fn a_job_that_is_no_longer_in_the_file_is_a_conflict() {
        let host = host_with(ANACRONTAB);
        for id in ["anacron:gone", "anacron:cron.hourly"] {
            let err = writer(&host)
                .update(id, &spec("gone", 1, "x"), ApplyMode::Apply)
                .expect_err(id);
            assert!(matches!(err, Error::Conflict(_)), "{id}: {err:?}");
        }
    }

    #[test]
    fn a_job_id_on_two_lines_is_ambiguous_rather_than_guessed_at() {
        let host = host_with(&format!("{ANACRONTAB}7 10 cron.weekly other-command\n"));
        let err = writer(&host)
            .delete("anacron:cron.weekly", ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
    }

    // -- delete ----------------------------------------------------------

    #[test]
    fn deleting_a_job_drops_the_line_and_the_spool_file() {
        let host = Arc::new(
            FakeHost::new()
                .with_policy(sudoer())
                .root_only("/etc/")
                .root_only("/var/spool/")
                .file(ANACRONTAB_PATH, ANACRONTAB)
                .file("/var/spool/anacron/cron.weekly", "20240101\n"),
        );
        let outcome = writer(&host)
            .delete("anacron:cron.weekly", ApplyMode::Apply)
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("anacron:cron.weekly"));
        assert_eq!(
            contents(&host),
            ANACRONTAB.replace("7 10 cron.weekly run-parts --report /etc/cron.weekly\n", "")
        );
        assert_eq!(host.file_contents("/var/spool/anacron/cron.weekly"), None);
    }

    #[test]
    fn deleting_a_job_that_never_ran_is_not_an_error() {
        let host = host_with(ANACRONTAB);
        writer(&host)
            .delete("anacron:cron.monthly", ApplyMode::Apply)
            .unwrap();
        assert!(!contents(&host).contains("cron.monthly"));
    }

    // -- set_enabled -----------------------------------------------------

    #[test]
    fn disabling_then_enabling_restores_the_exact_bytes() {
        let host = host_with(ANACRONTAB);
        writer(&host)
            .set_enabled("anacron:cron.daily", false, ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            contents(&host),
            ANACRONTAB.replace("1\t5\tcron.daily", "#shuvjobs-disabled# 1\t5\tcron.daily")
        );
        writer(&host)
            .set_enabled("anacron:cron.daily", true, ApplyMode::Apply)
            .unwrap();
        assert_eq!(contents(&host), ANACRONTAB);
    }

    #[test]
    fn toggling_a_job_that_is_already_in_that_state_does_nothing() {
        let host = host_with(ANACRONTAB);
        let outcome = writer(&host)
            .set_enabled("anacron:cron.daily", true, ApplyMode::Apply)
            .unwrap();
        assert!(outcome.changes.is_empty());
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.contains("already enabled")),
            "{:?}",
            outcome.notes
        );
        assert_eq!(contents(&host), ANACRONTAB);
    }

    #[test]
    fn the_writer_answers_for_anacron() {
        let host = host_with(ANACRONTAB);
        assert_eq!(writer(&host).kind(), TaskSourceKind::Anacron);
    }
}
