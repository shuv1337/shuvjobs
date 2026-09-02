//! The single execution seam between the domain logic and a machine.
//!
//! Everything that touches a process or a file goes through [`Host`], so
//! readers and writers are identical whether the machine is this one or
//! one at the far end of an SSH multiplex master, and so tests can drive
//! the whole stack against an in-memory [`fake::FakeHost`].
//!
//! Privilege is per call, not per host: a caller states what a given
//! operation needs ([`Privilege`]) and the host's [`PrivilegePolicy`]
//! decides whether that is free (already root), rendered as
//! `sudo -n -- …`, or refused up front with [`Error::NeedsRoot`].

use std::borrow::Cow;

use chrono::FixedOffset;

use crate::{Error, Result};

pub mod shell;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeHost;

/// What a single operation needs from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// Runs as whoever we already are.
    User,
    /// Must run as root (another user's crontab, `/etc/cron.d`, system units).
    Root,
}

/// How a host is allowed to satisfy [`Privilege::Root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrivilegePolicy {
    /// We are already uid 0, so root operations need no wrapper.
    pub is_root: bool,
    /// The operator passed `--sudo`, so root operations may be wrapped in
    /// `sudo -n --` (non-interactive: it fails rather than prompting).
    pub sudo: bool,
}

/// Which OS the host runs, as far as the schedulers care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    MacOs,
    Other,
}

/// The result of a command that actually ran. A non-zero exit is a
/// successful *invocation* — only spawn/transport failures are `Err`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Fragments that mean "sudo refused", in lowercase. `sudo -n` prints one
/// of these instead of prompting, and none of them are the command's own
/// failure, so they map to [`Error::NeedsRoot`] rather than a command error.
const SUDO_REFUSAL_PATTERNS: &[&str] = &[
    "a password is required",
    "password is required",
    "no tty present",
    "must have a tty",
    "sudo: command not found",
    "sudo: not found",
];

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Turn a completed command into its stdout, or into the error that
    /// best explains the exit status.
    pub fn require_success(self, command: &str) -> Result<String> {
        if self.success() {
            return Ok(self.stdout_str().into_owned());
        }
        if self.code == Some(127) {
            return Err(Error::CommandNotFound(command.to_string()));
        }
        let lowered = self.stderr.to_ascii_lowercase();
        if SUDO_REFUSAL_PATTERNS.iter().any(|p| lowered.contains(p)) {
            return Err(Error::NeedsRoot {
                operation: command.to_string(),
            });
        }
        let stderr = self.stderr.trim();
        let message = if stderr.is_empty() {
            match self.code {
                Some(code) => format!("exit status {code}"),
                None => "killed by signal".to_string(),
            }
        } else {
            stderr.to_string()
        };
        Err(Error::Command {
            command: command.to_string(),
            message,
        })
    }
}

/// A machine we can run commands on and read and write files on.
pub trait Host: Send + Sync {
    fn policy(&self) -> PrivilegePolicy;

    /// Human label for messages and backup paths: `local`, `alice@host`.
    fn label(&self) -> String;

    fn current_user(&self) -> Result<String>;
    fn current_uid(&self) -> Result<u32>;
    fn home_dir(&self) -> Result<String>;
    fn os(&self) -> Result<HostOs>;
    fn utc_offset(&self) -> Result<FixedOffset>;

    /// Run `cmd` through the host's shell, optionally feeding `stdin`.
    ///
    /// A non-zero exit is `Ok(CmdOutput)`; only a spawn or transport
    /// failure is `Err`.
    fn run(&self, cmd: &str, stdin: Option<&[u8]>, privilege: Privilege) -> Result<CmdOutput>;

    /// `Ok(None)` when the file does not exist.
    fn read_file(&self, path: &str, privilege: Privilege) -> Result<Option<Vec<u8>>>;

    /// Atomic within the target directory: write a temp file, chmod, rename.
    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: u32,
        privilege: Privilege,
    ) -> Result<()>;

    /// `Ok(false)` when the file was already absent.
    fn remove_file(&self, path: &str, privilege: Privilege) -> Result<bool>;

    fn exists(&self, path: &str, privilege: Privilege) -> Result<bool>;

    /// Direct children of `path` by name, without `.` and `..`.
    fn list_dir(&self, path: &str, privilege: Privilege) -> Result<Vec<String>>;

    fn create_dir_all(&self, path: &str, privilege: Privilege) -> Result<()>;

    /// Lossy UTF-8 view of [`Host::read_file`]; config files are text.
    fn read_to_string(&self, path: &str, privilege: Privilege) -> Result<Option<String>> {
        Ok(self
            .read_file(path, privilege)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// Render `cmd` for the privilege it needs under `policy`, or refuse.
///
/// `operation` is what the caller was trying to do, and is what the
/// operator sees in the [`Error::NeedsRoot`] message.
pub fn privileged_command<'a>(
    cmd: &'a str,
    privilege: Privilege,
    policy: PrivilegePolicy,
    operation: &str,
) -> Result<Cow<'a, str>> {
    match privilege {
        Privilege::User => Ok(Cow::Borrowed(cmd)),
        Privilege::Root if policy.is_root => Ok(Cow::Borrowed(cmd)),
        Privilege::Root if policy.sudo => Ok(Cow::Owned(format!("sudo -n -- {cmd}"))),
        Privilege::Root => Err(Error::NeedsRoot {
            operation: operation.to_string(),
        }),
    }
}

