//! An in-memory [`Host`] for tests.
//!
//! Writers are tested by asserting the exact commands they issue and the
//! exact file contents they leave behind, so `FakeHost` records the
//! *unwrapped* command: a test asserts
//! `("systemctl enable --now x.timer", Privilege::Root)` regardless of
//! whether the real host would have prefixed `sudo -n --`. The sudo
//! rendering itself is pinned separately, in `privileged_command`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::FixedOffset;

use super::{privileged_command, CmdOutput, Host, HostOs, Privilege, PrivilegePolicy};
use crate::{Error, Result};

fn output(code: i32, stdout: &str, stderr: &str) -> CmdOutput {
    CmdOutput {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.to_string(),
    }
}

/// One recorded [`Host::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub cmd: String,
    pub stdin: Option<Vec<u8>>,
    pub privilege: Privilege,
}

#[derive(Debug, Default)]
struct State {
    /// path -> (contents, mode)
    files: BTreeMap<String, (Vec<u8>, u32)>,
    calls: Vec<Call>,
    /// How many times each command has run, so a queued script can hand
    /// out a different answer the second time round.
    runs: BTreeMap<String, usize>,
}

/// Builder-configured fake host. Cheap to clone-free share: every method
/// takes `&self`, and the mutable parts sit behind one `Mutex`.
#[derive(Debug)]
pub struct FakeHost {
    policy: PrivilegePolicy,
    root_only: Vec<String>,
    os: HostOs,
    /// Responses per command, consumed in order; the last one repeats.
    scripts: BTreeMap<String, Vec<CmdOutput>>,
    default_output: CmdOutput,
    state: Mutex<State>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    pub fn new() -> Self {
        Self {
            policy: PrivilegePolicy::default(),
            root_only: Vec::new(),
            os: HostOs::Linux,
            scripts: BTreeMap::new(),
            default_output: CmdOutput {
                // Unscripted commands look like a missing binary, so a
                // writer that runs something the test did not expect
                // fails loudly instead of silently succeeding.
                code: Some(127),
                stdout: Vec::new(),
                stderr: "fake host: command not scripted".to_string(),
            },
            state: Mutex::new(State::default()),
        }
    }

