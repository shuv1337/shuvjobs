use thiserror::Error;

use crate::TaskSourceKind;

#[derive(Debug, Error)]
pub enum Error {
    /// Source isn't available on this host (e.g. systemd on macOS).
    #[error("source unavailable: {0}")]
    Unavailable(String),

    #[error("command `{command}` failed: {message}")]
    Command { command: String, message: String },

    #[error("failed to parse {kind} output: {message}")]
    Parse { kind: String, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The operation can only be done as root and we are not root.
    #[error("{operation} needs root: re-run with --sudo (passwordless sudo) or as root")]
    NeedsRoot { operation: String },

    /// A required binary is missing on the host.
    #[error("command not found: {0}")]
    CommandNotFound(String),

    /// The source cannot express what was asked of it.
    /// The field is `source_kind`, not `source`: thiserror treats a field
    /// named `source` as the error's `std::error::Error::source()`.
    #[error("{source_kind} {id}: unsupported: {reason}")]
    Unsupported {
        source_kind: TaskSourceKind,
        id: String,
        reason: String,
    },

    /// The on-disk state moved under us, or the target already exists.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The request itself is malformed.
    #[error("invalid input: {0}")]
    Validation(String),

    /// The transport (SSH) failed, as distinct from the remote command failing.
    #[error("transport error: {0}")]
    Transport(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
