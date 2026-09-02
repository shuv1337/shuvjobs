//! Domain types for shuvjobs. OS-agnostic by design.

mod error;
pub mod export;
pub mod host;
pub mod manage;
mod models;
mod source;
pub mod view;

pub use error::{Error, Result};
pub use manage::{
    ApplyMode, Change, FailPolicy, JobScope, JobSpec, JobWriter, MutationOutcome, Op,
};
pub use models::{ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus};
pub use source::TaskSource;
