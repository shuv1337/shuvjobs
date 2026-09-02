//! systemd `*.timer` adapter.
//!
//! `systemctl list-timers` gives us the next/last instants and the bound
//! service. We then `systemctl show` each timer for `OnCalendar=` /
//! `OnUnitActiveSec=` / `Result=` and the bound service for `ExecStart=`.

use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use shuvjobs_core::{
    Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind, TaskStatus,
};

#[derive(Debug, Default)]
pub struct SystemdAdapter;

impl SystemdAdapter {
    pub fn new() -> Self {
        Self
    }

    pub fn parse_list_timers(json: &str) -> Result<Vec<ScheduledTask>> {
        // `next`/`last` come back as either microsecond integers or
        // `null` (Ubuntu's `apport-autoreport.timer` is the canonical
        // null case). `left`/`passed` are unused but still optional.
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            next: Option<i64>,
            #[serde(default)]
            #[allow(dead_code)]
            left: Option<i64>,
            #[serde(default)]
            last: Option<i64>,
            #[serde(default)]
            #[allow(dead_code)]
            passed: Option<i64>,
            unit: String,
            #[serde(default)]
            activates: String,
        }

        let rows: Vec<Row> = serde_json::from_str(json).map_err(|e| Error::Parse {
            kind: "systemctl list-timers".into(),
            message: e.to_string(),
        })?;

        let tasks = rows
            .into_iter()
            .map(|r| ScheduledTask {
                id: r.unit.clone(),
                name: trim_unit_suffix(&r.unit).to_string(),
                source: TaskSourceKind::Systemd,
                // Filled in later by parse_show_timer.
                schedule: ScheduleType::Calendar(String::new()),
                last_run: micros_to_dt(r.last.unwrap_or(0)),
                last_status: None,
                last_duration: None,
                next_run: micros_to_dt(r.next.unwrap_or(0)),
                command: r.activates,
            })
            .collect();
        Ok(tasks)
    }

    /// Parse `systemctl show <timer> --property=TimersCalendar,TimersMonotonic,Result`.
    pub fn parse_show_timer(text: &str) -> ShowTimer {
        let mut out = ShowTimer::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                // value: { OnCalendar=*-*-* 00:00:00 ; next_elapse=... }
                "TimersCalendar" => {
                    if let Some(expr) = extract_property_subfield(value, "OnCalendar=") {
                        if out.on_calendar.is_none() {
                            out.on_calendar = Some(expr);
                        }
                    }
                }
                // value: { OnBootSec=15min ; next_elapse=... } — any On*Sec key.
                "TimersMonotonic" => {
                    if let Some(raw) = extract_monotonic_value(value) {
                        if out.on_interval.is_none() {
                            out.on_interval = parse_systemd_duration(&raw);
                        }
                    }
                }
                "Result" if !value.is_empty() => {
                    out.result = Some(value.to_string());
                }
                _ => {}
            }
        }
        out
    }

    /// Parse `systemctl show <service> --property=ExecStart`, returning the argv portion.
    pub fn parse_show_service(text: &str) -> ShowService {
        let mut out = ShowService::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key == "ExecStart" {
                // value: { path=...; argv[]=... ; ignore_errors=no ; ... }
                if let Some(argv) = extract_property_subfield(value, "argv[]=") {
                    if out.exec_start.is_none() {
                        out.exec_start = Some(argv);
                    }
                }
            }
        }
        out
    }
}

