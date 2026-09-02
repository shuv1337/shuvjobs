//! [`Host`] for the machine we are running on.
//!
//! Two code paths, chosen per call by [`Privilege`]:
//!
//! * unprivileged (or already root) — `std::fs` and `sh -c`, so the
//!   common case never spawns a shell to read a file;
//! * root without being root — the pinned scripts from
//!   [`shuvjobs_core::host::shell`] under `sudo -n -- sh -c …`, byte for
//!   byte the same strings the SSH host sends, so the privileged local
//!   and remote paths cannot drift apart.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use chrono::{FixedOffset, Local};
use shuvjobs_core::host::shell::{
    exists_script, list_dir_script, mkdir_script, read_file_script, remove_file_script,
    write_file_script, ABSENT_EXIT,
};
use shuvjobs_core::host::{CmdOutput, Host, HostOs, Privilege, PrivilegePolicy};
use shuvjobs_core::{Error, Result};

use crate::process::{run_process, to_cmd_output};

/// Disambiguates concurrent atomic writes into the same directory from
/// this process; the pid covers other processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
pub struct LocalHost {
    policy: PrivilegePolicy,
    user: OnceLock<String>,
    uid: OnceLock<u32>,
    home: OnceLock<String>,
    offset: OnceLock<FixedOffset>,
}

impl LocalHost {
    /// `sudo` is the operator's `--sudo` flag. Whether we are already
    /// root is detected once, here, so no later call has to ask again.
    pub fn new(sudo: bool) -> Self {
        Self::with_policy(PrivilegePolicy {
            is_root: detect_root(),
            sudo,
        })
    }

