use thiserror::Error;

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

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
