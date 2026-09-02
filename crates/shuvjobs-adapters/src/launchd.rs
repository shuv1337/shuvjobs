//! launchd adapter — macOS only at runtime, parsers compile everywhere.
//!
//! `launchctl list` gives runtime state (PID + LastExitStatus); the
//! plist files under the standard `Library/Launch{Agents,Daemons}`
//! directories give the schedule and command. We merge them by `Label`.
//! `collect()` is a no-op (returns `Unavailable`) outside macOS.

use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Utc};
use plist::Value;
#[cfg(target_os = "macos")]
use shuvjobs_core::TaskStatus;
use shuvjobs_core::{Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind};

use crate::cron::compute_next_run;

#[derive(Debug, Default)]
pub struct LaunchdAdapter;

impl LaunchdAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Parse `launchctl list` output: header `PID\tStatus\tLabel` then
    /// tab-separated rows. PID/Status are `-` when not currently running.
    pub fn parse_launchctl_list(text: &str) -> Vec<LaunchctlEntry> {
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if i == 0 && line.starts_with("PID") {
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            let Some(pid) = parts.next() else { continue };
            let Some(status) = parts.next() else { continue };
            let Some(label) = parts.next() else { continue };
            out.push(LaunchctlEntry {
                pid: parse_dash_int(pid),
                last_exit_status: parse_dash_int(status),
                label: label.to_string(),
            });
        }
        out
    }

    /// Parse a launchd plist (XML or binary). Returns `None` for plists
    /// without a `Label` or without any of `StartInterval`/`StartCalendarInterval`
    /// (i.e. unscheduled jobs like `RunAtLoad`-only daemons).
    ///
    /// `path` is the plist's own location, reported as the task's
    /// `location`. `next_run` is resolved against the local wall clock;
    /// use [`Self::parse_plist_at`] to supply the clock explicitly
    /// (remote hosts).
    pub fn parse_plist(bytes: &[u8], path: &str) -> Result<Option<ScheduledTask>> {
        Self::parse_plist_at(bytes, path, Local::now())
    }

    /// [`Self::parse_plist`] with an explicit "now", in the timezone the
    /// job's host interprets its schedule in.
    ///
    /// `enabled` is `Some(false)` when the plist carries `Disabled=true`
    /// and `None` otherwise; the caller raises it to `Some(true)` for
    /// labels that `launchctl list` reports as loaded. (`launchctl
    /// print-disabled <domain>` would also settle the override database
    /// case; it is not consulted yet.)
    pub fn parse_plist_at<Tz: TimeZone>(
        bytes: &[u8],
        path: &str,
        now: DateTime<Tz>,
    ) -> Result<Option<ScheduledTask>> {
        let value: Value = plist::from_bytes(bytes).map_err(|e| Error::Parse {
            kind: "launchd plist".into(),
            message: e.to_string(),
        })?;
        let dict = match value {
            Value::Dictionary(d) => d,
            _ => return Ok(None),
        };
        let Some(label) = dict
            .get("Label")
            .and_then(|v| v.as_string())
            .map(str::to_string)
        else {
            return Ok(None);
        };

        let disabled = dict
            .get("Disabled")
            .and_then(|v| v.as_boolean())
            .unwrap_or(false);

        let command = match dict.get("ProgramArguments") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_string().map(str::to_string))
                .collect::<Vec<_>>()
                .join(" "),
            _ => dict
                .get("Program")
                .and_then(|v| v.as_string())
                .unwrap_or("")
                .to_string(),
        };

        // `StartInterval` jobs fire `n` seconds after the job was loaded,
        // and launchd does not publish that load time anywhere the plist
        // (or `launchctl list`) can see, so the next fire is unknowable
        // from static inspection. `next_run` stays `None` on purpose.
        let (schedule, next_run) = if let Some(secs) = dict
            .get("StartInterval")
            .and_then(|v| v.as_unsigned_integer())
        {
            (ScheduleType::Interval(Duration::from_secs(secs)), None)
        } else if let Some(cal) = dict.get("StartCalendarInterval") {
            let next = next_calendar_run(cal, now);
            (ScheduleType::Calendar(format_calendar_interval(cal)), next)
        } else {
            return Ok(None);
        };

        Ok(Some(ScheduledTask {
            id: label.clone(),
            name: label,
            source: TaskSourceKind::Launchd,
            schedule,
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run,
            command,
            location: Some(path.to_string()),
            enabled: disabled.then_some(false),
        }))
    }
}

