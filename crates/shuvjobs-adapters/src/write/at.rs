//! The `at` writer — one-shot jobs, scheduled by a command rather than
//! by a file.
//!
//! `at` is the only source here with no config file to edit: the queue
//! lives in `/var/spool/at*`, in a format the daemon owns, so every
//! change is a command and the plan is one line long. Two consequences
//! shape everything below.
//!
//! First, the job id does not exist until the command has run. `at`
//! prints `job 14 at Wed Sep  2 14:00:00 2026` on *stderr*, so the
//! writer reads it back out of the recorded output after [`execute`],
//! and a dry run honestly reports no id at all.
//!
//! Second, there is no edit. An update is a fresh `at` followed by
//! `atrm` of the old job, in that order: if the new job cannot be
//! queued, the old one is still there, and the operator has lost
//! nothing but the attempt.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use shuvjobs_core::host::shell::shell_quote;
use shuvjobs_core::host::{Host, Privilege};
use shuvjobs_core::manage::{
    execute, ApplyMode, Change, FailPolicy, JobSpec, JobWriter, MutationOutcome,
};
use shuvjobs_core::{Error, Result, ScheduleType, TaskSourceKind};

use crate::at::AtAdapter;
use crate::ids::parse_at_id;
use crate::write::validate_command;

/// How far ahead a one-shot has to be. `atd` polls on a minute
/// granularity and `at` refuses times already gone, so anything closer
/// than this either never runs or races the daemon.
const MIN_LEAD_SECONDS: i64 = 60;

/// The id used in errors raised before a job number exists.
const NEW_JOB: &str = "new";

fn unsupported(id: &str, reason: &str) -> Error {
    Error::Unsupported {
        source_kind: TaskSourceKind::At,
        id: id.to_string(),
        reason: reason.to_string(),
    }
}

/// The job number `at` prints on stderr when it queues a job.
///
/// Both Debian's `at` and the macOS one print `job <n> at <date>`, after
/// an optional warning line about which shell will be used, so the
/// parser looks at every line and takes the first that has the shape.
/// Done by hand rather than with a regex: it is one prefix, one run of
/// digits, and one separator.
pub fn parse_at_job_id(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        let Some(rest) = line.trim_start().strip_prefix("job ") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || !rest[digits.len()..].starts_with(" at ") {
            continue;
        }
        if let Ok(job) = digits.parse::<u64>() {
            return Some(job);
        }
    }
    None
}

/// Re-shape the failures `at` and `atrm` report in prose.
///
/// `at` validates the time itself and says so on stderr; that is the
/// operator's typo, not a broken command, so it comes back as
/// [`Error::Validation`] with the tool's own wording.
fn map_at_error(err: Error) -> Error {
    let Error::Command { command, message } = err else {
        return err;
    };
    if message.contains("Garbled time") {
        return Error::Validation(message);
    }
    if message.contains("You do not have permission") {
        return Error::Command {
            command,
            message: format!("{message} (check /etc/at.allow and /etc/at.deny)"),
        };
    }
    Error::Command { command, message }
}

/// Creates and removes `at` jobs on one [`Host`].
pub struct AtWriter {
    host: Arc<dyn Host>,
}