impl TaskSource for SystemdAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Systemd
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        let probe = Command::new("systemctl").arg("--version").output();
        match probe {
            Ok(o) if o.status.success() => {}
            _ => {
                return Err(Error::Unavailable(
                    "`systemctl` not available on this host".into(),
                ))
            }
        }

        let listing = Command::new("systemctl")
            .args(["list-timers", "--all", "--output=json", "--no-pager"])
            .output()
            .map_err(|e| Error::Command {
                command: "systemctl list-timers".into(),
                message: e.to_string(),
            })?;
        if !listing.status.success() {
            return Err(Error::Command {
                command: "systemctl list-timers".into(),
                message: String::from_utf8_lossy(&listing.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&listing.stdout);
        let mut tasks = Self::parse_list_timers(&stdout)?;

        for task in &mut tasks {
            // Enrich with timer-side schedule + last result.
            if let Some(text) = run_show(&task.id, &["TimersCalendar", "TimersMonotonic", "Result"])
            {
                let timer = Self::parse_show_timer(&text);
                if let Some(expr) = timer.on_calendar {
                    task.schedule = ScheduleType::Calendar(expr);
                } else if let Some(d) = timer.on_interval {
                    task.schedule = ScheduleType::Interval(d);
                }
                task.last_status = timer.result.map(map_result);
            }

            // Enrich with the bound service's command line.
            let service = task.command.clone();
            if !service.is_empty() {
                if let Some(text) = run_show(&service, &["ExecStart"]) {
                    let svc = Self::parse_show_service(&text);
                    if let Some(cmd) = svc.exec_start {
                        task.command = cmd;
                    }
                }
            }
        }

        Ok(tasks)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShowTimer {
    pub on_calendar: Option<String>,
    pub on_interval: Option<Duration>,
    pub result: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShowService {
    pub exec_start: Option<String>,
}

fn run_show(unit: &str, properties: &[&str]) -> Option<String> {
    let prop_arg = format!("--property={}", properties.join(","));
    let out = Command::new("systemctl")
        .args(["show", unit, &prop_arg, "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn map_result(result: String) -> TaskStatus {
    match result.as_str() {
        "success" => TaskStatus::Success,
        other => TaskStatus::Failed(other.to_string()),
    }
}

fn micros_to_dt(us: i64) -> Option<DateTime<Utc>> {
    if us <= 0 {
        return None;
    }
    let secs = us / 1_000_000;
    let nanos = ((us % 1_000_000) * 1000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

fn trim_unit_suffix(unit: &str) -> &str {
    unit.strip_suffix(".timer").unwrap_or(unit)
}

/// Pull a `key=value` subfield out of a systemd record like
/// `{ path=...; argv[]=...; ignore_errors=no }`. Value runs up to the next ` ; `.
fn extract_property_subfield(record: &str, key: &str) -> Option<String> {
    let trimmed = record.trim().trim_start_matches('{').trim_end_matches('}');
    let pos = trimmed.find(key)?;
    let after = &trimmed[pos + key.len()..];
    let end = after.find(" ; ").unwrap_or(after.len());
    let value = after[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn extract_monotonic_value(record: &str) -> Option<String> {
    let trimmed = record.trim().trim_start_matches('{').trim_end_matches('}');
    let segment = trimmed.split(" ; ").next()?.trim();
    let (_, value) = segment.split_once('=')?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// systemd duration: `15min`, `1h`, `30s`, `1h30min`, `500ms`.
fn parse_systemd_duration(s: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut digits = String::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(c);
            chars.next();
            continue;
        }
        if !digits.is_empty() {
            let mut unit = String::new();
            while let Some(&u) = chars.peek() {
                if u.is_ascii_digit() {
                    break;
                }
                unit.push(u);
                chars.next();
            }
            let n: u64 = digits.parse().ok()?;
            digits.clear();
            let factor = match unit.trim() {
                "ms" => return Some(total + Duration::from_millis(n)),
                "s" | "sec" | "secs" | "second" | "seconds" => 1,
                "m" | "min" | "mins" | "minute" | "minutes" => 60,
                "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
                "d" | "day" | "days" => 86_400,
                "w" | "week" | "weeks" => 7 * 86_400,
                _ => return None,
            };
            total += Duration::from_secs(n * factor);
        } else {
            return None;
        }
    }
    if digits.is_empty() && total > Duration::ZERO {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Three rows covering the shapes systemctl actually produces:
    //   - snapper-cleanup: integer `next`, `last: 0` (never run)
    //   - logrotate: integer `next` and `last`
    //   - apport-autoreport: `next: null`, `last: 0` — Ubuntu's
    //     canonical "no scheduled elapse" shape that broke us once.
    const LIST_TIMERS_FIXTURE: &str = r#"[
        {"next":1775916894043109,"left":1775916894043109,"last":0,"passed":0,"unit":"snapper-cleanup.timer","activates":"snapper-cleanup.service"},
        {"next":1775919186922121,"left":1775919186922121,"last":1775499594111976,"passed":0,"unit":"logrotate.timer","activates":"logrotate.service"},
        {"next":null,"left":null,"last":0,"passed":0,"unit":"apport-autoreport.timer","activates":"apport-autoreport.service"}
    ]"#;

    #[test]
    fn parse_list_timers_extracts_units_and_timestamps() {
        let tasks = SystemdAdapter::parse_list_timers(LIST_TIMERS_FIXTURE).unwrap();
        assert_eq!(tasks.len(), 3);

        let snapper = &tasks[0];
        assert_eq!(snapper.id, "snapper-cleanup.timer");
        assert_eq!(snapper.name, "snapper-cleanup");
        assert_eq!(snapper.source, TaskSourceKind::Systemd);
        assert!(snapper.last_run.is_none());
        assert!(snapper.next_run.is_some());
        assert_eq!(snapper.command, "snapper-cleanup.service");

        let logrotate = &tasks[1];
        assert!(logrotate.last_run.is_some());
        assert_eq!(logrotate.command, "logrotate.service");

        let apport = &tasks[2];
        assert_eq!(apport.id, "apport-autoreport.timer");
        assert!(apport.next_run.is_none());
        assert!(apport.last_run.is_none());
    }

    #[test]
    fn parse_list_timers_inactive_next_is_none() {
        let json = r#"[{"next":0,"left":0,"last":0,"passed":0,"unit":"foo.timer","activates":"foo.service"}]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert!(tasks[0].next_run.is_none());
        assert!(tasks[0].last_run.is_none());
    }

    /// Regression: newer `systemctl list-timers --output=json` returns
    /// JSON `null` (not `0`) for unset elapse times. Older shuvjobs versions
    /// declared the field as `i64` and panicked at deserialization.
    #[test]
    fn parse_list_timers_handles_null_next_and_last() {
        let json = r#"[{"next":null,"left":null,"last":null,"passed":null,"unit":"foo.timer","activates":"foo.service"}]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].next_run.is_none());
        assert!(tasks[0].last_run.is_none());
        assert_eq!(tasks[0].id, "foo.timer");
    }

    #[test]
    fn parse_list_timers_handles_mixed_null_and_int() {
        let json = r#"[
            {"next":1775919186922121,"left":null,"last":null,"passed":null,"unit":"never-run.timer","activates":"never-run.service"},
            {"next":1775919186922121,"left":null,"last":1775499594111976,"passed":null,"unit":"once.timer","activates":"once.service"}
        ]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].next_run.is_some());
        assert!(tasks[0].last_run.is_none());
        assert!(tasks[1].next_run.is_some());
        assert!(tasks[1].last_run.is_some());
    }

    #[test]
    fn parse_list_timers_returns_empty_for_empty_array() {
        let tasks = SystemdAdapter::parse_list_timers("[]").unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_list_timers_surfaces_parse_errors() {
        let err = SystemdAdapter::parse_list_timers("not json").unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    const SHOW_TIMER_SUCCESS: &str =
        "TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Sat 2026-04-11 17:04:57 +03 }\n\
                                       LastTriggerUSec=Mon 2026-04-06 21:19:54 +03\n\
                                       Result=success\n";

    #[test]
    fn parse_show_timer_extracts_oncalendar_and_success() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_SUCCESS);
        assert_eq!(parsed.on_calendar.as_deref(), Some("*-*-* 00:00:00"));
        assert!(parsed.on_interval.is_none());
        assert_eq!(parsed.result.as_deref(), Some("success"));
    }

    const SHOW_TIMER_FAILED: &str =
        "TimersCalendar={ OnCalendar=daily ; next_elapse=Sat 2026-04-11 00:00:00 +03 }\n\
                                      Result=failed\n";

    #[test]
    fn parse_show_timer_maps_failed_result() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_FAILED);
        assert_eq!(parsed.result.as_deref(), Some("failed"));
        let mapped = map_result(parsed.result.unwrap());
        assert!(matches!(mapped, TaskStatus::Failed(ref s) if s == "failed"));
    }

    // Interval timer (no calendar entry, OnBootSec/OnUnitActiveSec only).
    const SHOW_TIMER_INTERVAL: &str = "TimersCalendar=\n\
                                        TimersMonotonic={ OnBootSec=15min ; next_elapse=4h 12min left }\n\
                                        Result=success\n";

    #[test]
    fn parse_show_timer_extracts_interval() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_INTERVAL);
        assert!(parsed.on_calendar.is_none());
        assert_eq!(parsed.on_interval, Some(Duration::from_secs(15 * 60)));
    }

    const SHOW_SERVICE_FIXTURE: &str = "ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";

    #[test]
    fn parse_show_service_pulls_argv() {
        let parsed = SystemdAdapter::parse_show_service(SHOW_SERVICE_FIXTURE);
        assert_eq!(
            parsed.exec_start.as_deref(),
            Some("/usr/sbin/logrotate /etc/logrotate.conf")
        );
    }

    #[test]
    fn parse_systemd_duration_handles_common_forms() {
        assert_eq!(
            parse_systemd_duration("15min"),
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            parse_systemd_duration("1h"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(parse_systemd_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_systemd_duration("1h30min"),
            Some(Duration::from_secs(5400))
        );
        assert_eq!(parse_systemd_duration("garbage"), None);
    }

    #[test]
    fn adapter_reports_systemd_kind() {
        assert_eq!(SystemdAdapter::new().kind(), TaskSourceKind::Systemd);
    }
}
