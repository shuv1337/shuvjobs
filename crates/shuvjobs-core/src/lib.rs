//! Domain types for shuvjobs. OS-agnostic by design.

mod error;
pub mod export;
mod models;
mod source;
pub mod view;

pub use error::{Error, Result};
pub use models::{ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus};
pub use source::TaskSource;