impl AtWriter {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }

    /// The `at` invocation for a schedule.
    ///
    /// A concrete instant is rendered with `-t`, in the host's own UTC
    /// offset: `at`'s free-text grammar is locale-dependent, but
    /// `[[CC]YY]MMDDhhmm[.ss]` is POSIX and means the same thing
    /// everywhere. Free text the operator typed is passed through word
    /// by word, each quoted, so `now + 5 minutes` reaches `at` as five
    /// arguments and nothing reaches the shell.
    fn at_command(&self, schedule: &ScheduleType, now: DateTime<Utc>) -> Result<String> {
        match schedule {
            ScheduleType::OneShot(when) => {
                let lead = when.signed_duration_since(now).num_seconds();
                if lead < MIN_LEAD_SECONDS {
                    return Err(Error::Validation(format!(
                        "`at` needs a time at least {MIN_LEAD_SECONDS} seconds from now: \
                         {} is too soon",
                        when.to_rfc3339()
                    )));
                }
                let offset = self.host.utc_offset()?;
                let stamp = when.with_timezone(&offset).format("%Y%m%d%H%M.%S");
                Ok(format!("at -t {}", shell_quote(&stamp.to_string())))
            }
            ScheduleType::Calendar(text) => {
                let words: Vec<&str> = text.split_whitespace().collect();
                if words.is_empty() {
                    return Err(Error::Validation("schedule must not be empty".to_string()));
                }
                let rendered = words
                    .iter()
                    .map(|word| shell_quote(word))
                    .collect::<Vec<_>>()
                    .join(" ");
                Ok(format!("at {rendered}"))
            }
            ScheduleType::Cron(_) => Err(unsupported(
                NEW_JOB,
                "a repeating expression belongs in cron, not in a one-shot `at` job",
            )),
            ScheduleType::Interval(_) => Err(unsupported(
                NEW_JOB,
                "an `at` job runs once; use cron or a systemd timer to repeat it",
            )),
        }
    }

    /// [`JobWriter::create`] with the clock supplied, so the one-shot
    /// lead-time check is testable.
    pub fn create_at(
        &self,
        spec: &JobSpec,
        mode: ApplyMode,
        now: DateTime<Utc>,
    ) -> Result<MutationOutcome> {
        spec.validate()?;
        validate_command(&spec.command)?;
        // `at` queues jobs for whoever runs it, and has no `-u`. Asking
        // for another user would silently queue it for the wrong one.
        if let Some(user) = spec.user.as_deref() {
            if user != self.host.current_user()? {
                return Err(unsupported(
                    NEW_JOB,
                    &format!("`at` has no -u; run shuvjobs as {user} to queue a job for them"),
                ));
            }
        }

        let cmd = self.at_command(&spec.schedule, now)?;
        let change = Change::Command {
            cmd: cmd.clone(),
            stdin: Some(format!("{}\n", spec.command.trim())),
            privilege: Privilege::User,
            description: format!("queue `{}` with at", spec.command.trim()),
            on_fail: FailPolicy::Error,
        };

        let mut outcome = execute(&*self.host, vec![change], mode).map_err(map_at_error)?;
        if mode == ApplyMode::DryRun {
            outcome
                .notes
                .push("the job number is assigned by `at` when the plan runs".to_string());
            return Ok(outcome);
        }
        let stderr = outcome
            .outputs
            .first()
            .map(|output| output.stderr.clone())
            .unwrap_or_default();
        let job = parse_at_job_id(&stderr).ok_or_else(|| Error::Parse {
            kind: "at".to_string(),
            message: format!("`{cmd}` did not report a job number: {stderr:?}"),
        })?;
        outcome.id = Some(format!("at:{job}"));
        Ok(outcome)
    }

    /// The job body `at -c` reports, so an edit that only changes the
    /// time keeps the command it had.
    fn existing_command(&self, job: u64) -> Result<String> {
        let cmd = format!("at -c {}", shell_quote(&job.to_string()));
        let output = self.host.run(&cmd, None, Privilege::User)?;
        let text = output.require_success(&cmd).map_err(map_at_error)?;
        AtAdapter::parse_at_c(&text).ok_or_else(|| Error::Parse {
            kind: "at".to_string(),
            message: format!("could not read the command of at job {job}"),
        })
    }

    fn atrm_change(job: u64) -> Change {
        Change::Command {
            cmd: format!("atrm {}", shell_quote(&job.to_string())),
            stdin: None,
            privilege: Privilege::User,
            description: format!("remove at job {job}"),
            on_fail: FailPolicy::Error,
        }
    }

    /// `atrm` fails the same way for a job that is not ours and for one
    /// that is not there, because the queue directory is not readable by
    /// anybody but root and the owner.
    fn map_atrm_error(&self, err: Error, job: u64) -> Error {
        let Error::Command { command, message } = err else {
            return err;
        };
        let policy = self.host.policy();
        let denied = message.contains("Cannot find jobid") || message.contains("permission");
        if denied && !policy.is_root && !policy.sudo {
            return Error::NeedsRoot {
                operation: format!("remove at job {job}"),
            };
        }
        Error::Command { command, message }
    }
}

