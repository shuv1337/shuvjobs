use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskSourceKind {
    Systemd,
    Cron,
    At,
    Anacron,
    Launchd,
}

impl TaskSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Cron => "cron",
            Self::At => "at",
            Self::Anacron => "anacron",
            Self::Launchd => "launchd",
        }
    }
}

impl std::fmt::Display for TaskSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a task is scheduled. Each variant keeps the source's native
/// representation rather than collapsing everything to a duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleType {
    Cron(String),
    Interval(Duration),
    OneShot(DateTime<Utc>),
    /// systemd `OnCalendar=` expression. Richer grammar than cron, so
    /// we hold it as text.
    Calendar(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Success,
    Failed(String),
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Stable id within `source`: unit name for systemd, `<file>:<line>`
    /// for cron, label for launchd, etc.
    pub id: String,
    pub name: String,
    pub source: TaskSourceKind,
    pub schedule: ScheduleType,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<TaskStatus>,
    pub last_duration: Option<Duration>,
    pub next_run: Option<DateTime<Utc>>,
    pub command: String,
    /// Backing file the job lives in: the crontab or `/etc/cron.d` file,
    /// the systemd unit's `FragmentPath`, the anacrontab, or the launchd
    /// plist path. `None` when there is no addressable file, as for a
    /// per-user crontab (only reachable through `crontab -l`) and for `at`.
    #[serde(default)]
    pub location: Option<String>,
    /// Whether the job would run if its schedule fired. `None` means
    /// unknown or not applicable for this source (`at` jobs, for
    /// instance, are always pending until they run).
    #[serde(default)]
    pub enabled: Option<bool>,
}