    /// Construct with an explicit policy. Tests use this to exercise the
    /// sudo and refusal branches without being root.
    pub fn with_policy(policy: PrivilegePolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// True when file operations may go straight through `std::fs`.
    fn direct_fs(&self, privilege: Privilege) -> bool {
        privilege == Privilege::User || self.policy.is_root
    }

    /// Run one of the pinned shell scripts as root.
    fn script(&self, script: &str, stdin: Option<&[u8]>) -> Result<CmdOutput> {
        self.run(script, stdin, Privilege::Root)
    }

    /// Stdout of `cmd`, trimmed, or `None` if it could not be run or failed.
    fn probe(cmd: &str, args: &[&str]) -> Option<String> {
        let mut command = Command::new(cmd);
        command.args(args);
        let out = to_cmd_output(run_process(&mut command, None).ok()?);
        if !out.success() {
            return None;
        }
        let text = out.stdout_str().trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

fn detect_root() -> bool {
    match LocalHost::probe("id", &["-u"]) {
        Some(uid) => uid == "0",
        None => std::env::var("USER").is_ok_and(|u| u == "root"),
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Program and argv for `cmd` under `privilege`, or the refusal.
///
/// Mirrors [`shuvjobs_core::host::privileged_command`], but as an argv:
/// spawning `sudo` directly means the command text is never re-parsed by
/// an intermediate shell.
pub(crate) fn argv_for(
    cmd: &str,
    privilege: Privilege,
    policy: PrivilegePolicy,
) -> Result<(String, Vec<String>)> {
    let plain = || ("sh".to_string(), vec!["-c".to_string(), cmd.to_string()]);
    match privilege {
        Privilege::User => Ok(plain()),
        Privilege::Root if policy.is_root => Ok(plain()),
        Privilege::Root if policy.sudo => Ok((
            "sudo".to_string(),
            vec![
                "-n".to_string(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                cmd.to_string(),
            ],
        )),
        Privilege::Root => Err(Error::NeedsRoot {
            operation: cmd.to_string(),
        }),
    }
}

/// Map an `std::io::Error` from a file operation onto a host error,
/// keeping "you need root for this" distinct from a real I/O failure.
fn fs_error(err: std::io::Error, operation: String) -> Error {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        Error::NeedsRoot { operation }
    } else {
        Error::Io(err)
    }
}

/// Write `contents` next to `path` and rename over it, so a reader ever
/// only sees the whole old file or the whole new one.
#[cfg_attr(not(unix), allow(unused_variables))]
fn atomic_write(path: &str, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write as _;

    let target = Path::new(path);
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".shuvjobs.{}.{n}.tmp", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, target)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

impl Host for LocalHost {
    fn policy(&self) -> PrivilegePolicy {
        self.policy
    }

    fn label(&self) -> String {
        "local".to_string()
    }

    fn current_user(&self) -> Result<String> {
        if let Some(user) = self.user.get() {
            return Ok(user.clone());
        }
        let user = env_nonempty("USER")
            .or_else(|| env_nonempty("LOGNAME"))
            .or_else(|| Self::probe("id", &["-un"]))
            .ok_or_else(|| Error::Other("cannot determine the current user".into()))?;
        let _ = self.user.set(user.clone());
        Ok(user)
    }

    fn current_uid(&self) -> Result<u32> {
        if let Some(uid) = self.uid.get() {
            return Ok(*uid);
        }
        let uid = Self::probe("id", &["-u"])
            .and_then(|text| text.parse::<u32>().ok())
            .ok_or_else(|| Error::Other("cannot determine the current uid".into()))?;
        let _ = self.uid.set(uid);
        Ok(uid)
    }

    fn home_dir(&self) -> Result<String> {
        if let Some(home) = self.home.get() {
            return Ok(home.clone());
        }
        let home = env_nonempty("HOME").ok_or_else(|| {
            Error::Other("cannot determine the home directory: $HOME unset".into())
        })?;
        let _ = self.home.set(home.clone());
        Ok(home)
    }

    fn os(&self) -> Result<HostOs> {
        Ok(if cfg!(target_os = "linux") {
            HostOs::Linux
        } else if cfg!(target_os = "macos") {
            HostOs::MacOs
        } else {
            HostOs::Other
        })
    }

    fn utc_offset(&self) -> Result<FixedOffset> {
        if let Some(offset) = self.offset.get() {
            return Ok(*offset);
        }
        let offset = *Local::now().offset();
        let _ = self.offset.set(offset);
        Ok(offset)
    }

    fn run(&self, cmd: &str, stdin: Option<&[u8]>, privilege: Privilege) -> Result<CmdOutput> {
        let (program, args) = argv_for(cmd, privilege, self.policy)?;
        let mut command = Command::new(&program);
        command.args(&args);
        let output = run_process(&mut command, stdin).map_err(|e| Error::Command {
            command: cmd.to_string(),
            message: format!("failed to spawn {program}: {e}"),
        })?;
        Ok(to_cmd_output(output))
    }

    fn read_file(&self, path: &str, privilege: Privilege) -> Result<Option<Vec<u8>>> {
        if self.direct_fs(privilege) {
            return match std::fs::read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(fs_error(e, format!("read {path}"))),
            };
        }
        let out = self.script(&read_file_script(path), None)?;
        if out.success() {
            return Ok(Some(out.stdout));
        }
        if out.code == Some(ABSENT_EXIT) {
            return Ok(None);
        }
        Err(out
            .require_success(&format!("read {path}"))
            .expect_err("a non-zero exit cannot succeed"))
    }

    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: u32,
        privilege: Privilege,
    ) -> Result<()> {
        if self.direct_fs(privilege) {
            return atomic_write(path, contents, mode)
                .map_err(|e| fs_error(e, format!("write {path}")));
        }
        self.script(&write_file_script(path, mode), Some(contents))?
            .require_success(&format!("write {path}"))?;
        Ok(())
    }

    fn remove_file(&self, path: &str, privilege: Privilege) -> Result<bool> {
        if self.direct_fs(privilege) {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(fs_error(e, format!("remove {path}"))),
            };
        }
        let out = self.script(&remove_file_script(path), None)?;
        if out.success() {
            return Ok(true);
        }
        if out.code == Some(ABSENT_EXIT) {
            return Ok(false);
        }
        Err(out
            .require_success(&format!("remove {path}"))
            .expect_err("a non-zero exit cannot succeed"))
    }

    fn exists(&self, path: &str, privilege: Privilege) -> Result<bool> {
        if self.direct_fs(privilege) {
            return Ok(Path::new(path).exists());
        }
        let out = self.script(&exists_script(path), None)?;
        if out.success() {
            return Ok(true);
        }
        // `test -e` says "no" with exit 1 and nothing on stderr; anything
        // else (a sudo refusal, a missing shell) is a real failure.
        if out.code == Some(1) && out.stderr.trim().is_empty() {
            return Ok(false);
        }
        Err(out
            .require_success(&format!("check {path}"))
            .expect_err("a non-zero exit cannot succeed"))
    }

    fn list_dir(&self, path: &str, privilege: Privilege) -> Result<Vec<String>> {
        if self.direct_fs(privilege) {
            let entries = match std::fs::read_dir(path) {
                Ok(entries) => entries,
                // An absent directory holds no entries; callers that care
                // ask `exists` first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(fs_error(e, format!("list {path}"))),
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            return Ok(names);
        }
        let out = self.script(&list_dir_script(path), None)?;
        if out.code == Some(ABSENT_EXIT) {
            return Ok(Vec::new());
        }
        let listing = out.require_success(&format!("list {path}"))?;
        let mut names: Vec<String> = listing
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        names.sort();
        Ok(names)
    }

    fn create_dir_all(&self, path: &str, privilege: Privilege) -> Result<()> {
        if self.direct_fs(privilege) {
            return std::fs::create_dir_all(path).map_err(|e| fs_error(e, format!("mkdir {path}")));
        }
        self.script(&mkdir_script(path), None)?
            .require_success(&format!("mkdir {path}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(is_root: bool, sudo: bool) -> PrivilegePolicy {
        PrivilegePolicy { is_root, sudo }
    }

    #[test]
    fn argv_for_user_commands_uses_sh_c() {
        let (program, args) = argv_for("echo hi", Privilege::User, policy(false, false)).unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn argv_for_root_as_root_uses_sh_c() {
        let (program, args) = argv_for("echo hi", Privilege::Root, policy(true, false)).unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn argv_for_root_with_sudo_wraps_the_shell() {
        let (program, args) = argv_for("echo hi", Privilege::Root, policy(false, true)).unwrap();
        assert_eq!(program, "sudo");
        assert_eq!(args, vec!["-n", "--", "sh", "-c", "echo hi"]);
    }

    #[test]
    fn argv_for_root_without_sudo_refuses() {
        let err = argv_for("cat /etc/shadow", Privilege::Root, policy(false, false))
            .expect_err("must refuse");
        match err {
            Error::NeedsRoot { operation } => assert_eq!(operation, "cat /etc/shadow"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }
    }

    #[test]
    fn run_feeds_stdin_and_reports_exit_status() {
        let host = LocalHost::with_policy(policy(false, false));
        let out = host.run("cat", Some(b"piped\n"), Privilege::User).unwrap();
        assert_eq!(out.stdout_str(), "piped\n");

        let out = host.run("exit 4", None, Privilege::User).unwrap();
        assert_eq!(out.code, Some(4));
    }

    #[test]
    fn run_refuses_root_without_sudo_before_spawning() {
        let host = LocalHost::with_policy(policy(false, false));
        let err = host
            .run("id -u", None, Privilege::Root)
            .expect_err("must refuse");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
    }

    #[test]
    fn current_user_and_offset_are_available() {
        let host = LocalHost::with_policy(policy(false, false));
        assert!(!host.current_user().unwrap().is_empty());
        // Second call comes from the cache and must agree.
        assert_eq!(host.current_user().unwrap(), host.current_user().unwrap());
        assert_eq!(host.utc_offset().unwrap(), host.utc_offset().unwrap());
        assert!(host.os().is_ok());
    }

    /// Removes its directory however the test ends.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("shuvjobs-test-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_round_trip_through_the_direct_path() {
        let dir = TempDir::new("files");
        let host = LocalHost::with_policy(policy(false, false));
        let path = dir.path("job.conf");

        assert_eq!(host.read_file(&path, Privilege::User).unwrap(), None);
        assert!(!host.exists(&path, Privilege::User).unwrap());
        assert!(!host.remove_file(&path, Privilege::User).unwrap());

        host.write_file(&path, b"first\n", 0o600, Privilege::User)
            .unwrap();
        assert_eq!(
            host.read_to_string(&path, Privilege::User).unwrap(),
            Some("first\n".to_string())
        );
        // Overwriting an existing file must also work: the temp name is
        // fresh even though the target is not.
        host.write_file(&path, b"second\n", 0o600, Privilege::User)
            .unwrap();
        assert_eq!(
            host.read_to_string(&path, Privilege::User).unwrap(),
            Some("second\n".to_string())
        );

        assert!(host.exists(&path, Privilege::User).unwrap());
        assert_eq!(
            host.list_dir(&dir.0.to_string_lossy(), Privilege::User)
                .unwrap(),
            vec!["job.conf".to_string()]
        );
        assert!(host.remove_file(&path, Privilege::User).unwrap());
        assert!(!host.exists(&path, Privilege::User).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new("mode");
        let host = LocalHost::with_policy(policy(false, false));
        let path = dir.path("secret");
        host.write_file(&path, b"x\n", 0o600, Privilege::User)
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn list_dir_of_an_absent_directory_is_empty() {
        let host = LocalHost::with_policy(policy(false, false));
        let missing = std::env::temp_dir().join("shuvjobs-test-not-here");
        assert!(host
            .list_dir(&missing.to_string_lossy(), Privilege::User)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn create_dir_all_is_idempotent() {
        let dir = TempDir::new("mkdir");
        let host = LocalHost::with_policy(policy(false, false));
        let nested = dir.path("a/b/c");
        host.create_dir_all(&nested, Privilege::User).unwrap();
        host.create_dir_all(&nested, Privilege::User).unwrap();
        assert!(Path::new(&nested).is_dir());
    }
}