    pub fn with_policy(mut self, policy: PrivilegePolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_os(mut self, os: HostOs) -> Self {
        self.os = os;
        self
    }

    /// Seed a file with mode 0644.
    pub fn file(self, path: &str, contents: &str) -> Self {
        {
            let mut state = self.lock();
            state
                .files
                .insert(path.to_string(), (contents.as_bytes().to_vec(), 0o644));
        }
        self
    }

    /// Any path under `prefix` refuses [`Privilege::User`] with `NeedsRoot`.
    pub fn root_only(mut self, prefix: &str) -> Self {
        self.root_only.push(prefix.to_string());
        self
    }

    pub fn script(mut self, cmd: &str, code: i32, stdout: &str, stderr: &str) -> Self {
        self.scripts
            .insert(cmd.to_string(), vec![output(code, stdout, stderr)]);
        self
    }

    /// Queue a further response for `cmd`. A writer that re-reads what it
    /// just wrote — `crontab -l` after `crontab -` — sees the new state
    /// on the second call, and the last queued response repeats after
    /// that.
    pub fn then_script(mut self, cmd: &str, code: i32, stdout: &str, stderr: &str) -> Self {
        self.scripts
            .entry(cmd.to_string())
            .or_default()
            .push(output(code, stdout, stderr));
        self
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub fn file_contents(&self, path: &str) -> Option<String> {
        self.lock()
            .files
            .get(path)
            .map(|(bytes, _)| String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn file_mode(&self, path: &str) -> Option<u32> {
        self.lock().files.get(path).map(|(_, mode)| *mode)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake host mutex poisoned")
    }

    /// File operations honour `root_only` the way the real hosts honour
    /// filesystem permissions: unprivileged access is refused outright,
    /// and a privileged one still has to be payable under the policy.
    fn guard(&self, path: &str, privilege: Privilege, operation: &str) -> Result<()> {
        if !self.root_only.iter().any(|p| path.starts_with(p.as_str())) {
            return Ok(());
        }
        match privilege {
            Privilege::User => Err(Error::NeedsRoot {
                operation: operation.to_string(),
            }),
            Privilege::Root => {
                privileged_command("", Privilege::Root, self.policy, operation)?;
                Ok(())
            }
        }
    }
}

impl Host for FakeHost {
    fn policy(&self) -> PrivilegePolicy {
        self.policy
    }

    fn label(&self) -> String {
        "fake".to_string()
    }

    fn current_user(&self) -> Result<String> {
        Ok("tester".to_string())
    }

    fn current_uid(&self) -> Result<u32> {
        Ok(1000)
    }

    fn home_dir(&self) -> Result<String> {
        Ok("/home/tester".to_string())
    }

    fn os(&self) -> Result<HostOs> {
        Ok(self.os)
    }

    fn utc_offset(&self) -> Result<FixedOffset> {
        Ok(FixedOffset::east_opt(0).expect("UTC is a valid offset"))
    }

    fn run(&self, cmd: &str, stdin: Option<&[u8]>, privilege: Privilege) -> Result<CmdOutput> {
        // Consulted for its refusal only: what we record is the plain
        // command, not the sudo-wrapped rendering.
        privileged_command(cmd, privilege, self.policy, cmd)?;
        let nth = {
            let mut state = self.lock();
            state.calls.push(Call {
                cmd: cmd.to_string(),
                stdin: stdin.map(|b| b.to_vec()),
                privilege,
            });
            let seen = state.runs.entry(cmd.to_string()).or_insert(0);
            let nth = *seen;
            *seen += 1;
            nth
        };
        let Some(queued) = self.scripts.get(cmd) else {
            return Ok(self.default_output.clone());
        };
        Ok(queued[nth.min(queued.len() - 1)].clone())
    }

    fn read_file(&self, path: &str, privilege: Privilege) -> Result<Option<Vec<u8>>> {
        self.guard(path, privilege, &format!("read {path}"))?;
        Ok(self.lock().files.get(path).map(|(bytes, _)| bytes.clone()))
    }

    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: u32,
        privilege: Privilege,
    ) -> Result<()> {
        self.guard(path, privilege, &format!("write {path}"))?;
        self.lock()
            .files
            .insert(path.to_string(), (contents.to_vec(), mode));
        Ok(())
    }

    fn remove_file(&self, path: &str, privilege: Privilege) -> Result<bool> {
        self.guard(path, privilege, &format!("remove {path}"))?;
        Ok(self.lock().files.remove(path).is_some())
    }

    fn exists(&self, path: &str, privilege: Privilege) -> Result<bool> {
        self.guard(path, privilege, &format!("check {path}"))?;
        Ok(self.lock().files.contains_key(path))
    }

    fn list_dir(&self, path: &str, privilege: Privilege) -> Result<Vec<String>> {
        self.guard(path, privilege, &format!("list {path}"))?;
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        let mut names: Vec<String> = self
            .lock()
            .files
            .keys()
            .filter_map(|key| key.strip_prefix(&prefix))
            .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn create_dir_all(&self, path: &str, privilege: Privilege) -> Result<()> {
        // Directories are implied by the paths of the files in the map,
        // so there is nothing to create — only the privilege to check.
        self.guard(path, privilege, &format!("mkdir {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_round_trip_and_absent_paths_are_none() {
        let host = FakeHost::new().file("/etc/crontab", "0 9 * * * root x\n");
        assert_eq!(
            host.read_to_string("/etc/crontab", Privilege::User)
                .unwrap(),
            Some("0 9 * * * root x\n".to_string())
        );
        assert_eq!(host.read_file("/nope", Privilege::User).unwrap(), None);
        assert!(host.exists("/etc/crontab", Privilege::User).unwrap());
        assert!(!host.exists("/nope", Privilege::User).unwrap());
    }

    #[test]
    fn write_then_remove_reports_presence() {
        let host = FakeHost::new();
        host.write_file("/tmp/a", b"body", 0o600, Privilege::User)
            .unwrap();
        assert_eq!(host.file_contents("/tmp/a"), Some("body".to_string()));
        assert_eq!(host.file_mode("/tmp/a"), Some(0o600));
        assert!(host.remove_file("/tmp/a", Privilege::User).unwrap());
        assert!(!host.remove_file("/tmp/a", Privilege::User).unwrap());
        assert_eq!(host.file_contents("/tmp/a"), None);
    }

    #[test]
    fn list_dir_returns_sorted_direct_children() {
        let host = FakeHost::new()
            .file("/etc/cron.d/zeta", "")
            .file("/etc/cron.d/alpha", "")
            .file("/etc/cron.d/deep/nested", "")
            .file("/etc/crontab", "");
        assert_eq!(
            host.list_dir("/etc/cron.d", Privilege::User).unwrap(),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
        assert!(host
            .list_dir("/var/empty", Privilege::User)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn root_only_paths_reject_unprivileged_access() {
        let host = FakeHost::new().root_only("/etc/cron.d");
        let err = host
            .write_file("/etc/cron.d/x", b"", 0o644, Privilege::User)
            .expect_err("must refuse");
        match err {
            Error::NeedsRoot { operation } => assert_eq!(operation, "write /etc/cron.d/x"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }
        // Root without a way to become root is refused too.
        assert!(matches!(
            host.write_file("/etc/cron.d/x", b"", 0o644, Privilege::Root),
            Err(Error::NeedsRoot { .. })
        ));
        // With sudo the same write goes through.
        let sudoer = FakeHost::new()
            .root_only("/etc/cron.d")
            .with_policy(PrivilegePolicy {
                is_root: false,
                sudo: true,
            });
        sudoer
            .write_file("/etc/cron.d/x", b"job\n", 0o644, Privilege::Root)
            .unwrap();
        assert_eq!(
            sudoer.file_contents("/etc/cron.d/x"),
            Some("job\n".to_string())
        );
    }

    #[test]
    fn run_records_the_unwrapped_command_with_stdin() {
        let host = FakeHost::new()
            .with_policy(PrivilegePolicy {
                is_root: false,
                sudo: true,
            })
            .script("crontab -", 0, "", "");
        let output = host
            .run("crontab -", Some(b"0 9 * * * x\n"), Privilege::Root)
            .unwrap();
        assert!(output.success());
        assert_eq!(
            host.calls(),
            vec![Call {
                cmd: "crontab -".to_string(),
                stdin: Some(b"0 9 * * * x\n".to_vec()),
                privilege: Privilege::Root,
            }]
        );
    }

    #[test]
    fn unscripted_commands_look_like_a_missing_binary() {
        let host = FakeHost::new();
        let output = host.run("atq", None, Privilege::User).unwrap();
        assert_eq!(output.code, Some(127));
        assert!(matches!(
            output.require_success("atq"),
            Err(Error::CommandNotFound(_))
        ));
    }

    #[test]
    fn run_refuses_root_without_a_policy_and_records_nothing() {
        let host = FakeHost::new();
        let err = host
            .run("systemctl daemon-reload", None, Privilege::Root)
            .expect_err("must refuse");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
        assert!(host.calls().is_empty());
    }

    #[test]
    fn queued_scripts_are_handed_out_in_order_then_repeat() {
        let host = FakeHost::new()
            .script("crontab -l", 0, "first\n", "")
            .then_script("crontab -l", 0, "second\n", "");
        let read = || {
            host.run("crontab -l", None, Privilege::User)
                .unwrap()
                .stdout_str()
                .into_owned()
        };
        assert_eq!(read(), "first\n");
        assert_eq!(read(), "second\n");
        assert_eq!(read(), "second\n");
    }

    #[test]
    fn the_os_is_configurable() {
        assert_eq!(
            FakeHost::new().with_os(HostOs::MacOs).os().unwrap(),
            HostOs::MacOs
        );
    }

    #[test]
    fn identity_is_fixed() {
        let host = FakeHost::new();
        assert_eq!(host.label(), "fake");
        assert_eq!(host.current_user().unwrap(), "tester");
        assert_eq!(host.current_uid().unwrap(), 1000);
        assert_eq!(host.home_dir().unwrap(), "/home/tester");
        assert_eq!(host.os().unwrap(), HostOs::Linux);
        assert_eq!(host.utc_offset().unwrap().local_minus_utc(), 0);
        host.create_dir_all("/tmp/x", Privilege::User).unwrap();
    }
}