/// Longest command text kept in a [`run_operation`] description. Long
/// enough to name the program and its arguments, short enough that a
/// heredoc-sized script does not become the error message.
const RUN_OPERATION_LIMIT: usize = 60;

/// The plain-words description of running `cmd`, for the operator-facing
/// half of [`Error::NeedsRoot`]. The shell script a host would actually
/// send is not an answer to "what were you trying to do", so it is
/// summarized: `run systemctl enable --now 'x.timer'`.
pub fn run_operation(cmd: &str) -> String {
    let cmd = cmd.trim();
    let mut out = String::from("run ");
    if cmd.chars().count() <= RUN_OPERATION_LIMIT {
        out.push_str(cmd);
    } else {
        out.extend(cmd.chars().take(RUN_OPERATION_LIMIT));
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stderr: &str) -> CmdOutput {
        CmdOutput {
            code: Some(code),
            stdout: b"hello\n".to_vec(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn privileged_command_leaves_user_commands_alone() {
        let policy = PrivilegePolicy::default();
        let rendered = privileged_command("ls", Privilege::User, policy, "list").unwrap();
        assert_eq!(rendered, "ls");
    }

    #[test]
    fn privileged_command_leaves_root_commands_alone_when_already_root() {
        let policy = PrivilegePolicy {
            is_root: true,
            sudo: false,
        };
        let rendered = privileged_command("ls", Privilege::Root, policy, "list").unwrap();
        assert_eq!(rendered, "ls");
    }

    #[test]
    fn privileged_command_wraps_root_commands_in_sudo() {
        let policy = PrivilegePolicy {
            is_root: false,
            sudo: true,
        };
        let rendered = privileged_command("ls", Privilege::Root, policy, "list").unwrap();
        assert_eq!(rendered, "sudo -n -- ls");
    }

    #[test]
    fn privileged_command_refuses_root_without_sudo() {
        let policy = PrivilegePolicy::default();
        let err = privileged_command("ls", Privilege::Root, policy, "write /etc/cron.d/x")
            .expect_err("must refuse");
        match err {
            Error::NeedsRoot { operation } => assert_eq!(operation, "write /etc/cron.d/x"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }
    }

    #[test]
    fn run_operation_names_the_command_in_plain_words() {
        assert_eq!(
            run_operation("systemctl enable --now 'x.timer'"),
            "run systemctl enable --now 'x.timer'"
        );
        // Exactly at the limit: no ellipsis.
        let sixty = "a".repeat(60);
        assert_eq!(run_operation(&sixty), format!("run {sixty}"));
        // Over it: truncated, and the reader can tell.
        let long = "b".repeat(200);
        let described = run_operation(&long);
        assert_eq!(described, format!("run {}...", "b".repeat(60)));
        assert!(described.len() < 70);
    }

    #[test]
    fn require_success_returns_stdout_on_zero_exit() {
        assert_eq!(out(0, "").require_success("cat f").unwrap(), "hello\n");
    }

    #[test]
    fn require_success_maps_127_to_command_not_found() {
        let err = out(127, "sh: crontab: not found")
            .require_success("crontab -l")
            .expect_err("must fail");
        match err {
            Error::CommandNotFound(cmd) => assert_eq!(cmd, "crontab -l"),
            other => panic!("expected CommandNotFound, got {other:?}"),
        }
    }

    #[test]
    fn require_success_maps_sudo_password_prompt_to_needs_root() {
        let err = out(1, "sudo: a password is required\n")
            .require_success("sudo -n -- cat /etc/shadow")
            .expect_err("must fail");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
    }

    #[test]
    fn require_success_maps_sudo_tty_refusal_to_needs_root() {
        let err = out(1, "sudo: sorry, you must have a tty to run sudo\n")
            .require_success("sudo -n -- cat /etc/shadow")
            .expect_err("must fail");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
    }

    #[test]
    fn require_success_maps_other_failures_to_command() {
        let err = out(2, "cat: f: No such file or directory\n")
            .require_success("cat f")
            .expect_err("must fail");
        match err {
            Error::Command { command, message } => {
                assert_eq!(command, "cat f");
                assert_eq!(message, "cat: f: No such file or directory");
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn require_success_falls_back_to_the_exit_status_without_stderr() {
        let err = out(3, "   ")
            .require_success("cat f")
            .expect_err("must fail");
        match err {
            Error::Command { message, .. } => assert_eq!(message, "exit status 3"),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn stdout_str_is_lossy() {
        let output = CmdOutput {
            code: Some(0),
            stdout: vec![0xff, b'a'],
            stderr: String::new(),
        };
        assert!(output.stdout_str().ends_with('a'));
        assert!(output.success());
    }
}
