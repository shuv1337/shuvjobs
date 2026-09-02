//! Stable JSON wire format for `ScheduledTask`.
//!
//! `ScheduledTask`'s default serde derive emits Rust-shaped tags
//! (`{"Cron": "..."}`) and a struct-y `Duration`. `ExportTask` is the
//! flat shape we hand to `shuvjobs --json` consumers and the SSH bridge.
//! Round-trips losslessly.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportTask {
    pub id: String,
    pub name: String,
    pub source: TaskSourceKind,
    /// Cron expr, OnCalendar string, RFC3339 timestamp, or `"<n>s"` for interval.
    pub schedule: String,
    pub schedule_type: ScheduleKind,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_status: Option<StatusKind>,
    /// Failure message when `last_status == failed`, otherwise null.
    pub last_status_detail: Option<String>,
    pub last_duration_secs: Option<f64>,
    pub command: String,
    /// Backing file the job lives in; null when the source has none
    /// (per-user crontabs, `at`).
    #[serde(default)]
    pub location: Option<String>,
    /// Whether the job would run; null when unknown or not applicable.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    Cron,
    Interval,
    Calendar,
    Oneshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusKind {
    Success,
    Failed,
    Running,
}

impl From<&ScheduledTask> for ExportTask {
    fn from(t: &ScheduledTask) -> Self {
        let (schedule, schedule_type) = match &t.schedule {
            ScheduleType::Cron(expr) => (expr.clone(), ScheduleKind::Cron),
            ScheduleType::Calendar(expr) => (expr.clone(), ScheduleKind::Calendar),
            ScheduleType::Interval(d) => (format!("{}s", d.as_secs()), ScheduleKind::Interval),
            ScheduleType::OneShot(dt) => (dt.to_rfc3339(), ScheduleKind::Oneshot),
        };
        let (last_status, last_status_detail) = match &t.last_status {
            Some(TaskStatus::Success) => (Some(StatusKind::Success), None),
            Some(TaskStatus::Failed(msg)) => (Some(StatusKind::Failed), Some(msg.clone())),
            Some(TaskStatus::Running) => (Some(StatusKind::Running), None),
            None => (None, None),
        };
        ExportTask {
            id: t.id.clone(),
            name: t.name.clone(),
            source: t.source,
            schedule,
            schedule_type,
            last_run: t.last_run,
            next_run: t.next_run,
            last_status,
            last_status_detail,
            last_duration_secs: t.last_duration.map(|d| d.as_secs_f64()),
            command: t.command.clone(),
            location: t.location.clone(),
            enabled: t.enabled,
        }
    }
}

impl TryFrom<ExportTask> for ScheduledTask {
    type Error = Error;

    fn try_from(e: ExportTask) -> Result<Self> {
        let schedule = match e.schedule_type {
            ScheduleKind::Cron => ScheduleType::Cron(e.schedule),
            ScheduleKind::Calendar => ScheduleType::Calendar(e.schedule),
            ScheduleKind::Interval => {
                // Accept "<n>s" or bare digits.
                let trimmed = e.schedule.trim_end_matches('s');
                let secs: u64 = trimmed.parse().map_err(|_| Error::Parse {
                    kind: "export schedule".into(),
                    message: format!("interval not parseable: {}", e.schedule),
                })?;
                ScheduleType::Interval(Duration::from_secs(secs))
            }
            ScheduleKind::Oneshot => {
                let dt = DateTime::parse_from_rfc3339(&e.schedule)
                    .map_err(|err| Error::Parse {
                        kind: "export schedule".into(),
                        message: format!("oneshot not RFC3339: {err}"),
                    })?
                    .with_timezone(&Utc);
                ScheduleType::OneShot(dt)
            }
        };
        let last_status = match (e.last_status, e.last_status_detail) {
            (Some(StatusKind::Success), _) => Some(TaskStatus::Success),
            (Some(StatusKind::Failed), Some(msg)) => Some(TaskStatus::Failed(msg)),
            (Some(StatusKind::Failed), None) => Some(TaskStatus::Failed(String::new())),
            (Some(StatusKind::Running), _) => Some(TaskStatus::Running),
            (None, _) => None,
        };
        Ok(ScheduledTask {
            id: e.id,
            name: e.name,
            source: e.source,
            schedule,
            last_run: e.last_run,
            last_status,
            last_duration: e.last_duration_secs.map(Duration::from_secs_f64),
            next_run: e.next_run,
            command: e.command,
            location: e.location,
            enabled: e.enabled,
        })
    }
}

/// Pretty-print tasks as a JSON array.
pub fn serialize_tasks(tasks: &[ScheduledTask]) -> Result<String> {
    let exports: Vec<ExportTask> = tasks.iter().map(ExportTask::from).collect();
    serde_json::to_string_pretty(&exports).map_err(|e| Error::Parse {
        kind: "json export".into(),
        message: e.to_string(),
    })
}

