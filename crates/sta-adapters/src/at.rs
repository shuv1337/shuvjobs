//! `at` adapter — one-shot scheduled commands.
//!
//! `atq` lists the queue, `at -c <id>` dumps the script body of a job.
//! `atd` doesn't keep last-run state, so `last_run`/`last_status` stay `None`.

use std::process::Command;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use sta_core::{Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind};

#[derive(Debug, Default)]
pub struct AtAdapter;

impl AtAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Parse `atq` output. Rows look like `12<TAB>Thu Apr 14 14:00:00 2026 a alice`.
    /// The returned tasks have empty `command` — fill in via [`AtAdapter::parse_at_c`].
    pub fn parse_atq(atq_output: &str) -> Vec<ScheduledTask> {
        Self::parse_atq_in_timezone(atq_output, &Local)
    }

    pub fn parse_atq_in_timezone<Tz: TimeZone>(
        atq_output: &str,
        timezone: &Tz,
    ) -> Vec<ScheduledTask> {
        let mut tasks = Vec::new();
        for line in atq_output.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let Some(parsed) = parse_atq_line_in_timezone(line, timezone) else {
                eprintln!("warning: skipping unparseable atq line: {line}");
                continue;
            };
            tasks.push(ScheduledTask {
                id: format!("at:{}", parsed.id),
                name: format!("at job {}", parsed.id),
                source: TaskSourceKind::At,
                schedule: ScheduleType::OneShot(parsed.scheduled),
                last_run: None,
                last_status: None,
                last_duration: None,
                next_run: Some(parsed.scheduled),
                command: String::new(),
            });
        }
        tasks
    }

    /// Extract the command body from `at -c <id>` output. The script
    /// wraps the user's commands in a `${SHELL:-/bin/sh} << 'TAG'` heredoc.
    pub fn parse_at_c(text: &str) -> Option<String> {
        let mut lines = text.lines();
        let mut delimiter: Option<String> = None;
        for line in lines.by_ref() {
            if let Some(tag) = line
                .trim()
                .strip_prefix("${SHELL:-/bin/sh} << '")
                .and_then(|rest| rest.strip_suffix('\''))
            {
                delimiter = Some(tag.to_string());
                break;
            }
        }
        let tag = delimiter?;
        let mut body = Vec::new();
        for line in lines {
            if line == tag {
                break;
            }
            body.push(line);
        }
        while body.first().map(|s| s.trim().is_empty()).unwrap_or(false) {
            body.remove(0);
        }
        while body.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
            body.pop();
        }
        if body.is_empty() {
            return None;
        }
        Some(body.iter().map(|l| l.trim()).collect::<Vec<_>>().join("; "))
    }
}

impl TaskSource for AtAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::At
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        if Command::new("atq").arg("-V").output().is_err() {
            return Err(Error::Unavailable("`atq` not available".into()));
        }

        let listing = Command::new("atq").output().map_err(|e| Error::Command {
            command: "atq".into(),
            message: e.to_string(),
        })?;
        if !listing.status.success() {
            return Err(Error::Command {
                command: "atq".into(),
                message: String::from_utf8_lossy(&listing.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&listing.stdout);
        let mut tasks = Self::parse_atq(&stdout);

        for task in &mut tasks {
            let Some(num) = task.id.strip_prefix("at:") else {
                continue;
            };
            let Ok(body) = Command::new("at").args(["-c", num]).output() else {
                continue;
            };
            if !body.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&body.stdout);
            if let Some(cmd) = Self::parse_at_c(&text) {
                task.command = cmd;
            }
        }

        Ok(tasks)
    }
}

#[derive(Debug)]
struct AtqRow {
    id: String,
    scheduled: DateTime<Utc>,
}

fn parse_atq_line_in_timezone<Tz: TimeZone>(line: &str, timezone: &Tz) -> Option<AtqRow> {
    // Format is `<id>\t<date> <queue> <user>`. The date is exactly five
    // tokens (`Thu Apr 14 14:00:00 2026`), so we just take the first
    // five whitespace tokens after the id.
    let mut iter = line.split_whitespace();
    let id = iter.next()?.to_string();
    let date_tokens: Vec<&str> = iter.by_ref().take(5).collect();
    if date_tokens.len() < 5 {
        return None;
    }
    let date_str = date_tokens.join(" ");
    let naive = NaiveDateTime::parse_from_str(&date_str, "%a %b %d %H:%M:%S %Y").ok()?;
    let scheduled = timezone
        .from_local_datetime(&naive)
        .earliest()?
        .with_timezone(&Utc);
    Some(AtqRow { id, scheduled })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE_JOB: &str = "12\tTue Apr 14 14:00:00 2026 a alice\n";

    #[test]
    fn parses_single_atq_job() {
        let tasks = AtAdapter::parse_atq(SINGLE_JOB);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.id, "at:12");
        assert_eq!(t.source, TaskSourceKind::At);
        assert!(matches!(t.schedule, ScheduleType::OneShot(_)));
        assert!(t.next_run.is_some());
        assert!(t.command.is_empty(), "command is filled in by parse_at_c");
    }

    #[test]
    fn parses_atq_timestamp_in_the_supplied_local_timezone() {
        let timezone = chrono::FixedOffset::east_opt(3 * 60 * 60).unwrap();
        let tasks =
            AtAdapter::parse_atq_in_timezone("12\tTue Apr 14 14:00:00 2026 a alice", &timezone);
        assert_eq!(
            tasks[0].next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 14, 11, 0, 0).unwrap())
        );
    }

    #[test]
    fn parses_empty_atq_to_no_tasks() {
        let tasks = AtAdapter::parse_atq("");
        assert!(tasks.is_empty());
    }

    #[test]
    fn parses_multiple_atq_jobs() {
        let multi = "12\tTue Apr 14 14:00:00 2026 a alice\n13\tWed Apr 15 09:30:00 2026 a bob\n";
        let tasks = AtAdapter::parse_atq(multi);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "at:12");
        assert_eq!(tasks[1].id, "at:13");
    }

    #[test]
    fn skips_garbage_atq_lines() {
        let garbage = "this is not an atq line\n";
        let tasks = AtAdapter::parse_atq(garbage);
        assert!(tasks.is_empty());
    }

    const AT_C_BODY: &str = "#!/bin/sh\n\
                              # atrun uid=1000 gid=1000\n\
                              # mail alice 0\n\
                              umask 22\n\
                              PATH=/usr/local/bin:/bin; export PATH\n\
                              HOME=/home/alice; export HOME\n\
                              cd /home/alice || {\n\
                                  echo 'Execution directory inaccessible' >&2\n\
                                  exit 1\n\
                              }\n\
                              ${SHELL:-/bin/sh} << 'marcinABCDEF1234'\n\
                              \n\
                              echo \"hello world\"\n\
                              date\n\
                              \n\
                              marcinABCDEF1234\n";

    #[test]
    fn parse_at_c_extracts_heredoc_body() {
        let cmd = AtAdapter::parse_at_c(AT_C_BODY).unwrap();
        assert_eq!(cmd, "echo \"hello world\"; date");
    }

    #[test]
    fn parse_at_c_returns_none_when_no_heredoc() {
        assert!(AtAdapter::parse_at_c("just some random text").is_none());
    }

    #[test]
    fn adapter_reports_at_kind() {
        assert_eq!(AtAdapter::new().kind(), TaskSourceKind::At);
    }
}
