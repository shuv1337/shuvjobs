//! launchd adapter — macOS only at runtime, parsers compile everywhere.
//!
//! `launchctl list` gives runtime state (PID + LastExitStatus); the
//! plist files under the standard `Library/Launch{Agents,Daemons}`
//! directories give the schedule and command. We merge them by `Label`.
//! `collect()` is a no-op (returns `Unavailable`) outside macOS.

use std::time::Duration;

use plist::Value;
#[cfg(target_os = "macos")]
use shuvjobs_core::TaskStatus;
use shuvjobs_core::{Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind};

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
    pub fn parse_plist(bytes: &[u8]) -> Result<Option<ScheduledTask>> {
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

        let schedule = if let Some(secs) = dict
            .get("StartInterval")
            .and_then(|v| v.as_unsigned_integer())
        {
            ScheduleType::Interval(Duration::from_secs(secs))
        } else if let Some(cal) = dict.get("StartCalendarInterval") {
            ScheduleType::Calendar(format_calendar_interval(cal))
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
            next_run: None,
            command,
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
                    match Self::parse_plist(&bytes) {
                        Ok(Some(mut task)) => {
                            if let Some(rt) = runtime.get(&task.id) {
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

/// Render a `StartCalendarInterval` (dict or array of dicts) as
/// `Hour=9 Minute=0` style. Lossy but stable.
fn format_calendar_interval(value: &Value) -> String {
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
        let task = LaunchdAdapter::parse_plist(INTERVAL_PLIST.as_bytes())
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
        let task = LaunchdAdapter::parse_plist(CALENDAR_PLIST.as_bytes())
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
    fn parse_plist_skips_unscheduled_jobs() {
        let parsed = LaunchdAdapter::parse_plist(RUN_AT_LOAD_PLIST.as_bytes()).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn adapter_reports_launchd_kind() {
        assert_eq!(LaunchdAdapter::new().kind(), TaskSourceKind::Launchd);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn collect_is_unavailable_off_macos() {
        let err = LaunchdAdapter::new().collect().unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)));
    }
}