pub fn deserialize_tasks(json: &str) -> Result<Vec<ScheduledTask>> {
    let exports: Vec<ExportTask> = serde_json::from_str(json).map_err(|e| Error::Parse {
        kind: "json import".into(),
        message: e.to_string(),
    })?;
    exports.into_iter().map(ScheduledTask::try_from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture_task() -> ScheduledTask {
        ScheduledTask {
            id: "logrotate.timer".into(),
            name: "logrotate".into(),
            source: TaskSourceKind::Systemd,
            schedule: ScheduleType::Calendar("*-*-* 00:00:00".into()),
            last_run: Some(Utc.with_ymd_and_hms(2026, 4, 7, 0, 0, 1).unwrap()),
            last_status: Some(TaskStatus::Success),
            last_duration: Some(Duration::from_millis(1200)),
            next_run: Some(Utc.with_ymd_and_hms(2026, 4, 11, 0, 0, 0).unwrap()),
            command: "/usr/sbin/logrotate /etc/logrotate.conf".into(),
            location: None,
            enabled: None,
        }
    }

    #[test]
    fn round_trips_calendar_task_through_json() {
        let task = fixture_task();
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back, vec![task]);
    }

    #[test]
    fn round_trips_cron_task() {
        let task = ScheduledTask {
            id: "etc-crontab:6".into(),
            name: "metrics".into(),
            source: TaskSourceKind::Cron,
            schedule: ScheduleType::Cron("*/5 * * * *".into()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "/usr/local/bin/metrics-flush".into(),
            location: None,
            enabled: None,
        };
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back[0], task);
    }

    #[test]
    fn round_trips_interval_task() {
        let task = ScheduledTask {
            id: "snapper-cleanup.timer".into(),
            name: "snapper-cleanup".into(),
            source: TaskSourceKind::Systemd,
            schedule: ScheduleType::Interval(Duration::from_secs(3600)),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "/usr/lib/snapper/systemd-helper --cleanup".into(),
            location: None,
            enabled: None,
        };
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back[0], task);
    }

    #[test]
    fn round_trips_oneshot_task() {
        let dt = Utc.with_ymd_and_hms(2026, 4, 14, 14, 0, 0).unwrap();
        let task = ScheduledTask {
            id: "at:12".into(),
            name: "at job 12".into(),
            source: TaskSourceKind::At,
            schedule: ScheduleType::OneShot(dt),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: Some(dt),
            command: "echo hello".into(),
            location: None,
            enabled: None,
        };
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back[0], task);
    }

    #[test]
    fn round_trips_failed_status_with_detail() {
        let mut task = fixture_task();
        task.last_status = Some(TaskStatus::Failed("exit-code".into()));
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back[0], task);
    }

    #[test]
    fn json_uses_lowercase_source_and_schedule_type() {
        let task = fixture_task();
        let json = serialize_tasks(&[task]).unwrap();
        assert!(json.contains("\"source\": \"systemd\""));
        assert!(json.contains("\"schedule_type\": \"calendar\""));
        assert!(json.contains("\"last_status\": \"success\""));
    }

    #[test]
    fn round_trips_location_and_enabled() {
        let mut task = fixture_task();
        task.location = Some("/etc/systemd/system/logrotate.timer".into());
        task.enabled = Some(false);
        let json = serialize_tasks(std::slice::from_ref(&task)).unwrap();
        let back = deserialize_tasks(&json).unwrap();
        assert_eq!(back[0], task);
    }

    #[test]
    fn json_contains_location_and_enabled_keys() {
        let mut task = fixture_task();
        task.location = Some("/etc/systemd/system/logrotate.timer".into());
        task.enabled = Some(true);
        let json = serialize_tasks(&[task]).unwrap();
        assert!(json.contains("\"location\": \"/etc/systemd/system/logrotate.timer\""));
        assert!(json.contains("\"enabled\": true"));
    }

    #[test]
    fn missing_location_and_enabled_keys_default_to_none() {
        let old = r#"[{
            "id":"at:12","name":"at job 12","source":"at","schedule":"2026-04-14T14:00:00+00:00",
            "schedule_type":"oneshot","last_run":null,"next_run":null,
            "last_status":null,"last_status_detail":null,
            "last_duration_secs":null,"command":"echo hello"
        }]"#;
        let tasks = deserialize_tasks(old).unwrap();
        assert_eq!(tasks[0].location, None);
        assert_eq!(tasks[0].enabled, None);
    }

    #[test]
    fn deserialize_surfaces_invalid_oneshot() {
        let bad = r#"[{
            "id":"x","name":"x","source":"at","schedule":"not-a-date",
            "schedule_type":"oneshot","last_run":null,"next_run":null,
            "last_status":null,"last_status_detail":null,
            "last_duration_secs":null,"command":""
        }]"#;
        let err = deserialize_tasks(bad).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }
}
