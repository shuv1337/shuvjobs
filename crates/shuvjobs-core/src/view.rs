//! Filter, sort, and search operations over `ScheduledTask` slices.
//!
//! Pure functions — `shuvjobs-tui` composes them to drive its display state.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::{ScheduledTask, TaskSourceKind, TaskStatus};

/// Variants are listed in cycle order (the order `s` walks through in the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Default,
    NextRun,
    LastRun,
    Name,
    Status,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::NextRun,
            Self::NextRun => Self::LastRun,
            Self::LastRun => Self::Name,
            Self::Name => Self::Status,
            Self::Status => Self::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NextRun => "next run ↑",
            Self::LastRun => "last run ↓",
            Self::Name => "name",
            Self::Status => "status",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// `None` = all sources allowed.
    pub allowed_sources: Option<HashSet<TaskSourceKind>>,
    /// Case-insensitive substring match against `name` and `command`. Empty = no search.
    pub search: String,
}

impl Filter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn matches(&self, task: &ScheduledTask) -> bool {
        if let Some(allowed) = &self.allowed_sources {
            if !allowed.contains(&task.source) {
                return false;
            }
        }
        if !self.search.is_empty() {
            let needle = self.search.to_lowercase();
            let in_name = task.name.to_lowercase().contains(&needle);
            let in_cmd = task.command.to_lowercase().contains(&needle);
            if !in_name && !in_cmd {
                return false;
            }
        }
        true
    }
}

pub fn apply(tasks: &[ScheduledTask], filter: &Filter, sort: SortMode) -> Vec<ScheduledTask> {
    let mut out: Vec<ScheduledTask> = tasks
        .iter()
        .filter(|t| filter.matches(t))
        .cloned()
        .collect();
    sort_in_place(&mut out, sort);
    out
}

