//! `TaskSource` implementations for each scheduling subsystem.
//!
//! Every adapter exposes pure `parse_*` functions over `&str` so the
//! parser logic can be unit-tested against captured fixture output.

pub mod anacron;
pub mod at;
pub mod cron;
pub mod launchd;
pub mod systemd;

pub use anacron::AnacronAdapter;
pub use at::AtAdapter;
pub use cron::CronAdapter;
pub use launchd::LaunchdAdapter;
pub use systemd::SystemdAdapter;
