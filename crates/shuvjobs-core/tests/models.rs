use chrono::{TimeZone, Utc};
use shuvjobs_core::{ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus};
use std::time::Duration;

#[test]
fn task_source_kind_round_trips_to_str() {
    let all = [
        TaskSourceKind::Systemd,
        TaskSourceKind::Cron,
        TaskSourceKind::At,
        TaskSourceKind::Anacron,
        TaskSourceKind::Launchd,
    ];
    let labels: Vec<_> = all.iter().map(|k| k.as_str()).collect();
    // No two kinds should share a label.
    let mut sorted = labels.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len());
}

#[test]
fn scheduled_task_can_be_constructed_with_all_fields() {
    let t = ScheduledTask {
        id: "logrotate.timer".into(),
        name: "logrotate".into(),
        source: TaskSourceKind::Systemd,
        schedule: ScheduleType::Calendar("daily".into()),
        last_run: Some(Utc.with_ymd_and_hms(2026, 4, 10, 0, 0, 0).unwrap()),
        last_status: Some(TaskStatus::Success),
        last_duration: Some(Duration::from_secs(2)),
        next_run: Some(Utc.with_ymd_and_hms(2026, 4, 11, 0, 0, 0).unwrap()),
        command: "/usr/sbin/logrotate /etc/logrotate.conf".into(),
        location: None,
        enabled: None,
    };
    assert_eq!(t.source, TaskSourceKind::Systemd);
    assert!(matches!(t.schedule, ScheduleType::Calendar(_)));
}

#[test]
fn schedule_type_variants_are_distinct() {
    let cron = ScheduleType::Cron("*/5 * * * *".into());
    let interval = ScheduleType::Interval(Duration::from_secs(300));
    assert_ne!(cron, interval);
}

#[test]
fn task_status_failed_carries_message() {
    let s = TaskStatus::Failed("exit code 1".into());
    match s {
        TaskStatus::Failed(msg) => assert_eq!(msg, "exit code 1"),
        _ => panic!("expected Failed"),
    }
}