pub fn sort_in_place(tasks: &mut [ScheduledTask], mode: SortMode) {
    match mode {
        SortMode::Default => {
            tasks.sort_by(|a, b| {
                source_order(a.source)
                    .cmp(&source_order(b.source))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
        SortMode::NextRun => {
            tasks.sort_by(|a, b| option_dt_asc(a.next_run, b.next_run));
        }
        SortMode::LastRun => {
            tasks.sort_by(|a, b| option_dt_desc(a.last_run, b.last_run));
        }
        SortMode::Name => {
            tasks.sort_by_key(|task| task.name.to_lowercase());
        }
        SortMode::Status => {
            tasks.sort_by(|a, b| {
                status_priority(a.last_status.as_ref())
                    .cmp(&status_priority(b.last_status.as_ref()))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
    }
}

fn source_order(k: TaskSourceKind) -> u8 {
    match k {
        TaskSourceKind::Systemd => 0,
        TaskSourceKind::Cron => 1,
        TaskSourceKind::At => 2,
        TaskSourceKind::Anacron => 3,
        TaskSourceKind::Launchd => 4,
    }
}

// Failed > Running > Success > unknown.
fn status_priority(s: Option<&TaskStatus>) -> u8 {
    match s {
        Some(TaskStatus::Failed(_)) => 0,
        Some(TaskStatus::Running) => 1,
        Some(TaskStatus::Success) => 2,
        None => 3,
    }
}

fn option_dt_asc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn option_dt_desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScheduleType;
    use chrono::{TimeZone, Utc};

    fn t(name: &str, src: TaskSourceKind, command: &str) -> ScheduledTask {
        ScheduledTask {
            id: name.into(),
            name: name.into(),
            source: src,
            schedule: ScheduleType::Cron("* * * * *".into()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: command.into(),
        }
    }

    fn fixture() -> Vec<ScheduledTask> {
        vec![
            t("alpha", TaskSourceKind::Systemd, "/usr/bin/alpha"),
            t("bravo", TaskSourceKind::Cron, "/usr/bin/bravo --flag"),
            t("charlie", TaskSourceKind::At, "/bin/echo hi"),
            t("delta", TaskSourceKind::Anacron, "/etc/cron.daily/delta"),
            t("echo", TaskSourceKind::Launchd, "/usr/local/bin/echo"),
        ]
    }

    #[test]
    fn no_filter_passes_everything() {
        let tasks = fixture();
        let f = Filter::new();
        let out = apply(&tasks, &f, SortMode::Default);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn source_filter_keeps_only_allowed() {
        let tasks = fixture();
        let mut allowed = HashSet::new();
        allowed.insert(TaskSourceKind::Systemd);
        allowed.insert(TaskSourceKind::Cron);
        let f = Filter {
            allowed_sources: Some(allowed),
            search: String::new(),
        };
        let out = apply(&tasks, &f, SortMode::Default);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|t| matches!(t.source, TaskSourceKind::Systemd | TaskSourceKind::Cron)));
    }

    #[test]
    fn empty_allowed_set_filters_everything_out() {
        let tasks = fixture();
        let f = Filter {
            allowed_sources: Some(HashSet::new()),
            search: String::new(),
        };
        let out = apply(&tasks, &f, SortMode::Default);
        assert!(out.is_empty());
    }

    #[test]
    fn search_matches_name_case_insensitive() {
        let tasks = fixture();
        let f = Filter {
            allowed_sources: None,
            search: "ALPH".into(),
        };
        let out = apply(&tasks, &f, SortMode::Default);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "alpha");
    }

    #[test]
    fn search_also_matches_command() {
        let tasks = fixture();
        let f = Filter {
            allowed_sources: None,
            search: "--flag".into(),
        };
        let out = apply(&tasks, &f, SortMode::Default);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "bravo");
    }

    #[test]
    fn filter_and_search_compose() {
        let tasks = fixture();
        let mut allowed = HashSet::new();
        allowed.insert(TaskSourceKind::Cron);
        let f = Filter {
            allowed_sources: Some(allowed),
            search: "echo".into(),
        };
        // bravo's command doesn't match "echo"; charlie's command does
        // but is in `at`, not `cron`. Result: nothing.
        let out = apply(&tasks, &f, SortMode::Default);
        assert!(out.is_empty());
    }

    #[test]
    fn sort_default_is_source_then_name() {
        let mut tasks = vec![
            t("zeta", TaskSourceKind::Cron, ""),
            t("alpha", TaskSourceKind::Systemd, ""),
            t("beta", TaskSourceKind::Cron, ""),
        ];
        sort_in_place(&mut tasks, SortMode::Default);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn sort_next_run_ascending_with_none_last() {
        let make = |name: &str, n: Option<i64>| {
            let mut x = t(name, TaskSourceKind::Systemd, "");
            x.next_run = n.map(|s| Utc.timestamp_opt(s, 0).single().unwrap());
            x
        };
        let mut tasks = vec![
            make("late", Some(2_000)),
            make("never", None),
            make("soon", Some(1_000)),
        ];
        sort_in_place(&mut tasks, SortMode::NextRun);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["soon", "late", "never"]);
    }

    #[test]
    fn sort_last_run_descending_with_none_last() {
        let make = |name: &str, l: Option<i64>| {
            let mut x = t(name, TaskSourceKind::Systemd, "");
            x.last_run = l.map(|s| Utc.timestamp_opt(s, 0).single().unwrap());
            x
        };
        let mut tasks = vec![
            make("old", Some(1_000)),
            make("never", None),
            make("recent", Some(5_000)),
        ];
        sort_in_place(&mut tasks, SortMode::LastRun);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["recent", "old", "never"]);
    }

    #[test]
    fn sort_status_failed_first_then_running_then_success_then_none() {
        let make = |name: &str, s: Option<TaskStatus>| {
            let mut x = t(name, TaskSourceKind::Systemd, "");
            x.last_status = s;
            x
        };
        let mut tasks = vec![
            make("ok", Some(TaskStatus::Success)),
            make("none", None),
            make("dead", Some(TaskStatus::Failed("x".into()))),
            make("running", Some(TaskStatus::Running)),
        ];
        sort_in_place(&mut tasks, SortMode::Status);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["dead", "running", "ok", "none"]);
    }

    #[test]
    fn sort_name_is_alphabetical_case_insensitive() {
        let mut tasks = vec![
            t("Charlie", TaskSourceKind::Systemd, ""),
            t("alpha", TaskSourceKind::Systemd, ""),
            t("BRAVO", TaskSourceKind::Systemd, ""),
        ];
        sort_in_place(&mut tasks, SortMode::Name);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "BRAVO", "Charlie"]);
    }

    #[test]
    fn sort_mode_cycles_through_all_variants() {
        let mut m = SortMode::Default;
        let mut seen = vec![m];
        for _ in 0..4 {
            m = m.next();
            seen.push(m);
        }
        assert_eq!(
            seen,
            vec![
                SortMode::Default,
                SortMode::NextRun,
                SortMode::LastRun,
                SortMode::Name,
                SortMode::Status,
            ]
        );
        // One more cycle returns home.
        assert_eq!(m.next(), SortMode::Default);
    }
}
