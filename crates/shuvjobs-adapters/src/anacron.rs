//! anacron adapter.
//!
//! `/etc/anacrontab` lines are `period delay job-id command...` where
//! period is days as an integer or one of the cronie aliases `@daily`,
//! `@weekly`, `@monthly`, `@yearly`/`@annually`. Last-run dates sit
//! under `/var/spool/anacron/<job-id>` in `YYYYMMDD` form.

use std::fs;
use std::path::Path;
use std::time::Duration;

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use shuvjobs_core::{Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind};

#[derive(Debug, Default)]
pub struct AnacronAdapter;

impl AnacronAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Parse `/etc/anacrontab`. `last_run`/`next_run` are filled in by
    /// `collect()` from the per-job spool file.
    pub fn parse_anacrontab(contents: &str) -> Vec<ScheduledTask> {
        let mut out = Vec::new();
        for raw in contents.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if is_env_assignment(line) {
                continue;
            }
            let Some(entry) = parse_entry(line) else {
                eprintln!("warning: skipping malformed anacrontab line: {line}");
                continue;
            };
            let schedule = match &entry.period {
                Period::Days(n) => ScheduleType::Interval(Duration::from_secs(n * 86_400)),
                Period::Monthly => ScheduleType::Calendar("@monthly".into()),
                Period::Yearly => ScheduleType::Calendar("@yearly".into()),
            };
            out.push(ScheduledTask {
                id: format!("anacron:{}", entry.job_id),
                name: entry.job_id.clone(),
                source: TaskSourceKind::Anacron,
                schedule,
                last_run: None,
                last_status: None,
                last_duration: None,
                next_run: None,
                command: entry.command,
            });
        }
        out
    }

    /// Parse `/var/spool/anacron/<job-id>` (single `YYYYMMDD` line).
    pub fn parse_spool_file(contents: &str) -> Option<NaiveDate> {
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return None;
        }
        NaiveDate::parse_from_str(trimmed, "%Y%m%d").ok()
    }
}

impl TaskSource for AnacronAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Anacron
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        let anacrontab = Path::new("/etc/anacrontab");
        if !anacrontab.exists() {
            return Err(Error::Unavailable("/etc/anacrontab not present".into()));
        }
        let text = fs::read_to_string(anacrontab)?;
        let mut tasks = Self::parse_anacrontab(&text);

        for task in &mut tasks {
            let Some(job_id) = task.id.strip_prefix("anacron:") else {
                continue;
            };
            let spool_path = format!("/var/spool/anacron/{job_id}");
            // A missing spool file means the job has never run yet.
            let Ok(text) = fs::read_to_string(&spool_path) else {
                continue;
            };
            let Some(date) = Self::parse_spool_file(&text) else {
                continue;
            };
            // anacron only records the date — pin to UTC midnight.
            let last = Utc
                .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
                .single();
            task.last_run = last;
            if let (Some(last), Some(d)) = (last, period_advance(&task.schedule)) {
                task.next_run = Some(last + d);
            }
        }

        Ok(tasks)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Period {
    Days(u64),
    Monthly,
    Yearly,
}

impl Period {
    fn parse(token: &str) -> Option<Self> {
        if let Some(alias) = token.strip_prefix('@') {
            return match alias.to_ascii_lowercase().as_str() {
                "daily" => Some(Self::Days(1)),
                "weekly" => Some(Self::Days(7)),
                "monthly" => Some(Self::Monthly),
                "yearly" | "annually" => Some(Self::Yearly),
                _ => None,
            };
        }
        token.parse().ok().filter(|&n| n > 0).map(Self::Days)
    }
}

#[derive(Debug)]
struct Entry {
    period: Period,
    #[allow(dead_code)]
    delay_minutes: u64,
    job_id: String,
    command: String,
}

fn parse_entry(line: &str) -> Option<Entry> {
    // `period delay job-id <command...>` — track byte offsets so the
    // command keeps its original whitespace.
    let mut iter = line.split_whitespace();
    let period_tok = iter.next()?;
    let delay_tok = iter.next()?;
    let job_id = iter.next()?.to_string();
    let mut cursor = 0;
    for tok in [period_tok, delay_tok, job_id.as_str()] {
        let pos = line[cursor..].find(tok)? + cursor;
        cursor = pos + tok.len();
    }
    let command = line[cursor..].trim().to_string();
    if command.is_empty() {
        return None;
    }

    let period = Period::parse(period_tok)?;
    let delay_minutes: u64 = delay_tok.parse().ok()?;
    Some(Entry {
        period,
        delay_minutes,
        job_id,
        command,
    })
}