impl JobWriter for AtWriter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::At
    }

    fn create(&self, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        self.create_at(spec, mode, Utc::now())
    }

    /// Queue the replacement first, drop the old job second.
    ///
    /// A failure anywhere in the first step leaves the original job
    /// exactly where it was; the worst case is a duplicate, which is
    /// visible in `atq`, rather than a silently lost job.
    fn update(&self, id: &str, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        let old = parse_at_id(id)?;
        let mut spec = spec.clone();
        if spec.command.trim().is_empty() {
            spec.command = self.existing_command(old)?;
        }

        let mut outcome = self.create_at(&spec, mode, Utc::now())?;
        let removed = execute(&*self.host, vec![Self::atrm_change(old)], mode)
            .map_err(|err| self.map_atrm_error(err, old))?;
        outcome.changes.extend(removed.changes);
        outcome.outputs.extend(removed.outputs);
        outcome.applied = removed.applied;
        outcome.notes.push(format!("replaced at job {old}"));
        Ok(outcome)
    }

    fn delete(&self, id: &str, mode: ApplyMode) -> Result<MutationOutcome> {
        let job = parse_at_id(id)?;
        let mut outcome = execute(&*self.host, vec![Self::atrm_change(job)], mode)
            .map_err(|err| self.map_atrm_error(err, job))?;
        outcome.id = Some(format!("at:{job}"));
        Ok(outcome)
    }

    fn set_enabled(&self, id: &str, _enabled: bool, _mode: ApplyMode) -> Result<MutationOutcome> {
        // The queue has no disabled state: a job is pending or gone.
        Err(unsupported(
            id,
            "`at` jobs cannot be paused; delete the job and schedule a new one",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use shuvjobs_core::host::fake::Call;
    use shuvjobs_core::host::{FakeHost, PrivilegePolicy};

    /// What Debian's `at` prints: a warning first, then the job line.
    const DEBIAN_STDERR: &str =
        "warning: commands will be executed using /bin/sh\njob 14 at Wed Sep  2 14:00:00 2026\n";
    /// macOS prints the job line on its own.
    const MACOS_STDERR: &str = "job 7 at Wed Sep  2 14:00:00 2026\n";

    fn writer(host: &Arc<FakeHost>) -> AtWriter {
        AtWriter::new(Arc::clone(host) as Arc<dyn Host>)
    }

    fn spec(schedule: ScheduleType, command: &str) -> JobSpec {
        JobSpec::new(TaskSourceKind::At, schedule, command.to_string())
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 10, 0, 0).unwrap()
    }

    fn at_ten_thirty() -> ScheduleType {
        ScheduleType::OneShot(Utc.with_ymd_and_hms(2026, 9, 2, 10, 30, 0).unwrap())
    }

    fn call(cmd: &str, stdin: Option<&str>) -> Call {
        Call {
            cmd: cmd.to_string(),
            stdin: stdin.map(|s| s.as_bytes().to_vec()),
            privilege: Privilege::User,
        }
    }

    #[test]
    fn a_one_shot_is_rendered_with_dash_t_in_the_hosts_own_offset() {
        let host = Arc::new(
            FakeHost::new()
                .with_offset(FixedOffset::east_opt(3 * 3600).unwrap())
                .script("at -t '202609021330.00'", 0, "", DEBIAN_STDERR),
        );
        let outcome = writer(&host)
            .create_at(&spec(at_ten_thirty(), "echo hi"), ApplyMode::Apply, now())
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("at:14"));
        assert_eq!(
            host.calls(),
            vec![call("at -t '202609021330.00'", Some("echo hi\n"))]
        );
    }

    #[test]
    fn a_one_shot_too_close_to_now_is_refused_before_any_call() {
        let host = Arc::new(FakeHost::new());
        let soon = ScheduleType::OneShot(now() + chrono::Duration::seconds(30));
        let err = writer(&host)
            .create_at(&spec(soon, "echo hi"), ApplyMode::Apply, now())
            .expect_err("must refuse a time inside the lead window");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(host.calls().is_empty());
    }

    #[test]
    fn free_text_schedules_reach_at_one_quoted_word_at_a_time() {
        let host =
            Arc::new(FakeHost::new().script("at 'now' '+' '5' 'minutes'", 0, "", MACOS_STDERR));
        let outcome = writer(&host)
            .create_at(
                &spec(ScheduleType::Calendar("now + 5 minutes".into()), "echo hi"),
                ApplyMode::Apply,
                now(),
            )
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("at:7"));
        assert_eq!(
            host.calls(),
            vec![call("at 'now' '+' '5' 'minutes'", Some("echo hi\n"))]
        );
    }

    #[test]
    fn the_job_number_is_read_from_either_dialects_stderr() {
        assert_eq!(parse_at_job_id(DEBIAN_STDERR), Some(14));
        assert_eq!(parse_at_job_id(MACOS_STDERR), Some(7));
        for text in [
            "",
            "job at Wed Sep  2 14:00:00 2026\n",
            "job 14\n",
            "jobs 14 at Wed\n",
            "job 14x at Wed\n",
        ] {
            assert_eq!(parse_at_job_id(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_missing_job_number_is_a_parse_error() {
        let host = Arc::new(FakeHost::new().script("at -t '202609021030.00'", 0, "", "queued\n"));
        let err = writer(&host)
            .create_at(&spec(at_ten_thirty(), "echo hi"), ApplyMode::Apply, now())
            .expect_err("must not invent an id");
        match err {
            Error::Parse { kind, .. } => assert_eq!(kind, "at"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn a_garbled_time_is_the_operators_mistake() {
        let host = Arc::new(FakeHost::new().script("at 'yesteryear'", 1, "", "Garbled time\n"));
        let err = writer(&host)
            .create_at(
                &spec(ScheduleType::Calendar("yesteryear".into()), "echo hi"),
                ApplyMode::Apply,
                now(),
            )
            .expect_err("must reject the time");
        match err {
            Error::Validation(message) => assert!(message.contains("Garbled time"), "{message}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn a_denied_queue_keeps_the_tools_own_wording() {
        let host = Arc::new(FakeHost::new().script(
            "at -t '202609021030.00'",
            1,
            "",
            "You do not have permission to use at.\n",
        ));
        let err = writer(&host)
            .create_at(&spec(at_ten_thirty(), "echo hi"), ApplyMode::Apply, now())
            .expect_err("must fail");
        match err {
            Error::Command { message, .. } => {
                assert!(message.contains("You do not have permission"), "{message}");
                assert!(message.contains("at.deny"), "{message}");
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn repeating_schedules_belong_to_another_source() {
        let host = Arc::new(FakeHost::new());
        for schedule in [
            ScheduleType::Cron("0 9 * * *".into()),
            ScheduleType::Interval(std::time::Duration::from_secs(3600)),
        ] {
            let err = writer(&host)
                .create_at(&spec(schedule, "echo hi"), ApplyMode::Apply, now())
                .expect_err("must refuse a repeating schedule");
            assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
        }
        assert!(host.calls().is_empty());
    }

    #[test]
    fn queueing_for_another_user_is_unsupported() {
        let host = Arc::new(FakeHost::new());
        let mut job = spec(at_ten_thirty(), "echo hi");
        job.user = Some("alice".to_string());
        let err = writer(&host)
            .create_at(&job, ApplyMode::Apply, now())
            .expect_err("must refuse another user");
        match err {
            Error::Unsupported { reason, .. } => assert!(reason.contains("no -u"), "{reason}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(host.calls().is_empty());

        // Naming yourself is fine: it is what `at` would do anyway.
        let host = Arc::new(FakeHost::new().script("at -t '202609021030.00'", 0, "", MACOS_STDERR));
        let mut mine = spec(at_ten_thirty(), "echo hi");
        mine.user = Some("tester".to_string());
        writer(&host)
            .create_at(&mine, ApplyMode::Apply, now())
            .unwrap();
    }

    #[test]
    fn a_dry_run_makes_no_calls_and_reports_no_id() {
        let host = Arc::new(FakeHost::new());
        let outcome = writer(&host)
            .create_at(&spec(at_ten_thirty(), "echo hi"), ApplyMode::DryRun, now())
            .unwrap();
        assert!(host.calls().is_empty());
        assert!(!outcome.applied);
        assert_eq!(outcome.id, None);
        assert_eq!(outcome.changes.len(), 1);
        assert!(outcome.notes.iter().any(|n| n.contains("job number")));
    }

    #[test]
    fn an_update_queues_the_new_job_before_removing_the_old_one() {
        let host = Arc::new(
            FakeHost::new()
                .script("at 'noon' 'tomorrow'", 0, "", MACOS_STDERR)
                .script("atrm '14'", 0, "", ""),
        );
        let outcome = writer(&host)
            .update(
                "at:14",
                &spec(ScheduleType::Calendar("noon tomorrow".into()), "echo new"),
                ApplyMode::Apply,
            )
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("at:7"));
        assert_eq!(
            host.calls(),
            vec![
                call("at 'noon' 'tomorrow'", Some("echo new\n")),
                call("atrm '14'", None),
            ]
        );
    }

    #[test]
    fn an_update_with_no_command_keeps_the_one_at_c_reports() {
        let at_c = "#!/bin/sh\n\
                    ${SHELL:-/bin/sh} << 'marcinABCDEF1234'\n\
                    echo original\n\
                    marcinABCDEF1234\n";
        let host = Arc::new(
            FakeHost::new()
                .script("at -c '14'", 0, at_c, "")
                .script("at 'noon' 'tomorrow'", 0, "", MACOS_STDERR)
                .script("atrm '14'", 0, "", ""),
        );
        writer(&host)
            .update(
                "at:14",
                &spec(ScheduleType::Calendar("noon tomorrow".into()), "   "),
                ApplyMode::Apply,
            )
            .unwrap();
        assert_eq!(
            host.calls()[1],
            call("at 'noon' 'tomorrow'", Some("echo original\n"))
        );
    }

    #[test]
    fn an_update_whose_create_fails_removes_nothing() {
        let host =
            Arc::new(FakeHost::new().script("at 'noon' 'tomorrow'", 1, "", "Garbled time\n"));
        writer(&host)
            .update(
                "at:14",
                &spec(ScheduleType::Calendar("noon tomorrow".into()), "echo new"),
                ApplyMode::Apply,
            )
            .expect_err("must fail");
        assert_eq!(
            host.calls(),
            vec![call("at 'noon' 'tomorrow'", Some("echo new\n"))]
        );
    }

    #[test]
    fn delete_runs_atrm() {
        let host = Arc::new(FakeHost::new().script("atrm '14'", 0, "", ""));
        let outcome = writer(&host).delete("at:14", ApplyMode::Apply).unwrap();
        assert_eq!(outcome.id.as_deref(), Some("at:14"));
        assert!(outcome.applied);
        assert_eq!(host.calls(), vec![call("atrm '14'", None)]);
    }

    #[test]
    fn deleting_somebody_elses_job_asks_for_root() {
        let host = Arc::new(FakeHost::new().script("atrm '14'", 1, "", "Cannot find jobid 14\n"));
        let err = writer(&host)
            .delete("at:14", ApplyMode::Apply)
            .expect_err("must fail");
        match err {
            Error::NeedsRoot { operation } => assert_eq!(operation, "remove at job 14"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }

        // With sudo already offered there is nothing more to suggest, so
        // the tool's own failure is what the operator sees.
        let sudoer = Arc::new(
            FakeHost::new()
                .with_policy(PrivilegePolicy {
                    is_root: false,
                    sudo: true,
                })
                .script("atrm '14'", 1, "", "Cannot find jobid 14\n"),
        );
        let err = writer(&sudoer)
            .delete("at:14", ApplyMode::Apply)
            .expect_err("must fail");
        assert!(matches!(err, Error::Command { .. }), "got {err:?}");
    }

    #[test]
    fn a_dry_run_delete_makes_no_calls() {
        let host = Arc::new(FakeHost::new());
        let outcome = writer(&host).delete("at:14", ApplyMode::DryRun).unwrap();
        assert!(host.calls().is_empty());
        assert!(!outcome.applied);
        assert_eq!(outcome.changes.len(), 1);
    }

    #[test]
    fn a_bad_id_never_reaches_the_host() {
        let host = Arc::new(FakeHost::new());
        assert!(matches!(
            writer(&host).delete("at:nope", ApplyMode::Apply),
            Err(Error::Validation(_))
        ));
        assert!(host.calls().is_empty());
    }

    #[test]
    fn at_jobs_cannot_be_paused() {
        let host = Arc::new(FakeHost::new());
        for enabled in [true, false] {
            let err = writer(&host)
                .set_enabled("at:14", enabled, ApplyMode::Apply)
                .expect_err("must refuse");
            match err {
                Error::Unsupported { id, reason, .. } => {
                    assert_eq!(id, "at:14");
                    assert!(reason.contains("cannot be paused"), "{reason}");
                }
                other => panic!("expected Unsupported, got {other:?}"),
            }
        }
        assert!(host.calls().is_empty());
    }

    #[test]
    fn the_writer_reports_its_own_source() {
        let host = Arc::new(FakeHost::new());
        assert_eq!(writer(&host).kind(), TaskSourceKind::At);
    }
}