impl TaskSource for LaunchdAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Launchd
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        #[cfg(target_os = "macos")]
        {
            use std::collections::HashMap;
            use std::fs;
            use std::path::PathBuf;
            use std::process::Command;

            // Runtime state from `launchctl list`.
            let listing = Command::new("launchctl")
                .arg("list")
                .output()
                .map_err(|_| Error::Unavailable("`launchctl` not available".into()))?;
            if !listing.status.success() {
                return Err(Error::Command {
                    command: "launchctl list".into(),
                    message: String::from_utf8_lossy(&listing.stderr).into_owned(),
                });
            }
            let runtime: HashMap<String, LaunchctlEntry> =
                Self::parse_launchctl_list(&String::from_utf8_lossy(&listing.stdout))
                    .into_iter()
                    .map(|e| (e.label.clone(), e))
                    .collect();

            let mut dirs: Vec<PathBuf> = vec![
                PathBuf::from("/Library/LaunchAgents"),
                PathBuf::from("/Library/LaunchDaemons"),
                PathBuf::from("/System/Library/LaunchAgents"),
                PathBuf::from("/System/Library/LaunchDaemons"),
            ];
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join("Library/LaunchAgents"));
            }

            let now = Local::now();
            let mut tasks = Vec::new();
            for dir in dirs {
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("plist") {
                        continue;
                    }
                    let Ok(bytes) = fs::read(&path) else { continue };
                    let path_str = path.to_string_lossy().into_owned();
                    match Self::parse_plist_at(&bytes, &path_str, now) {
                        Ok(Some(mut task)) => {
                            if let Some(rt) = runtime.get(&task.id) {
                                // Loaded in launchd's domain, whatever
                                // the plist's own `Disabled` key says.
                                task.enabled = Some(true);
                                task.last_status = rt.last_exit_status.map(|code| {
                                    if code == 0 {
                                        TaskStatus::Success
                                    } else {
                                        TaskStatus::Failed(format!("exit {code}"))
                                    }
                                });
                                if rt.pid.is_some() && task.last_status.is_none() {
                                    task.last_status = Some(TaskStatus::Running);
                                }
                            }
                            tasks.push(task);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("warning: failed to parse {}: {e}", path.display());
                        }
                    }
                }
            }
            Ok(tasks)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::Unavailable(
                "launchd is only available on macOS".into(),
            ))
        }
    }
}

/// One row of `launchctl list`. `None` fields are `-` in the source.
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchctlEntry {
    pub pid: Option<i64>,
    pub last_exit_status: Option<i64>,
    pub label: String,
}

fn parse_dash_int(s: &str) -> Option<i64> {
    if s == "-" {
        None
    } else {
        s.parse().ok()
    }
}

/// Convert one `StartCalendarInterval` dict into a 5-field cron
/// expression (`minute hour day-of-month month day-of-week`). A key that
/// launchd omits means "every value", i.e. `*`. launchd accepts both `0`
/// and `7` for Sunday; croner wants a single canonical value, so `7` is
/// folded to `0`. Returns `None` when a present key is not an integer;
/// out-of-range numbers are left for the cron parser to reject.
fn calendar_dict_to_cron(d: &plist::Dictionary) -> Option<String> {
    let field = |key: &str| -> Option<String> {
        match d.get(key) {
            None => Some("*".to_string()),
            Some(v) => {
                let n = v.as_signed_integer()?;
                let n = if key == "Weekday" && n == 7 { 0 } else { n };
                Some(n.to_string())
            }
        }
    };
    Some(format!(
        "{} {} {} {} {}",
        field("Minute")?,
        field("Hour")?,
        field("Day")?,
        field("Month")?,
        field("Weekday")?
    ))
}

/// Earliest next occurrence after `now` across every dict in a
/// `StartCalendarInterval` (launchd allows a single dict or an array of
/// them, and fires on the union). `None` when nothing parses — an
/// out-of-range or malformed entry drops silently rather than panicking.
///
/// launchd's own semantics when both `Day` and `Weekday` are set match
/// Vixie cron's OR of day-of-month and day-of-week, which is what
/// [`compute_next_run`] gives us.
fn next_calendar_run<Tz: TimeZone>(value: &Value, now: DateTime<Tz>) -> Option<DateTime<Utc>> {
    let dicts: Vec<&plist::Dictionary> = match value {
        Value::Dictionary(d) => vec![d],
        Value::Array(arr) => arr.iter().filter_map(|v| v.as_dictionary()).collect(),
        _ => return None,
    };
    dicts
        .into_iter()
        .filter_map(calendar_dict_to_cron)
        .filter_map(|expr| compute_next_run(&ScheduleType::Cron(expr), now.clone()))
        .min()
}