/// How far anacron will wait after a recorded run before the job is due
/// again. Calendar aliases are approximated; anacron itself only tracks
/// whole days. Shared with the SSH bridge so both paths agree.
pub fn period_advance(schedule: &ScheduleType) -> Option<chrono::Duration> {
    match schedule {
        ScheduleType::Interval(d) => chrono::Duration::from_std(*d).ok(),
        ScheduleType::Calendar(s) if s == "@monthly" => Some(chrono::Duration::days(30)),
        ScheduleType::Calendar(s) if s == "@yearly" => Some(chrono::Duration::days(365)),
        _ => None,
    }
}

fn is_env_assignment(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    if i >= bytes.len() {
        return false;
    }
    let first = bytes[i];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    i += 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'='
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD_ANACRONTAB: &str = "\
# /etc/anacrontab: configuration file for anacron
SHELL=/bin/sh
PATH=/usr/local/sbin:/usr/local/bin:/sbin:/bin:/usr/sbin:/usr/bin

#period  delay  job-identifier  command
1        5      cron.daily      run-parts /etc/cron.daily
7        25     cron.weekly     run-parts /etc/cron.weekly
@monthly 45     cron.monthly    run-parts /etc/cron.monthly
";

    #[test]
    fn parses_three_entry_anacrontab() {
        let tasks = AnacronAdapter::parse_anacrontab(STANDARD_ANACRONTAB);
        assert_eq!(tasks.len(), 3);

        let daily = &tasks[0];
        assert_eq!(daily.id, "anacron:cron.daily");
        assert_eq!(daily.name, "cron.daily");
        assert_eq!(daily.source, TaskSourceKind::Anacron);
        assert_eq!(daily.command, "run-parts /etc/cron.daily");
        assert!(matches!(
            daily.schedule,
            ScheduleType::Interval(d) if d == Duration::from_secs(86_400)
        ));

        let weekly = &tasks[1];
        assert!(matches!(
            weekly.schedule,
            ScheduleType::Interval(d) if d == Duration::from_secs(7 * 86_400)
        ));

        let monthly = &tasks[2];
        assert!(matches!(monthly.schedule, ScheduleType::Calendar(ref s) if s == "@monthly"));
    }

    #[test]
    fn parses_env_only_anacrontab_to_no_tasks() {
        let text = "# comment\nSHELL=/bin/sh\nPATH=/usr/bin\n";
        let tasks = AnacronAdapter::parse_anacrontab(text);
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_spool_file_reads_yyyymmdd() {
        let date = AnacronAdapter::parse_spool_file("20260410\n").unwrap();
        assert_eq!(date, NaiveDate::from_ymd_opt(2026, 4, 10).unwrap());
    }

    #[test]
    fn parse_spool_file_handles_empty() {
        assert!(AnacronAdapter::parse_spool_file("").is_none());
        assert!(AnacronAdapter::parse_spool_file("   \n").is_none());
    }

    #[test]
    fn parse_spool_file_rejects_garbage() {
        assert!(AnacronAdapter::parse_spool_file("not a date").is_none());
    }

    // cronie's anacron accepts these aliases alongside integer days.
    const ALIAS_ANACRONTAB: &str = "\
@daily   5  d-job  /usr/local/bin/daily
@weekly  10 w-job  /usr/local/bin/weekly
@YEARLY  15 y-job  /usr/local/bin/yearly
@annually 15 a-job /usr/local/bin/annually
";

    #[test]
    fn parses_period_aliases() {
        let tasks = AnacronAdapter::parse_anacrontab(ALIAS_ANACRONTAB);
        assert_eq!(tasks.len(), 4, "got {tasks:?}");
        assert!(matches!(
            tasks[0].schedule,
            ScheduleType::Interval(d) if d == Duration::from_secs(86_400)
        ));
        assert!(matches!(
            tasks[1].schedule,
            ScheduleType::Interval(d) if d == Duration::from_secs(7 * 86_400)
        ));
        assert!(matches!(tasks[2].schedule, ScheduleType::Calendar(ref s) if s == "@yearly"));
        assert!(matches!(tasks[3].schedule, ScheduleType::Calendar(ref s) if s == "@yearly"));
        assert_eq!(
            period_advance(&tasks[2].schedule),
            Some(chrono::Duration::days(365))
        );
    }

    #[test]
    fn rejects_zero_day_period() {
        let tasks = AnacronAdapter::parse_anacrontab("0 5 job /bin/true\n");
        assert!(tasks.is_empty());
    }

    #[test]
    fn skips_malformed_period_token() {
        let text = "abc 5 cron.daily run-parts /etc/cron.daily\n";
        let tasks = AnacronAdapter::parse_anacrontab(text);
        assert!(tasks.is_empty());
    }

    #[test]
    fn adapter_reports_anacron_kind() {
        assert_eq!(AnacronAdapter::new().kind(), TaskSourceKind::Anacron);
    }
}
