use crate::{Result, ScheduledTask, TaskSourceKind};

/// One scheduling subsystem we can enumerate (systemd, cron, ...).
///
/// `Ok(vec![])` means "available but empty"; `Err(Unavailable)` means
/// "not present on this host" and callers should skip silently.
pub trait TaskSource: Send + Sync {
    fn kind(&self) -> TaskSourceKind;
    fn collect(&self) -> Result<Vec<ScheduledTask>>;
}