/// Render a `StartCalendarInterval` (dict or array of dicts) as
/// `Hour=9 Minute=0` style. Lossy but stable.
pub(crate) fn format_calendar_interval(value: &Value) -> String {
    fn dict_to_string(d: &plist::Dictionary) -> String {
        let mut parts = Vec::new();
        for key in ["Minute", "Hour", "Day", "Weekday", "Month"] {
            if let Some(v) = d.get(key).and_then(|v| v.as_signed_integer()) {
                parts.push(format!("{key}={v}"));
            }
        }
        parts.join(" ")
    }
    match value {
        Value::Dictionary(d) => dict_to_string(d),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| match v {
                Value::Dictionary(d) => Some(dict_to_string(d)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PLIST_PATH: &str = "/Library/LaunchAgents/com.example.heartbeat.plist";

    const LAUNCHCTL_LIST: &str = "PID\tStatus\tLabel\n\
                                   -\t0\tcom.apple.cloudphotosd\n\
                                   1234\t-\tcom.example.runner\n\
                                   -\t1\tcom.example.failed\n";

    #[test]
    fn parses_launchctl_list_rows() {
        let entries = LaunchdAdapter::parse_launchctl_list(LAUNCHCTL_LIST);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            LaunchctlEntry {
                pid: None,
                last_exit_status: Some(0),
                label: "com.apple.cloudphotosd".into()
            }
        );
        assert_eq!(entries[1].pid, Some(1234));
        assert_eq!(entries[2].last_exit_status, Some(1));
    }

    #[test]
    fn parses_header_only_launchctl_list_to_no_entries() {
        let entries = LaunchdAdapter::parse_launchctl_list("PID\tStatus\tLabel\n");
        assert!(entries.is_empty());
    }

    const INTERVAL_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.heartbeat</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/heartbeat</string>
        <string>--once</string>
    </array>
    <key>StartInterval</key>
    <integer>900</integer>
</dict>
</plist>"#;

    #[test]
    fn parse_plist_extracts_interval() {
        let task = LaunchdAdapter::parse_plist(INTERVAL_PLIST.as_bytes(), TEST_PLIST_PATH)
            .unwrap()
            .expect("interval plist should produce a task");
        assert_eq!(task.id, "com.example.heartbeat");
        assert_eq!(task.source, TaskSourceKind::Launchd);
        assert!(matches!(
            task.schedule,
            ScheduleType::Interval(d) if d == Duration::from_secs(900)
        ));
        assert_eq!(task.command, "/usr/local/bin/heartbeat --once");
    }

    const CALENDAR_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.morning</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/say</string>
        <string>good morning</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>9</integer>
        <key>Minute</key>
        <integer>30</integer>
    </dict>
</dict>
</plist>"#;

    #[test]
    fn parse_plist_extracts_calendar_interval() {
        let task = LaunchdAdapter::parse_plist(CALENDAR_PLIST.as_bytes(), TEST_PLIST_PATH)
            .unwrap()
            .unwrap();
        assert_eq!(task.id, "com.example.morning");
        match &task.schedule {
            ScheduleType::Calendar(s) => {
                assert!(s.contains("Hour=9"));
                assert!(s.contains("Minute=30"));
            }
            other => panic!("expected Calendar, got {other:?}"),
        }
        assert_eq!(task.command, "/usr/bin/say good morning");
    }

    // RunAtLoad-only daemon — no StartInterval or StartCalendarInterval.
    const RUN_AT_LOAD_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.daemon</string>
    <key>ProgramArguments</key>
    <array><string>/usr/local/bin/daemon</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>"#;

    #[test]
    fn parse_plist_records_the_plist_path_as_the_location() {
        let task = LaunchdAdapter::parse_plist(INTERVAL_PLIST.as_bytes(), TEST_PLIST_PATH)
            .unwrap()
            .unwrap();
        assert_eq!(task.location.as_deref(), Some(TEST_PLIST_PATH));
        // Nothing in the plist says otherwise and `launchctl list` has
        // not been consulted yet.
        assert_eq!(task.enabled, None);
    }

    const DISABLED_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.example.heartbeat</string>
    <key>ProgramArguments</key><array><string>/usr/local/bin/heartbeat</string></array>
    <key>StartInterval</key><integer>900</integer>
    <key>Disabled</key><true/>
</dict>
</plist>"#;

    #[test]
    fn parse_plist_reports_a_disabled_key_as_disabled() {
        let task = LaunchdAdapter::parse_plist(DISABLED_PLIST.as_bytes(), TEST_PLIST_PATH)
            .unwrap()
            .unwrap();
        assert_eq!(task.enabled, Some(false));
    }

    #[test]
    fn parse_plist_skips_unscheduled_jobs() {
        let parsed =
            LaunchdAdapter::parse_plist(RUN_AT_LOAD_PLIST.as_bytes(), TEST_PLIST_PATH).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn adapter_reports_launchd_kind() {
        assert_eq!(LaunchdAdapter::new().kind(), TaskSourceKind::Launchd);
    }

    // ---- StartCalendarInterval next_run ----

    /// Fixed local "now": 2026-04-14 01:30:00 -07:00 (a Tuesday).
    fn now_west7() -> DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::west_opt(7 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 4, 14, 1, 30, 0)
            .unwrap()
    }

    fn calendar_plist(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.example.job</string>
    <key>ProgramArguments</key><array><string>/bin/true</string></array>
    <key>StartCalendarInterval</key>
{body}
</dict>
</plist>"#
        )
    }

    fn parsed_at(body: &str) -> ScheduledTask {
        LaunchdAdapter::parse_plist_at(
            calendar_plist(body).as_bytes(),
            TEST_PLIST_PATH,
            now_west7(),
        )
        .unwrap()
        .expect("calendar plist should produce a task")
    }

    #[test]
    fn calendar_hour_and_minute_next_run_is_today_in_local_time() {
        let task = parsed_at(
            "    <dict>\n        <key>Hour</key><integer>9</integer>\n\
             \x20       <key>Minute</key><integer>30</integer>\n    </dict>",
        );
        assert_eq!(
            task.next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 14, 16, 30, 0).unwrap())
        );
    }

    #[test]
    fn calendar_weekday_zero_and_seven_both_mean_sunday() {
        let zero = parsed_at(
            "    <dict>\n        <key>Weekday</key><integer>0</integer>\n\
             \x20       <key>Hour</key><integer>3</integer>\n\
             \x20       <key>Minute</key><integer>0</integer>\n    </dict>",
        );
        let seven = parsed_at(
            "    <dict>\n        <key>Weekday</key><integer>7</integer>\n\
             \x20       <key>Hour</key><integer>3</integer>\n\
             \x20       <key>Minute</key><integer>0</integer>\n    </dict>",
        );
        // 2026-04-14 is a Tuesday; the next Sunday 03:00 -07:00 is
        // 2026-04-19 03:00 -07:00 == 2026-04-19 10:00 UTC.
        assert_eq!(
            zero.next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 19, 10, 0, 0).unwrap())
        );
        assert_eq!(zero.next_run, seven.next_run);
        // The rendered schedule text still reports launchd's own value.
        assert!(matches!(seven.schedule, ScheduleType::Calendar(ref s) if s.contains("Weekday=7")));
    }

    #[test]
    fn calendar_array_picks_the_earliest_occurrence() {
        let task = parsed_at(
            "    <array>\n\
             \x20       <dict><key>Hour</key><integer>23</integer>\
             <key>Minute</key><integer>0</integer></dict>\n\
             \x20       <dict><key>Hour</key><integer>6</integer>\
             <key>Minute</key><integer>15</integer></dict>\n\
             \x20   </array>",
        );
        // 06:15 -07:00 today beats 23:00 -07:00 today.
        assert_eq!(
            task.next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 14, 13, 15, 0).unwrap())
        );
        // Rendering of an array is unchanged: dicts joined by " | ".
        assert!(matches!(task.schedule, ScheduleType::Calendar(ref s) if s.contains(" | ")));
    }

    #[test]
    fn calendar_minute_only_runs_every_hour() {
        let task = parsed_at("    <dict><key>Minute</key><integer>45</integer></dict>");
        assert_eq!(
            task.next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 14, 8, 45, 0).unwrap())
        );
    }

    #[test]
    fn calendar_out_of_range_value_yields_no_next_run() {
        let task = parsed_at(
            "    <dict><key>Hour</key><integer>99</integer>\
             <key>Minute</key><integer>0</integer></dict>",
        );
        assert!(task.next_run.is_none());
        assert!(matches!(task.schedule, ScheduleType::Calendar(_)));
    }

    #[test]
    fn interval_jobs_have_no_next_run() {
        let task =
            LaunchdAdapter::parse_plist_at(INTERVAL_PLIST.as_bytes(), TEST_PLIST_PATH, now_west7())
                .unwrap()
                .unwrap();
        assert!(task.next_run.is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn collect_is_unavailable_off_macos() {
        let err = LaunchdAdapter::new().collect().unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)));
    }
}
