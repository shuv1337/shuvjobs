//! Remote collection over SSH. No binary is uploaded — we shell out to
//! the remote host's own `systemctl` / `crontab` / `atq` / `cat` / `find`
//! and feed the captured stdout into the same `parse_*` functions the
//! local adapters use.
//!
//! SSH connection multiplexing (`ControlMaster=auto` + `ControlPersist`)
//! keeps the per-call cost negligible after the initial handshake, and
//! `BatchMode=yes` makes us fail fast on missing key auth instead of
//! hanging on a password prompt.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use shuvjobs_adapters::{
    anacron::run_times_from_spool,
    cron::crontab_list_args,
    ids::RUN_PARTS,
    launchd::LaunchctlEntry,
    process::{run_process, to_cmd_output},
    systemd::{
        merge_unit_file_tasks, show_unit_groups, split_task_id, Scope, LIST_TIMERS_ARGS,
        LIST_UNIT_FILES_ARGS, SERVICE_SHOW_PROPERTIES, SHOW_CHUNK_SIZE, TIMER_SHOW_PROPERTIES,
    },
    AnacronAdapter, AtAdapter, CronAdapter, LaunchdAdapter, SystemdAdapter,
};
use shuvjobs_core::host::shell::{
    exists_script, list_dir_script, mkdir_script, read_file_script, remove_file_script, sh_c,
    shell_quote, shell_safe_filename, shell_safe_username, write_file_script, ABSENT_EXIT,
};
use shuvjobs_core::host::{
    privileged_command, CmdOutput, Host, HostOs, Privilege, PrivilegePolicy,
};
use shuvjobs_core::{Error as CoreError, Result as CoreResult, ScheduledTask, TaskStatus};

/// Cap below sshd's default `MaxSessions` (10) so we never get refused
/// channels on the remote side, even on the worst case of many small
/// commands flying over the same multiplex master.
const REMOTE_PARALLELISM: usize = 8;

/// Construct once per `--host` invocation and reuse for the lifetime of
/// the run — the multiplex master opened by the first `run_command`
/// stays up across `collect` calls (so `--refresh` never re-handshakes)
/// and is torn down when the collector is dropped.
#[derive(Debug)]
pub struct RemoteCollector {
    pub host: String,
    pub port: Option<u16>,
    pub key_path: Option<PathBuf>,
    /// SSH multiplex control socket. Per-process so concurrent `shuvjobs`
    /// invocations against the same host don't collide.
    control_path: PathBuf,
    /// The operator's `--sudo`: may root operations be wrapped in `sudo -n --`.
    sudo: bool,
    /// Whether the SSH login user is uid 0. Probed at most once, and only
    /// when some caller actually asks for the privilege policy.
    root: OnceLock<bool>,
    user: OnceLock<String>,
    uid: OnceLock<u32>,
    home: OnceLock<String>,
    os: OnceLock<HostOs>,
    offset: OnceLock<FixedOffset>,
}

/// Result of one `ssh user@host -- <cmd>` invocation. `NotFound` is
/// split out so per-source collectors can treat exit 127 the same way
/// local adapters treat a missing binary — silent skip.
#[derive(Debug)]
pub enum RemoteCmdError {
    NotFound,
    Failed {
        exit_code: i32,
        stderr: String,
    },
    /// SSH itself failed (network down, key rejected, killed by signal).
    Ssh(anyhow::Error),
}

impl std::fmt::Display for RemoteCmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "remote command not found (exit 127)"),
            Self::Failed { exit_code, stderr } => {
                if stderr.is_empty() {
                    write!(f, "remote command exited {exit_code}")
                } else {
                    write!(f, "remote command exited {exit_code}: {}", stderr.trim())
                }
            }
            Self::Ssh(e) => write!(f, "ssh failed: {e}"),
        }
    }
}

/// Same Unavailable / hard-error split that local adapters use, but for
/// remote sources. The orchestrator in [`RemoteCollector::collect`] maps
/// `Unavailable` to "skip silently" and `Other` to a stderr warning,
/// matching `shuvjobs`'s local-mode behavior.
#[derive(Debug)]
enum RemoteSourceError {
    Unavailable,
    Transport(anyhow::Error),
    Other(anyhow::Error),
}

/// One command execution on the remote host. Production uses
/// [`RemoteCollector`] (an `ssh` round-trip per call); tests use a
/// fixture-driven runner so the per-source collectors below are
/// exercised exactly as production runs them.
///
/// `Sync` because the parallel collectors share a single runner across
/// [`REMOTE_PARALLELISM`] worker threads.
pub trait CommandRunner: Sync {
    fn run(&self, cmd: &str) -> std::result::Result<String, RemoteCmdError>;
}

impl CommandRunner for RemoteCollector {
    fn run(&self, cmd: &str) -> std::result::Result<String, RemoteCmdError> {
        self.run_command(cmd)
    }
}

impl RemoteCollector {
    pub fn new(host: String, port: Option<u16>, key_path: Option<PathBuf>) -> Self {
        let control_path =
            std::env::temp_dir().join(format!("shuvjobs-ssh-{}", std::process::id()));
        Self {
            host,
            port,
            key_path,
            control_path,
            sudo: false,
            root: OnceLock::new(),
            user: OnceLock::new(),
            uid: OnceLock::new(),
            home: OnceLock::new(),
            os: OnceLock::new(),
            offset: OnceLock::new(),
        }
    }

    /// Opt into `sudo -n --` for [`Privilege::Root`] operations.
    pub fn with_sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }

    /// Build the argv passed to `ssh` for running `cmd` on the remote.
    pub fn ssh_argv(&self, cmd: &str) -> Vec<String> {
        let mut args = self.ssh_options();
        args.push(self.host.clone());
        args.push("--".into());
        args.push(cmd.into());
        args
    }

    /// ssh options shared by command runs and the multiplex teardown.
    fn ssh_options(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", self.control_path.display()),
            "-o".into(),
            "ControlPersist=60".into(),
        ];
        if let Some(p) = self.port {
            args.push("-p".into());
            args.push(p.to_string());
        }
        if let Some(k) = &self.key_path {
            if let Some(s) = k.to_str() {
                args.push("-i".into());
                args.push(s.into());
            }
        }
        args
    }

    /// Run `cmd` on the remote host, optionally feeding it `stdin`, and
    /// report how it finished.
    ///
    /// Only SSH itself failing is an `Err`: the remote command's own
    /// non-zero exit is data, and the callers above decide what it means.
    pub fn run_raw(
        &self,
        cmd: &str,
        stdin: Option<&[u8]>,
    ) -> std::result::Result<CmdOutput, RemoteCmdError> {
        let mut command = Command::new("ssh");
        command.args(self.ssh_argv(cmd));
        let out = to_cmd_output(
            run_process(&mut command, stdin)
                .map_err(|e| RemoteCmdError::Ssh(anyhow!("invoking ssh: {e}")))?,
        );
        classify_raw(out.code, out.stdout, out.stderr)
    }

    /// Run `cmd` on the remote host and return its stdout.
    pub fn run_command(&self, cmd: &str) -> std::result::Result<String, RemoteCmdError> {
        let out = self.run_raw(cmd, None)?;
        classify(out.code, out.stdout, out.stderr.into_bytes())
    }

    /// Stdout of an unprivileged probe (`id -u`, `uname -s`, …). Kept off
    /// [`Host::run`] so asking who we are never has to ask whether we are
    /// root first.
    fn probe(&self, cmd: &str) -> CoreResult<String> {
        self.run_raw(cmd, None)
            .map_err(host_error)?
            .require_success(cmd)
    }

    /// One file operation, rendered through the privilege wrapper.
    fn file_op(
        &self,
        op: FileOp,
        path: &str,
        stdin: Option<&[u8]>,
        privilege: Privilege,
    ) -> CoreResult<CmdOutput> {
        let cmd = file_op_command(op, path, privilege, self.policy())?;
        self.run_raw(&cmd, stdin).map_err(host_error)
    }

    /// Collect from every supported source on the remote. Sources whose
    /// per-item count is high (`systemd`, `cron`, `launchd`) fan out
    /// across [`REMOTE_PARALLELISM`] worker threads sharing `&self` and
    /// the same multiplex master. `at` and `anacron` stay serial — too
    /// few items to bother.
    pub fn collect(&self) -> Result<Vec<ScheduledTask>> {
        // One probe up front: a network outage should produce a single
        // clean error rather than five identical warnings.
        if let Err(e) = self.run_command("true") {
            self.close();
            bail!("ssh connection to {} failed: {e}", self.host);
        }

        let offset_text = match self.run_command("date +%z") {
            Ok(offset) => offset,
            Err(error) => {
                self.close();
                return Err(anyhow!("reading timezone from {}: {error}", self.host));
            }
        };
        let Some(remote_offset) = parse_utc_offset(offset_text.trim()) else {
            self.close();
            return Err(anyhow!(
                "invalid UTC offset from {}: {offset_text:?}",
                self.host
            ));
        };
        let remote_now = Utc::now().with_timezone(&remote_offset);

        let systemd_result = collect_systemd(self);
        let cron_result = collect_cron(self, remote_now);
        let launchd_result = collect_launchd(self, remote_now);
        let at_result = collect_at(self, &remote_offset);
        let anacron_result = collect_anacron(self);

        let mut tasks: Vec<ScheduledTask> = Vec::new();
        for (label, result) in [
            ("systemd", systemd_result),
            ("cron", cron_result),
            ("at", at_result),
            ("anacron", anacron_result),
            ("launchd", launchd_result),
        ] {
            match result {
                Ok(mut t) => tasks.append(&mut t),
                Err(RemoteSourceError::Unavailable) => {}
                Err(RemoteSourceError::Transport(e)) => {
                    self.close();
                    return Err(anyhow!("remote {label} source lost its SSH transport: {e}"));
                }
                Err(RemoteSourceError::Other(e)) => {
                    eprintln!("warning: remote {label} source: {e}");
                }
            }
        }

        Ok(tasks)
    }

    /// Best-effort teardown of the multiplex master. Safe to call more
    /// than once; a no-op when no master was ever established.
    fn close(&self) {
        if !self.control_path.exists() {
            return;
        }
        let mut args = self.ssh_options();
        args.push("-O".into());
        args.push("exit".into());
        args.push(self.host.clone());
        let _ = Command::new("ssh").args(args).output();
    }
}

impl Drop for RemoteCollector {
    fn drop(&mut self) {
        self.close();
    }
}

/// The file operations [`Host`] performs over SSH, each one a pinned
/// POSIX script from [`shuvjobs_core::host::shell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Read,
    Write { mode: u32 },
    Remove,
    Exists,
    ListDir,
    MkdirAll,
}

impl FileOp {
    fn script(self, path: &str) -> String {
        match self {
            Self::Read => read_file_script(path),
            Self::Write { mode } => write_file_script(path, mode),
            Self::Remove => remove_file_script(path),
            Self::Exists => exists_script(path),
            Self::ListDir => list_dir_script(path),
            Self::MkdirAll => mkdir_script(path),
        }
    }

    /// What the operator sees in a `NeedsRoot` or command-failure message.
    fn operation(self, path: &str) -> String {
        let verb = match self {
            Self::Read => "read",
            Self::Write { .. } => "write",
            Self::Remove => "remove",
            Self::Exists => "check",
            Self::ListDir => "list",
            Self::MkdirAll => "mkdir",
        };
        format!("{verb} {path}")
    }
}

/// Render `cmd` for `privilege` under `policy`, or refuse. Pure, so the
/// exact strings that go over the wire are pinned by tests.
pub fn remote_command(
    cmd: &str,
    privilege: Privilege,
    policy: PrivilegePolicy,
    operation: &str,
) -> CoreResult<String> {
    Ok(privileged_command(cmd, privilege, policy, operation)?.into_owned())
}

/// The full command line for one file operation: the pinned script,
/// wrapped in `sh -c` because the remote login shell may be fish or zsh,
/// then wrapped again by the privilege policy.
pub fn file_op_command(
    op: FileOp,
    path: &str,
    privilege: Privilege,
    policy: PrivilegePolicy,
) -> CoreResult<String> {
    remote_command(
        &sh_c(&op.script(path)),
        privilege,
        policy,
        &op.operation(path),
    )
}

/// A failed SSH round-trip is a transport failure, never the remote
/// command's own verdict.
fn host_error(error: RemoteCmdError) -> CoreError {
    match error {
        RemoteCmdError::Ssh(e) => CoreError::Transport(e.to_string()),
        other => CoreError::Transport(other.to_string()),
    }
}

fn parse_host_os(uname: &str) -> HostOs {
    match uname.trim() {
        "Darwin" => HostOs::MacOs,
        "Linux" => HostOs::Linux,
        _ => HostOs::Other,
    }
}

/// The remote end of the execution seam. Reads go through
/// [`CommandRunner`] as before; writes and privileged reads go through
/// this impl, which renders the same scripts [`shuvjobs_adapters::LocalHost`]
/// runs under sudo.
impl Host for RemoteCollector {
    fn policy(&self) -> PrivilegePolicy {
        let is_root = *self.root.get_or_init(|| {
            self.run_raw("id -u", None)
                .is_ok_and(|out| out.success() && out.stdout_str().trim() == "0")
        });
        PrivilegePolicy {
            is_root,
            sudo: self.sudo,
        }
    }

    fn label(&self) -> String {
        self.host.clone()
    }

    fn current_user(&self) -> CoreResult<String> {
        if let Some(user) = self.user.get() {
            return Ok(user.clone());
        }
        let user = self.probe("id -un")?.trim().to_string();
        if user.is_empty() {
            return Err(CoreError::Other(format!(
                "cannot determine the remote user on {}",
                self.host
            )));
        }
        let _ = self.user.set(user.clone());
        Ok(user)
    }

    fn current_uid(&self) -> CoreResult<u32> {
        if let Some(uid) = self.uid.get() {
            return Ok(*uid);
        }
        let text = self.probe("id -u")?;
        let uid: u32 = text
            .trim()
            .parse()
            .map_err(|_| CoreError::Other(format!("invalid uid from {}: {text:?}", self.host)))?;
        let _ = self.uid.set(uid);
        Ok(uid)
    }

    fn home_dir(&self) -> CoreResult<String> {
        if let Some(home) = self.home.get() {
            return Ok(home.clone());
        }
        // `printf` rather than `echo $HOME`: no trailing newline of its
        // own and no word splitting on a path with spaces.
        let home = self.probe(r#"printf %s "$HOME""#)?.trim().to_string();
        if home.is_empty() {
            return Err(CoreError::Other(format!(
                "cannot determine the home directory on {}: $HOME unset",
                self.host
            )));
        }
        let _ = self.home.set(home.clone());
        Ok(home)
    }

    fn os(&self) -> CoreResult<HostOs> {
        if let Some(os) = self.os.get() {
            return Ok(*os);
        }
        let os = parse_host_os(&self.probe("uname -s")?);
        let _ = self.os.set(os);
        Ok(os)
    }

    fn utc_offset(&self) -> CoreResult<FixedOffset> {
        if let Some(offset) = self.offset.get() {
            return Ok(*offset);
        }
        let text = self.probe("date +%z")?;
        let offset = parse_utc_offset(text.trim()).ok_or_else(|| {
            CoreError::Other(format!("invalid UTC offset from {}: {text:?}", self.host))
        })?;
        let _ = self.offset.set(offset);
        Ok(offset)
    }

    fn run(&self, cmd: &str, stdin: Option<&[u8]>, privilege: Privilege) -> CoreResult<CmdOutput> {
        let rendered = remote_command(cmd, privilege, self.policy(), cmd)?;
        self.run_raw(&rendered, stdin).map_err(host_error)
    }

    fn read_file(&self, path: &str, privilege: Privilege) -> CoreResult<Option<Vec<u8>>> {
        let out = self.file_op(FileOp::Read, path, None, privilege)?;
        if out.success() {
            return Ok(Some(out.stdout));
        }
        if out.code == Some(ABSENT_EXIT) {
            return Ok(None);
        }
        Err(failure(out, FileOp::Read, path))
    }

    fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: u32,
        privilege: Privilege,
    ) -> CoreResult<()> {
        let op = FileOp::Write { mode };
        self.file_op(op, path, Some(contents), privilege)?
            .require_success(&op.operation(path))?;
        Ok(())
    }

    fn remove_file(&self, path: &str, privilege: Privilege) -> CoreResult<bool> {
        let out = self.file_op(FileOp::Remove, path, None, privilege)?;
        if out.success() {
            return Ok(true);
        }
        if out.code == Some(ABSENT_EXIT) {
            return Ok(false);
        }
        Err(failure(out, FileOp::Remove, path))
    }

    fn exists(&self, path: &str, privilege: Privilege) -> CoreResult<bool> {
        Ok(self
            .file_op(FileOp::Exists, path, None, privilege)?
            .success())
    }

    fn list_dir(&self, path: &str, privilege: Privilege) -> CoreResult<Vec<String>> {
        let out = self.file_op(FileOp::ListDir, path, None, privilege)?;
        if out.code == Some(ABSENT_EXIT) {
            return Ok(Vec::new());
        }
        let listing = out.require_success(&FileOp::ListDir.operation(path))?;
        let mut names: Vec<String> = listing
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        names.sort();
        Ok(names)
    }

    fn create_dir_all(&self, path: &str, privilege: Privilege) -> CoreResult<()> {
        self.file_op(FileOp::MkdirAll, path, None, privilege)?
            .require_success(&FileOp::MkdirAll.operation(path))?;
        Ok(())
    }
}

/// The error a non-zero, non-[`ABSENT_EXIT`] file operation deserves.
fn failure(out: CmdOutput, op: FileOp, path: &str) -> CoreError {
    out.require_success(&op.operation(path))
        .expect_err("a non-zero exit cannot succeed")
}

/// One batched `systemctl show` for a chunk of units. The reply is one
/// block per unit, keyed back to a unit by `Id=`.
fn systemctl_show_cmd(scope: Scope, units: &[String], properties: &str) -> String {
    let quoted: Vec<String> = units.iter().map(|u| shell_quote(u)).collect();
    format!(
        "systemctl{} show {} --property={properties} --no-pager",
        scope_flag(scope),
        quoted.join(" ")
    )
}

fn systemctl_list_timers_cmd(scope: Scope) -> String {
    format!(
        "systemctl{} {}",
        scope_flag(scope),
        LIST_TIMERS_ARGS.join(" ")
    )
}

/// The unit-file inventory, which unlike `list-timers` also names timers
/// that are disabled and stopped (and therefore unloaded).
fn systemctl_list_unit_files_cmd(scope: Scope) -> String {
    format!(
        "systemctl{} {}",
        scope_flag(scope),
        LIST_UNIT_FILES_ARGS.join(" ")
    )
}

/// `" --user"` or `""`, spliced into the remote shell command.
fn scope_flag(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "",
        Scope::User => " --user",
    }
}

/// `crontab -l` for the SSH login user, `crontab -l -u <user>` (root-only)
/// for everyone else. Usernames are pre-filtered by `shell_safe_username`.
fn crontab_list_cmd(user: &str, current: Option<&str>) -> String {
    format!(
        "crontab {} 2>/dev/null",
        crontab_list_args(user, current).join(" ")
    )
}

/// Username the SSH session runs as, so the bridge knows which crontab
/// to read without `-u`. `None` if `id` is unavailable or unsafe.
fn remote_current_user(
    runner: &impl CommandRunner,
) -> std::result::Result<Option<String>, RemoteSourceError> {
    Ok(optional_remote_output(runner.run("id -un 2>/dev/null"))?
        .map(|s| s.trim().to_string())
        .filter(|s| shell_safe_username(s)))
}

/// Split out so we can unit-test without spawning a process.
fn classify(
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> std::result::Result<String, RemoteCmdError> {
    match code {
        Some(0) => Ok(String::from_utf8_lossy(&stdout).into_owned()),
        Some(255) => {
            let message = String::from_utf8_lossy(&stderr);
            Err(RemoteCmdError::Ssh(anyhow!(
                "ssh transport exited 255: {}",
                message.trim()
            )))
        }
        Some(127) => Err(RemoteCmdError::NotFound),
        Some(c) => Err(RemoteCmdError::Failed {
            exit_code: c,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        None => Err(RemoteCmdError::Ssh(anyhow!("ssh terminated by signal"))),
    }
}

/// The transport-level half of [`classify`]: everything that is not SSH
/// itself failing comes back as a [`CmdOutput`] for the caller to judge.
fn classify_raw(
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
) -> std::result::Result<CmdOutput, RemoteCmdError> {
    match code {
        None => Err(RemoteCmdError::Ssh(anyhow!("ssh terminated by signal"))),
        Some(255) => Err(RemoteCmdError::Ssh(anyhow!(
            "ssh transport exited 255: {}",
            stderr.trim()
        ))),
        _ => Ok(CmdOutput {
            code,
            stdout,
            stderr,
        }),
    }
}

fn optional_remote_output(
    result: std::result::Result<String, RemoteCmdError>,
) -> std::result::Result<Option<String>, RemoteSourceError> {
    match result {
        Ok(output) => Ok(Some(output)),
        Err(RemoteCmdError::Ssh(error)) => Err(RemoteSourceError::Transport(error)),
        Err(RemoteCmdError::NotFound | RemoteCmdError::Failed { .. }) => Ok(None),
    }
}

fn parse_utc_offset(value: &str) -> Option<FixedOffset> {
    let compact = value.replace(':', "");
    if compact.len() != 5 {
        return None;
    }
    let sign = match &compact[..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let hours: i32 = compact[1..3].parse().ok()?;
    let minutes: i32 = compact[3..5].parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn remote_source_error(context: &str, error: RemoteCmdError) -> RemoteSourceError {
    match error {
        RemoteCmdError::Ssh(error) => RemoteSourceError::Transport(anyhow!("{context}: {error}")),
        error => RemoteSourceError::Other(anyhow!("{context}: {error}")),
    }
}

/// `$HOME` is expanded by the remote shell.
const LAUNCHD_FIND_CMD: &str = "find $HOME/Library/LaunchAgents /Library/LaunchAgents \
                                /Library/LaunchDaemons /System/Library/LaunchAgents \
                                /System/Library/LaunchDaemons -maxdepth 1 -name '*.plist' 2>/dev/null";

fn collect_systemd(
    runner: &impl CommandRunner,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
    match runner.run("command -v systemctl >/dev/null 2>&1 && echo present") {
        Ok(s) if s.trim() == "present" => {}
        Ok(_) => return Err(RemoteSourceError::Unavailable),
        Err(RemoteCmdError::NotFound) | Err(RemoteCmdError::Failed { .. }) => {
            return Err(RemoteSourceError::Unavailable);
        }
        Err(RemoteCmdError::Ssh(e)) => return Err(RemoteSourceError::Transport(e)),
    }

    let json = match runner.run(&systemctl_list_timers_cmd(Scope::System)) {
        Ok(s) => s,
        Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
        Err(e) => return Err(remote_source_error("systemctl list-timers", e)),
    };
    let mut tasks = SystemdAdapter::parse_list_timers(&json)
        .map_err(|e| RemoteSourceError::Other(anyhow!("parse list-timers: {e}")))?;

    // Over SSH the user manager is usually absent unless lingering is
    // enabled — `systemctl --user` then exits non-zero with "Failed to
    // connect to bus". Treat that as an empty scope, not an error.
    if let Some(user_json) =
        optional_remote_output(runner.run(&systemctl_list_timers_cmd(Scope::User)))?
    {
        if let Ok(user_tasks) = SystemdAdapter::parse_list_timers_scoped(&user_json, Scope::User) {
            tasks.extend(user_tasks);
        }
    }

    // `list-timers` only reports loaded units, so a disabled and stopped
    // timer is invisible there. The unit-file inventory is the complete
    // list; anything it names that the timer listing did not gets a
    // placeholder task, filled in by the `show` pass below.
    for scope in [Scope::System, Scope::User] {
        let Some(json) = optional_remote_output(runner.run(&systemctl_list_unit_files_cmd(scope)))?
        else {
            continue;
        };
        if let Ok(extra) = SystemdAdapter::parse_list_unit_files_scoped(&json, scope) {
            merge_unit_file_tasks(&mut tasks, extra);
        }
    }

    // One batched `systemctl show` per property set per scope (chunked),
    // instead of two round-trips per timer: 2N commands become
    // 2 * ceil(N / SHOW_CHUNK_SIZE), still fanned out across the
    // multiplexed connection.
    let jobs = show_jobs(&tasks);
    let outputs: Vec<Option<String>> = parallel_map(&jobs, REMOTE_PARALLELISM, |job| {
        optional_remote_output(runner.run(&systemctl_show_cmd(
            job.scope,
            &job.units,
            job.properties,
        )))
    })
    .into_iter()
    .collect::<std::result::Result<Vec<_>, RemoteSourceError>>()?;

    let mut timer_blocks: HashMap<Scope, HashMap<String, String>> = HashMap::new();
    let mut service_blocks: HashMap<Scope, HashMap<String, String>> = HashMap::new();
    for (job, output) in jobs.iter().zip(outputs) {
        let Some(text) = output else { continue };
        let target = if job.timers {
            &mut timer_blocks
        } else {
            &mut service_blocks
        };
        target
            .entry(job.scope)
            .or_default()
            .extend(SystemdAdapter::parse_show_blocks(&text));
    }

    for task in &mut tasks {
        let id = task.id.clone();
        let (scope, unit) = split_task_id(&id);
        let timer_text = timer_blocks.get(&scope).and_then(|b| b.get(unit));
        let service_text = service_blocks
            .get(&scope)
            .and_then(|b| b.get(&task.command));
        SystemdAdapter::apply_show(
            task,
            timer_text.map(String::as_str),
            service_text.map(String::as_str),
        );
    }

    Ok(tasks)
}

/// One batched `systemctl show` to issue over SSH.
#[derive(Debug, PartialEq, Eq)]
struct ShowJob {
    scope: Scope,
    /// `true` for the timer property set, `false` for the service one.
    timers: bool,
    properties: &'static str,
    units: Vec<String>,
}

/// Split the listed timers into the `systemctl show` calls to make:
/// timers and their bound services, per scope, chunked so no single
/// remote command line grows unbounded.
fn show_jobs(tasks: &[ScheduledTask]) -> Vec<ShowJob> {
    let mut jobs = Vec::new();
    for (scope, timer_units, service_units) in show_unit_groups(tasks) {
        for chunk in timer_units.chunks(SHOW_CHUNK_SIZE) {
            jobs.push(ShowJob {
                scope,
                timers: true,
                properties: TIMER_SHOW_PROPERTIES,
                units: chunk.to_vec(),
            });
        }
        for chunk in service_units.chunks(SHOW_CHUNK_SIZE) {
            jobs.push(ShowJob {
                scope,
                timers: false,
                properties: SERVICE_SHOW_PROPERTIES,
                units: chunk.to_vec(),
            });
        }
    }
    jobs
}

fn collect_cron(
    runner: &impl CommandRunner,
    now: DateTime<FixedOffset>,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
    let mut tasks = Vec::new();
    let mut any_present = false;

    if let Some(text) = optional_remote_output(runner.run("cat /etc/crontab 2>/dev/null"))? {
        any_present = true;
        if !text.is_empty() {
            tasks.extend(CronAdapter::parse_crontab_at(
                &text,
                "/etc/crontab",
                true,
                now,
            ));
        }
    }

    // /etc/cron.d/* — list once, then cat in parallel.
    if let Some(listing) = optional_remote_output(runner.run("ls -1 /etc/cron.d 2>/dev/null"))? {
        any_present = true;
        let filenames: Vec<String> = listing
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let cron_d_results: Vec<(
            String,
            std::result::Result<Option<String>, RemoteSourceError>,
        )> = parallel_map(&filenames, REMOTE_PARALLELISM, |fname| {
            let path = format!("/etc/cron.d/{fname}");
            let cmd = format!("cat {} 2>/dev/null", shell_quote(&path));
            (path, optional_remote_output(runner.run(&cmd)))
        });
        for (path, content) in cron_d_results {
            if let Some(text) = content? {
                tasks.extend(CronAdapter::parse_crontab_at(&text, &path, true, now));
            }
        }
    }

    // run-parts directories: one `ls` for the names plus one `find` for
    // the executable bits (run-parts skips non-executable files). Eight
    // small commands — keep serial.
    for (period, dir) in RUN_PARTS {
        let cmd = format!("ls -1 {} 2>/dev/null", shell_quote(dir));
        if let Some(listing) = optional_remote_output(runner.run(&cmd))? {
            any_present = true;
            let names: Vec<&str> = listing
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if names.is_empty() {
                continue;
            }
            let executable = optional_remote_output(runner.run(&run_parts_find_cmd(dir)))?;
            // No `find` output at all means "unknown", not "none are
            // executable" — a locked-down or `find`-less host must not
            // report every script as disabled.
            let executable: Option<std::collections::HashSet<&str>> =
                executable.as_deref().map(|text| {
                    text.lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .filter_map(|path| path.rsplit('/').next())
                        .collect()
                });
            let scripts: Vec<(&str, Option<bool>)> = names
                .iter()
                .map(|name| (*name, executable.as_ref().map(|set| set.contains(name))))
                .collect();
            tasks.extend(CronAdapter::parse_run_parts_at(period, &scripts, dir, now));
        }
    }

    // Per-user crontabs in parallel.
    if optional_remote_output(runner.run("command -v crontab >/dev/null 2>&1"))?.is_some() {
        any_present = true;
        if let Some(passwd) = optional_remote_output(runner.run("cat /etc/passwd 2>/dev/null"))? {
            let current = remote_current_user(runner)?;
            let users: Vec<String> = CronAdapter::parse_passwd(&passwd)
                .into_iter()
                .filter(|u| shell_safe_username(u))
                .collect();
            let user_results: Vec<(
                String,
                std::result::Result<Option<String>, RemoteSourceError>,
            )> = parallel_map(&users, REMOTE_PARALLELISM, |user| {
                let cmd = crontab_list_cmd(user, current.as_deref());
                (user.clone(), optional_remote_output(runner.run(&cmd)))
            });
            for (user, content) in user_results {
                if let Some(text) = content? {
                    if !text.is_empty() {
                        tasks.extend(CronAdapter::parse_crontab_at(
                            &text,
                            &format!("user:{user}"),
                            false,
                            now,
                        ));
                    }
                }
            }
        }
    }

    if !any_present {
        return Err(RemoteSourceError::Unavailable);
    }
    Ok(tasks)
}

/// Files in a run-parts directory that carry the owner execute bit.
fn run_parts_find_cmd(dir: &str) -> String {
    format!(
        "find {} -maxdepth 1 -type f -perm -u+x 2>/dev/null",
        shell_quote(dir)
    )
}

fn collect_launchd(
    runner: &impl CommandRunner,
    now: DateTime<FixedOffset>,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
    if optional_remote_output(runner.run("command -v launchctl >/dev/null 2>&1"))?.is_none() {
        return Err(RemoteSourceError::Unavailable);
    }
    let listing = match runner.run("launchctl list") {
        Ok(s) => s,
        Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
        Err(e) => return Err(remote_source_error("launchctl list", e)),
    };
    let runtime: std::collections::HashMap<String, LaunchctlEntry> =
        LaunchdAdapter::parse_launchctl_list(&listing)
            .into_iter()
            .map(|e| (e.label.clone(), e))
            .collect();

    let plist_paths_str = optional_remote_output(runner.run(LAUNCHD_FIND_CMD))?.unwrap_or_default();
    let plist_paths: Vec<String> = plist_paths_str
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    // /System/Library/LaunchDaemons alone has 50+ plists; parallel
    // cat is the biggest win in this whole module.
    let plist_results: Vec<(
        String,
        std::result::Result<Option<String>, RemoteSourceError>,
    )> = parallel_map(&plist_paths, REMOTE_PARALLELISM, |path| {
        let cmd = format!("cat {} 2>/dev/null", shell_quote(path));
        (path.clone(), optional_remote_output(runner.run(&cmd)))
    });

    let mut tasks = Vec::new();
    for (path, content_opt) in plist_results {
        let Some(content) = content_opt? else {
            continue;
        };
        match LaunchdAdapter::parse_plist_at(content.as_bytes(), &path, now) {
            Ok(Some(mut task)) => {
                let rt_entry = runtime.get(&task.id);
                apply_launchctl_runtime(&mut task, rt_entry);
                tasks.push(task);
            }
            Ok(None) => {}
            Err(e) => eprintln!("warning: parse {path}: {e}"),
        }
    }
    Ok(tasks)
}

fn collect_at<Tz: TimeZone>(
    runner: &impl CommandRunner,
    timezone: &Tz,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
    if optional_remote_output(runner.run("command -v atq >/dev/null 2>&1"))?.is_none() {
        return Err(RemoteSourceError::Unavailable);
    }
    let listing = match runner.run("atq") {
        Ok(s) => s,
        Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
        Err(e) => return Err(remote_source_error("atq", e)),
    };
    let mut tasks = AtAdapter::parse_atq_in_timezone(&listing, timezone);
    for task in &mut tasks {
        let Some(num) = task.id.strip_prefix("at:") else {
            continue;
        };
        // atq job IDs are unsigned integers; refuse anything weird before
        // it ends up in a shell command.
        if !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmd = format!("at -c {num} 2>/dev/null");
        if let Some(text) = optional_remote_output(runner.run(&cmd))? {
            if let Some(c) = AtAdapter::parse_at_c(&text) {
                task.command = c;
            }
        }
    }
    Ok(tasks)
}

fn collect_anacron(
    runner: &impl CommandRunner,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
    let text = match optional_remote_output(runner.run("cat /etc/anacrontab 2>/dev/null"))? {
        Some(text) if !text.is_empty() => text,
        _ => return Err(RemoteSourceError::Unavailable),
    };
    let mut tasks = AnacronAdapter::parse_anacrontab(&text);

    for task in &mut tasks {
        let Some(job_id) = task.id.strip_prefix("anacron:") else {
            continue;
        };
        if !shell_safe_filename(job_id) {
            continue;
        }
        let cmd = format!("cat /var/spool/anacron/{job_id} 2>/dev/null");
        if let Some(spool) = optional_remote_output(runner.run(&cmd))? {
            if let Some(date) = AnacronAdapter::parse_spool_file(&spool) {
                let (last, next) = run_times_from_spool(date, &task.schedule, Utc::now());
                task.last_run = last;
                task.next_run = next;
            }
        }
    }

    Ok(tasks)
}

/// Map `f` over `items` with up to `max_parallel` workers in flight,
/// preserving input order. Built on `thread::scope` so the closure can
/// borrow `&RemoteCollector` without any `Arc`/`Mutex` plumbing.
///
/// Chunked fan-out (process N, join, repeat) — not the smartest for
/// stragglers but trivial to reason about and good enough for our small
/// `N`.
fn parallel_map<T, R, F>(items: &[T], max_parallel: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let max_parallel = max_parallel.max(1);
    let mut results: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    thread::scope(|scope| {
        let f_ref = &f;
        let mut idx = 0;
        while idx < items.len() {
            let chunk_end = (idx + max_parallel).min(items.len());
            let mut handles = Vec::with_capacity(chunk_end - idx);
            for (i, item_ref) in items.iter().enumerate().take(chunk_end).skip(idx) {
                handles.push((i, scope.spawn(move || f_ref(item_ref))));
            }
            for (i, h) in handles {
                results[i] = Some(h.join().expect("parallel_map worker panicked"));
            }
            idx = chunk_end;
        }
    });
    results
        .into_iter()
        .map(|o| o.expect("parallel_map left a slot unfilled"))
        .collect()
}

/// Merge `launchctl list` runtime state onto a plist-parsed task.
fn apply_launchctl_runtime(task: &mut ScheduledTask, entry: Option<&LaunchctlEntry>) {
    let Some(rt) = entry else { return };
    // Listed by launchd means loaded, whatever the plist's `Disabled`
    // key says. (`launchctl print-disabled` is not consulted yet.)
    task.enabled = Some(true);
    task.last_status = rt.last_exit_status.map(|code| {
        if code == 0 {
            TaskStatus::Success
        } else {
            TaskStatus::Failed(format!("exit {code}"))
        }
    });
    if rt.pid.is_some() && task.last_status.is_none() {
        task.last_status = Some(TaskStatus::Running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuvjobs_core::ScheduleType;

    /// A collector with a fixed control path, so the argv assertions
    /// below don't depend on the test process's pid.
    fn collector() -> RemoteCollector {
        let mut coll = RemoteCollector::new("alice@host".into(), None, None);
        coll.control_path = PathBuf::from("/tmp/shuvjobs-ssh-1");
        coll
    }

    /// What the fixture runner returns for a command it has no fixture
    /// for. `Failed` is the realistic default (the source slice simply
    /// isn't present); the other two drive the "everything fails" cases.
    #[derive(Clone, Copy)]
    enum Fallback {
        Failed,
        NotFound,
        SshDown,
    }

    /// [`CommandRunner`] backed by a fixed command → stdout map, so the
    /// real production collectors run unchanged and the fixture keys
    /// assert the exact command strings issued over SSH.
    struct FixtureRunner {
        fixtures: HashMap<&'static str, &'static str>,
        fallback: Fallback,
    }

    impl FixtureRunner {
        fn new(fixtures: HashMap<&'static str, &'static str>) -> Self {
            Self {
                fixtures,
                fallback: Fallback::Failed,
            }
        }

        fn always(fallback: Fallback) -> Self {
            Self {
                fixtures: HashMap::new(),
                fallback,
            }
        }
    }

    impl CommandRunner for FixtureRunner {
        fn run(&self, cmd: &str) -> std::result::Result<String, RemoteCmdError> {
            match self.fixtures.get(cmd) {
                Some(s) => Ok((*s).to_string()),
                None => Err(match self.fallback {
                    Fallback::Failed => RemoteCmdError::Failed {
                        exit_code: 1,
                        stderr: String::new(),
                    },
                    Fallback::NotFound => RemoteCmdError::NotFound,
                    Fallback::SshDown => RemoteCmdError::Ssh(anyhow!("connection lost")),
                }),
            }
        }
    }

    /// Fixed "now" in a non-UTC offset, standing in for the remote host's
    /// wall clock in the tests that need a deterministic schedule.
    fn remote_now() -> DateTime<FixedOffset> {
        FixedOffset::west_opt(7 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 4, 14, 1, 30, 0)
            .unwrap()
    }

    #[test]
    fn ssh_argv_minimal_includes_batch_mode_and_multiplexing() {
        let argv = collector().ssh_argv("uptime");
        assert_eq!(
            argv,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/tmp/shuvjobs-ssh-1",
                "-o",
                "ControlPersist=60",
                "alice@host",
                "--",
                "uptime",
            ]
        );
    }

    #[test]
    fn ssh_argv_propagates_port_and_key() {
        let mut coll =
            RemoteCollector::new("alice@host".into(), Some(2222), Some(PathBuf::from("/k")));
        coll.control_path = PathBuf::from("/tmp/shuvjobs-ssh-1");
        let argv = coll.ssh_argv("ls");
        // ssh doesn't care about flag order; assert by adjacency.
        let p_idx = argv.iter().position(|a| a == "-p").unwrap();
        assert_eq!(argv[p_idx + 1], "2222");
        let i_idx = argv.iter().position(|a| a == "-i").unwrap();
        assert_eq!(argv[i_idx + 1], "/k");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"));
    }

    #[test]
    fn classify_zero_returns_stdout() {
        let r = classify(Some(0), b"hello\n".to_vec(), Vec::new()).unwrap();
        assert_eq!(r, "hello\n");
    }

    #[test]
    fn classify_127_is_not_found() {
        let r = classify(Some(127), Vec::new(), b"sh: foo: not found".to_vec()).unwrap_err();
        assert!(matches!(r, RemoteCmdError::NotFound));
    }

    #[test]
    fn classify_255_is_ssh_error() {
        let r = classify(Some(255), Vec::new(), b"Connection to host closed".to_vec()).unwrap_err();
        assert!(matches!(r, RemoteCmdError::Ssh(_)));
        assert!(r.to_string().contains("Connection to host closed"));
    }

    #[test]
    fn classify_other_nonzero_carries_stderr() {
        let r = classify(Some(2), Vec::new(), b"oops".to_vec()).unwrap_err();
        match r {
            RemoteCmdError::Failed { exit_code, stderr } => {
                assert_eq!(exit_code, 2);
                assert_eq!(stderr, "oops");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_signal_is_ssh_error() {
        let r = classify(None, Vec::new(), Vec::new()).unwrap_err();
        assert!(matches!(r, RemoteCmdError::Ssh(_)));
    }

    fn policy(is_root: bool, sudo: bool) -> PrivilegePolicy {
        PrivilegePolicy { is_root, sudo }
    }

    #[test]
    fn remote_command_renders_every_policy_branch() {
        assert_eq!(
            remote_command("crontab -l", Privilege::User, policy(false, false), "list").unwrap(),
            "crontab -l"
        );
        assert_eq!(
            remote_command("crontab -l", Privilege::Root, policy(true, false), "list").unwrap(),
            "crontab -l"
        );
        assert_eq!(
            remote_command("crontab -l", Privilege::Root, policy(false, true), "list").unwrap(),
            "sudo -n -- crontab -l"
        );
        let err = remote_command(
            "crontab -l",
            Privilege::Root,
            policy(false, false),
            "read /etc/cron.d/x",
        )
        .expect_err("must refuse");
        match err {
            CoreError::NeedsRoot { operation } => assert_eq!(operation, "read /etc/cron.d/x"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }
    }

    /// The exact bytes each `Host` file operation puts on the wire. A path
    /// containing a single quote goes through two rounds of quoting (the
    /// script, then `sh -c`) and must survive both.
    #[test]
    fn file_op_commands_are_pinned() {
        assert_eq!(
            file_op_command(
                FileOp::Read,
                "/etc/cron.d/x",
                Privilege::User,
                policy(false, false)
            )
            .unwrap(),
            r#"sh -c 'if test -e '\''/etc/cron.d/x'\''; then cat '\''/etc/cron.d/x'\''; else exit 66; fi'"#
        );
        assert_eq!(
            file_op_command(
                FileOp::Write { mode: 0o644 },
                "/etc/cron.d/x",
                Privilege::Root,
                policy(false, true)
            )
            .unwrap(),
            r#"sudo -n -- sh -c 'd=$(dirname '\''/etc/cron.d/x'\'') && t=$(mktemp "$d/.shuvjobs.XXXXXX") && { cat > "$t" && chmod 644 "$t" && mv -f "$t" '\''/etc/cron.d/x'\'' || { rm -f "$t"; exit 1; }; }'"#
        );
        assert_eq!(
            file_op_command(
                FileOp::Remove,
                "/tmp/it's here/x",
                Privilege::Root,
                policy(false, true)
            )
            .unwrap(),
            r#"sudo -n -- sh -c 'if test -e '\''/tmp/it'\''\'\'''\''s here/x'\''; then rm -f '\''/tmp/it'\''\'\'''\''s here/x'\''; else exit 66; fi'"#
        );
        assert_eq!(
            file_op_command(
                FileOp::Exists,
                "/etc/anacrontab",
                Privilege::User,
                policy(false, false)
            )
            .unwrap(),
            r#"sh -c 'test -e '\''/etc/anacrontab'\'''"#
        );
        assert_eq!(
            file_op_command(
                FileOp::ListDir,
                "/etc/cron.d",
                Privilege::User,
                policy(false, false)
            )
            .unwrap(),
            r#"sh -c 'if test -d '\''/etc/cron.d'\''; then ls -1A '\''/etc/cron.d'\''; else exit 66; fi'"#
        );
        assert_eq!(
            file_op_command(
                FileOp::MkdirAll,
                "/etc/systemd/system",
                Privilege::Root,
                policy(true, false)
            )
            .unwrap(),
            r#"sh -c 'mkdir -p '\''/etc/systemd/system'\'''"#
        );
        let err = file_op_command(
            FileOp::Write { mode: 0o644 },
            "/etc/cron.d/x",
            Privilege::Root,
            policy(false, false),
        )
        .expect_err("must refuse");
        match err {
            CoreError::NeedsRoot { operation } => assert_eq!(operation, "write /etc/cron.d/x"),
            other => panic!("expected NeedsRoot, got {other:?}"),
        }
    }

    #[test]
    fn classify_raw_keeps_nonzero_exits_as_output() {
        let out = classify_raw(Some(0), b"hi\n".to_vec(), String::new()).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_str(), "hi\n");

        let out = classify_raw(Some(1), Vec::new(), "nope\n".to_string()).unwrap();
        assert_eq!(out.code, Some(1));
        assert_eq!(out.stderr, "nope\n");
        assert!(!out.success());
    }

    #[test]
    fn classify_raw_maps_transport_failures_to_ssh_errors() {
        let err = classify_raw(Some(255), Vec::new(), "Permission denied".into()).unwrap_err();
        assert!(matches!(err, RemoteCmdError::Ssh(_)));
        assert!(err.to_string().contains("Permission denied"));

        let err = classify_raw(None, Vec::new(), String::new()).unwrap_err();
        assert!(matches!(err, RemoteCmdError::Ssh(_)));
        assert!(err.to_string().contains("signal"));
    }

    #[test]
    fn uname_maps_to_host_os() {
        assert_eq!(parse_host_os("Darwin\n"), HostOs::MacOs);
        assert_eq!(parse_host_os("Linux\n"), HostOs::Linux);
        assert_eq!(parse_host_os("FreeBSD"), HostOs::Other);
    }

    #[test]
    fn with_sudo_sets_the_policy_flag_and_label_is_the_host() {
        let coll = collector().with_sudo(true);
        assert!(coll.sudo);
        assert_eq!(coll.label(), "alice@host");
        assert!(!collector().sudo);
    }

    #[test]
    fn parses_remote_utc_offsets() {
        assert_eq!(parse_utc_offset("+0530").unwrap().local_minus_utc(), 19_800);
        assert_eq!(
            parse_utc_offset("-07:00").unwrap().local_minus_utc(),
            -25_200
        );
        assert!(parse_utc_offset("PDT").is_none());
        assert!(parse_utc_offset("+2460").is_none());
    }

    #[test]
    fn at_propagates_ssh_transport_errors() {
        let runner = FixtureRunner::always(Fallback::SshDown);
        let result = collect_at(&runner, &Utc);
        assert!(matches!(result, Err(RemoteSourceError::Transport(_))));
    }

    const SYSTEMD_LIST: &str = r#"[
        {"next":1775919186922121,"left":0,"last":1775499594111976,"passed":0,"unit":"logrotate.timer","activates":"logrotate.service"}
    ]"#;
    // Batched `systemctl show` replies: one block per unit, `Id=` first.
    const SYSTEMD_SHOW_TIMER: &str = "\
Id=logrotate.timer
ActiveState=active
FragmentPath=/usr/lib/systemd/system/logrotate.timer
UnitFileState=enabled
TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Sat 2026-04-11 17:04:57 +03 }
Result=success
";
    const SYSTEMD_SHOW_SERVICE: &str = "\
Id=logrotate.service
ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=40202351753
ExecMainExitTimestampMonotonic=40202376368
";

    #[test]
    fn systemd_runs_full_pipeline() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST,
        );
        fx.insert(
            "systemctl show 'logrotate.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_SHOW_TIMER,
        );
        fx.insert(
            "systemctl show 'logrotate.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_SHOW_SERVICE,
        );

        // No `systemctl --user` fixture: the runner's Failed fallback
        // stands in for "Failed to connect to bus" on a non-lingering host.
        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.id, "logrotate.timer");
        assert_eq!(t.command, "/usr/sbin/logrotate /etc/logrotate.conf");
        assert!(matches!(t.schedule, ScheduleType::Calendar(ref s) if s == "*-*-* 00:00:00"));
        assert!(matches!(t.last_status, Some(TaskStatus::Success)));
        assert_eq!(
            t.last_duration,
            Some(std::time::Duration::from_micros(24_615))
        );
        assert_eq!(
            t.location.as_deref(),
            Some("/usr/lib/systemd/system/logrotate.timer")
        );
        assert_eq!(t.enabled, Some(true));
    }

    // Captured from `systemctl --user list-timers --all --output=json
    // --no-pager` on a host with a running user manager.
    const SYSTEMD_USER_LIST: &str = r#"[
        {"next":1788394500000000,"left":1788394500000000,"last":1788308100017251,"passed":15000021453,"unit":"radar-daily.timer","activates":"radar-daily.service"}
    ]"#;
    const SYSTEMD_USER_SHOW_TIMER: &str = "\
Id=radar-daily.timer
ActiveState=active
FragmentPath=/home/alice/.config/systemd/user/radar-daily.timer
UnitFileState=enabled
TimersCalendar={ OnCalendar=*-*-* 09:15:00 ; next_elapse=Thu 2026-09-03 09:15:00 PDT }
Result=success
";
    const SYSTEMD_USER_SHOW_SERVICE: &str = "\
Id=radar-daily.service
ExecStart={ path=/home/alice/.local/bin/radar ; argv[]=/home/alice/.local/bin/radar daily ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=40202351753
ExecMainExitTimestampMonotonic=40202376368
";

    /// A remote host with lingering enabled: both managers answer, and the
    /// user timer lands with a prefixed id alongside the system one.
    #[test]
    fn systemd_collects_user_scope_when_the_user_bus_answers() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST,
        );
        fx.insert(
            "systemctl --user list-timers --all --output=json --no-pager",
            SYSTEMD_USER_LIST,
        );
        fx.insert(
            "systemctl show 'logrotate.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_SHOW_TIMER,
        );
        fx.insert(
            "systemctl show 'logrotate.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_SHOW_SERVICE,
        );
        fx.insert(
            "systemctl --user show 'radar-daily.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_USER_SHOW_TIMER,
        );
        fx.insert(
            "systemctl --user show 'radar-daily.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_USER_SHOW_SERVICE,
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "logrotate.timer");
        assert_eq!(tasks[0].name, "logrotate");

        let user = &tasks[1];
        assert_eq!(user.id, "user/radar-daily.timer");
        assert_eq!(user.name, "radar-daily (user)");
        assert_eq!(user.command, "/home/alice/.local/bin/radar daily");
        assert!(matches!(user.schedule, ScheduleType::Calendar(ref s) if s == "*-*-* 09:15:00"));
        assert!(matches!(user.last_status, Some(TaskStatus::Success)));
        assert_eq!(
            user.location.as_deref(),
            Some("/home/alice/.config/systemd/user/radar-daily.timer")
        );
        assert_eq!(user.enabled, Some(true));
    }

    /// No user bus (the common non-lingering SSH case): `systemctl --user`
    /// exits non-zero and we report the system scope alone.
    #[test]
    fn systemd_skips_user_scope_when_the_user_bus_is_absent() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST,
        );
        fx.insert(
            "systemctl show 'logrotate.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_SHOW_TIMER,
        );
        fx.insert(
            "systemctl show 'logrotate.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_SHOW_SERVICE,
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "logrotate.timer");
    }

    // Captured from `systemctl list-unit-files --type=timer --all
    // --output=json --no-pager`. `btrbk.timer` is disabled and stopped,
    // so it never appears in `list-timers`.
    const SYSTEMD_UNIT_FILES: &str = r#"[
        {"unit_file":"btrbk.timer","state":"disabled","preset":"disabled"},
        {"unit_file":"btrfs-scrub@.timer","state":"disabled","preset":"disabled"},
        {"unit_file":"logrotate.timer","state":"enabled","preset":"disabled"}
    ]"#;
    const SYSTEMD_SHOW_TIMER_WITH_DISABLED: &str = "\
Id=btrbk.timer
ActiveState=inactive
FragmentPath=/usr/lib/systemd/system/btrbk.timer
UnitFileState=disabled
TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=(null) }
Result=success

Id=logrotate.timer
ActiveState=active
FragmentPath=/usr/lib/systemd/system/logrotate.timer
UnitFileState=enabled
TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Wed 2026-09-02 00:00:00 PDT }
Result=success
";
    const SYSTEMD_SHOW_SERVICE_WITH_DISABLED: &str = "\
Id=btrbk.service
ExecStart={ path=/usr/bin/btrbk ; argv[]=/usr/bin/btrbk run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=0
ExecMainExitTimestampMonotonic=0

Id=logrotate.service
ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=40202351753
ExecMainExitTimestampMonotonic=40202376368
";

    /// The defect this listing exists for: `btrbk.timer` is disabled and
    /// stopped, so `list-timers` does not mention it, and before the
    /// unit-file pass `shuvjobs list` could not see it at all.
    #[test]
    fn systemd_reports_timers_only_list_unit_files_knows_about() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST,
        );
        fx.insert(
            "systemctl list-unit-files --type=timer --all --output=json --no-pager",
            SYSTEMD_UNIT_FILES,
        );
        fx.insert(
            "systemctl show 'btrbk.timer' 'logrotate.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_SHOW_TIMER_WITH_DISABLED,
        );
        fx.insert(
            "systemctl show 'btrbk.service' 'logrotate.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_SHOW_SERVICE_WITH_DISABLED,
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        // The template unit is skipped and the listed timer is not doubled.
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "logrotate.timer");
        assert_eq!(tasks[0].enabled, Some(true));

        let btrbk = &tasks[1];
        assert_eq!(btrbk.id, "btrbk.timer");
        assert_eq!(btrbk.name, "btrbk");
        assert_eq!(btrbk.enabled, Some(false));
        assert_eq!(btrbk.command, "/usr/bin/btrbk run");
        assert!(matches!(btrbk.schedule, ScheduleType::Calendar(ref c) if c == "*-*-* 00:00:00"));
        assert_eq!(
            btrbk.location.as_deref(),
            Some("/usr/lib/systemd/system/btrbk.timer")
        );
        assert!(btrbk.next_run.is_none());
        assert!(btrbk.last_run.is_none());
    }

    /// The user manager has its own inventory, and a unit name present in
    /// both scopes stays two distinct tasks.
    #[test]
    fn systemd_reads_the_user_unit_file_inventory_too() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert("systemctl list-timers --all --output=json --no-pager", "[]");
        fx.insert(
            "systemctl list-unit-files --type=timer --all --output=json --no-pager",
            r#"[{"unit_file":"logrotate.timer","state":"disabled","preset":null}]"#,
        );
        fx.insert(
            "systemctl --user list-unit-files --type=timer --all --output=json --no-pager",
            r#"[{"unit_file":"logrotate.timer","state":"enabled","preset":null}]"#,
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "logrotate.timer");
        assert_eq!(tasks[0].enabled, Some(false));
        assert_eq!(tasks[1].id, "user/logrotate.timer");
        assert_eq!(tasks[1].name, "logrotate (user)");
        assert_eq!(tasks[1].enabled, Some(true));
    }

    #[test]
    fn systemctl_cmds_carry_the_user_flag_for_user_scope() {
        assert_eq!(
            systemctl_list_timers_cmd(Scope::User),
            "systemctl --user list-timers --all --output=json --no-pager"
        );
        assert_eq!(
            systemctl_list_timers_cmd(Scope::System),
            "systemctl list-timers --all --output=json --no-pager"
        );
        assert_eq!(
            systemctl_list_unit_files_cmd(Scope::System),
            "systemctl list-unit-files --type=timer --all --output=json --no-pager"
        );
        assert_eq!(
            systemctl_list_unit_files_cmd(Scope::User),
            "systemctl --user list-unit-files --type=timer --all --output=json --no-pager"
        );
        assert_eq!(
            systemctl_show_cmd(Scope::User, &["radar-daily.timer".into()], "Result"),
            "systemctl --user show 'radar-daily.timer' --property=Result --no-pager"
        );
        assert_eq!(
            systemctl_show_cmd(Scope::System, &["logrotate.timer".into()], "Result"),
            "systemctl show 'logrotate.timer' --property=Result --no-pager"
        );
    }

    // Two system timers: the batched `show` must carry both units in one
    // command, and each block must land on the task named by its `Id=`.
    const SYSTEMD_LIST_TWO: &str = r#"[
        {"next":1775919186922121,"left":0,"last":1775499594111976,"passed":0,"unit":"logrotate.timer","activates":"logrotate.service"},
        {"next":1775919186922121,"left":0,"last":0,"passed":0,"unit":"man-db.timer","activates":"man-db.service"}
    ]"#;
    // Deliberately reversed relative to the request order, and with the
    // service blocks in a different order again.
    const SYSTEMD_SHOW_TIMER_BATCH: &str = "\
Id=man-db.timer
TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Wed 2026-09-02 00:00:00 PDT }
Result=success

Id=logrotate.timer
TimersMonotonic={ OnBootSec=15min ; next_elapse=4h 12min left }
Result=success
";
    const SYSTEMD_SHOW_SERVICE_BATCH: &str = "\
Id=man-db.service
ExecStart={ path=/usr/bin/mandb ; argv[]=/usr/bin/mandb --quiet ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=0
ExecMainExitTimestampMonotonic=0

Id=logrotate.service
ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=40202351753
ExecMainExitTimestampMonotonic=40202376368
";

    #[test]
    fn systemd_batches_show_and_keys_blocks_by_id() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST_TWO,
        );
        // Units are sorted, so `logrotate` precedes `man-db`; one command
        // for both timers and one for both services.
        fx.insert(
            "systemctl show 'logrotate.timer' 'man-db.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            SYSTEMD_SHOW_TIMER_BATCH,
        );
        fx.insert(
            "systemctl show 'logrotate.service' 'man-db.service' --property=Id,ExecStart,Result,ActiveState,SubState,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic --no-pager",
            SYSTEMD_SHOW_SERVICE_BATCH,
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 2);

        let logrotate = &tasks[0];
        assert_eq!(logrotate.id, "logrotate.timer");
        assert_eq!(logrotate.command, "/usr/sbin/logrotate /etc/logrotate.conf");
        assert_eq!(
            logrotate.schedule,
            ScheduleType::Interval(std::time::Duration::from_secs(900))
        );
        assert_eq!(
            logrotate.last_duration,
            Some(std::time::Duration::from_micros(24_615))
        );

        let man_db = &tasks[1];
        assert_eq!(man_db.id, "man-db.timer");
        assert_eq!(man_db.command, "/usr/bin/mandb --quiet");
        assert!(matches!(man_db.schedule, ScheduleType::Calendar(ref c) if c == "*-*-* 00:00:00"));
        // Never triggered and clean: no status, as before batching.
        assert!(man_db.last_status.is_none());
    }

    /// A unit absent from the batched reply keeps its `list-timers` data
    /// rather than borrowing the neighbouring block.
    #[test]
    fn systemd_keeps_list_timers_data_when_a_block_is_missing() {
        let mut fx = HashMap::new();
        fx.insert(
            "command -v systemctl >/dev/null 2>&1 && echo present",
            "present\n",
        );
        fx.insert(
            "systemctl list-timers --all --output=json --no-pager",
            SYSTEMD_LIST_TWO,
        );
        fx.insert(
            "systemctl show 'logrotate.timer' 'man-db.timer' --property=Id,TimersCalendar,TimersMonotonic,Result,FragmentPath,UnitFileState,ActiveState --no-pager",
            "Id=man-db.timer\nTimersCalendar={ OnCalendar=daily ; next_elapse=Wed 2026-09-02 00:00:00 PDT }\nResult=success\n",
        );

        let tasks = collect_systemd(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 2);
        // No timer block and no service block: the bound unit name stays.
        assert_eq!(tasks[0].id, "logrotate.timer");
        assert_eq!(tasks[0].command, "logrotate.service");
        assert_eq!(tasks[0].schedule, ScheduleType::Calendar(String::new()));
        assert!(matches!(tasks[1].schedule, ScheduleType::Calendar(ref c) if c == "daily"));
    }

    #[test]
    fn show_jobs_are_two_per_scope_and_chunked() {
        let tasks = SystemdAdapter::parse_list_timers(SYSTEMD_LIST_TWO).unwrap();
        let jobs = show_jobs(&tasks);
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].timers);
        assert_eq!(jobs[0].scope, Scope::System);
        assert_eq!(jobs[0].units, ["logrotate.timer", "man-db.timer"]);
        assert!(!jobs[1].timers);
        assert_eq!(jobs[1].units, ["logrotate.service", "man-db.service"]);

        // 150 timers: 3 timer chunks + 3 service chunks at 64 units each.
        let many: Vec<_> = (0..150)
            .map(|i| {
                let mut t = tasks[0].clone();
                t.id = format!("t{i:03}.timer");
                t.command = format!("t{i:03}.service");
                t
            })
            .collect();
        let jobs = show_jobs(&many);
        assert_eq!(jobs.len(), 6);
        assert_eq!(
            jobs.iter().map(|j| j.units.len()).collect::<Vec<_>>(),
            [64, 64, 22, 64, 64, 22]
        );
        assert!(jobs.iter().all(|j| j.scope == Scope::System));
    }

    #[test]
    fn show_jobs_are_empty_without_timers() {
        assert!(show_jobs(&[]).is_empty());
    }

    #[test]
    fn systemctl_show_cmd_quotes_every_unit_in_a_batch() {
        assert_eq!(
            systemctl_show_cmd(
                Scope::System,
                &["a.timer".to_string(), "b.timer".to_string()],
                "Id,Result"
            ),
            "systemctl show 'a.timer' 'b.timer' --property=Id,Result --no-pager"
        );
    }

    #[test]
    fn systemd_unavailable_when_systemctl_missing() {
        let err = collect_systemd(&FixtureRunner::always(Fallback::NotFound)).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    #[test]
    fn cron_reads_own_crontab_without_dash_u() {
        let mut fx = HashMap::new();
        fx.insert("command -v crontab >/dev/null 2>&1", "");
        fx.insert("id -un 2>/dev/null", "alice\n");
        fx.insert(
            "cat /etc/passwd 2>/dev/null",
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/zsh\n",
        );
        fx.insert(
            "crontab -l 2>/dev/null",
            "0 9 * * 1-5 /home/alice/bin/standup\n",
        );
        // root's crontab is unreadable as alice: the runner returns Failed{1}.
        let tasks = collect_cron(&FixtureRunner::new(fx), remote_now()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "user:alice:1");
        assert_eq!(tasks[0].command, "/home/alice/bin/standup");
    }

    #[test]
    fn crontab_list_cmd_shapes() {
        assert_eq!(
            crontab_list_cmd("alice", Some("alice")),
            "crontab -l 2>/dev/null"
        );
        assert_eq!(
            crontab_list_cmd("bob", Some("alice")),
            "crontab -l -u bob 2>/dev/null"
        );
    }

    #[test]
    fn remote_current_user_rejects_unsafe_names() {
        let mut fx = HashMap::new();
        fx.insert("id -un 2>/dev/null", "evil; rm -rf /\n");
        assert_eq!(remote_current_user(&FixtureRunner::new(fx)).unwrap(), None);

        let mut fx = HashMap::new();
        fx.insert("id -un 2>/dev/null", "alice\n");
        assert_eq!(
            remote_current_user(&FixtureRunner::new(fx)).unwrap(),
            Some("alice".to_string())
        );
    }

    const ETC_CRONTAB: &str = "\
# m h dom mon dow user  command
17 *    * * *   root    cd / && run-parts --report /etc/cron.hourly
@reboot root    /usr/local/bin/warmup-cache
";

    #[test]
    fn cron_parses_etc_crontab_through_the_runner() {
        let mut fx = HashMap::new();
        fx.insert("cat /etc/crontab 2>/dev/null", ETC_CRONTAB);
        let tasks = collect_cron(&FixtureRunner::new(fx), remote_now()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(matches!(tasks[0].schedule, ScheduleType::Cron(ref s) if s == "17 * * * *"));
        assert!(matches!(tasks[1].schedule, ScheduleType::Cron(ref s) if s == "@reboot"));
    }

    #[test]
    fn cron_next_run_uses_the_remote_offset() {
        // 01:30 on the remote's +03:00 clock; the next 02:00 there is
        // 23:00 UTC on the previous day.
        let now = FixedOffset::east_opt(3 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 4, 14, 1, 30, 0)
            .unwrap();
        let mut fx = HashMap::new();
        fx.insert("cat /etc/crontab 2>/dev/null", "0 2 * * * root /bin/true\n");
        let tasks = collect_cron(&FixtureRunner::new(fx), now).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 13, 23, 0, 0).unwrap())
        );
    }

    #[test]
    fn cron_run_parts_reports_the_executable_bit_and_the_script_path() {
        let mut fx = HashMap::new();
        fx.insert(
            "ls -1 '/etc/cron.daily' 2>/dev/null",
            "logrotate\nmlocate\n",
        );
        fx.insert(
            "find '/etc/cron.daily' -maxdepth 1 -type f -perm -u+x 2>/dev/null",
            "/etc/cron.daily/logrotate\n",
        );
        let tasks = collect_cron(&FixtureRunner::new(fx), remote_now()).unwrap();
        assert_eq!(tasks.len(), 2, "got {tasks:?}");
        assert_eq!(tasks[0].id, "/etc/cron.daily/logrotate");
        assert_eq!(
            tasks[0].location.as_deref(),
            Some("/etc/cron.daily/logrotate")
        );
        assert_eq!(tasks[0].enabled, Some(true));
        assert_eq!(tasks[1].id, "/etc/cron.daily/mlocate");
        assert_eq!(tasks[1].enabled, Some(false));
    }

    /// Without a usable `find` the executable bit is unknown, not false.
    #[test]
    fn cron_run_parts_leaves_the_executable_bit_unknown_without_find() {
        let mut fx = HashMap::new();
        fx.insert("ls -1 '/etc/cron.daily' 2>/dev/null", "logrotate\n");
        let tasks = collect_cron(&FixtureRunner::new(fx), remote_now()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].enabled, None);
    }

    #[test]
    fn cron_unavailable_when_nothing_found() {
        let err = collect_cron(&FixtureRunner::always(Fallback::Failed), remote_now()).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    const ATQ_OUTPUT: &str = "12\tTue Apr 14 14:00:00 2026 a alice\n";
    const AT_C_OUTPUT: &str = "#!/bin/sh\n\
                                # atrun uid=1000 gid=1000\n\
                                umask 22\n\
                                ${SHELL:-/bin/sh} << 'marcinABC'\n\
                                \n\
                                echo hello\n\
                                marcinABC\n";

    #[test]
    fn at_parses_atq_and_at_c() {
        let mut fx = HashMap::new();
        fx.insert("command -v atq >/dev/null 2>&1", "");
        fx.insert("atq", ATQ_OUTPUT);
        fx.insert("at -c 12 2>/dev/null", AT_C_OUTPUT);
        let tasks = collect_at(&FixtureRunner::new(fx), &Utc).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "at:12");
        assert_eq!(tasks[0].command, "echo hello");
    }

    #[test]
    fn at_unavailable_when_atq_missing() {
        let err = collect_at(&FixtureRunner::always(Fallback::NotFound), &Utc).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    // -- anacron --

    const ANACRONTAB: &str = "\
1 5 cron.daily run-parts /etc/cron.daily
@monthly 45 cron.monthly run-parts /etc/cron.monthly
";

    #[test]
    fn anacron_parses_anacrontab_and_spool() {
        let mut fx = HashMap::new();
        fx.insert("cat /etc/anacrontab 2>/dev/null", ANACRONTAB);
        fx.insert(
            "cat /var/spool/anacron/cron.daily 2>/dev/null",
            "20260410\n",
        );
        let tasks = collect_anacron(&FixtureRunner::new(fx)).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "cron.daily");
        assert!(tasks[0].last_run.is_some());
        // The 2026-04-10 spool date is long past, so the daily job is due now.
        assert!(tasks[0].next_run > tasks[0].last_run);
        assert!(tasks[0].next_run.unwrap() <= Utc::now());
        // monthly entry has no spool fixture
        assert!(tasks[1].last_run.is_none());
    }

    #[test]
    fn anacron_unavailable_when_anacrontab_missing() {
        let err = collect_anacron(&FixtureRunner::always(Fallback::Failed)).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    const LAUNCHCTL_LIST: &str = "PID\tStatus\tLabel\n\
                                   -\t0\tcom.example.heartbeat\n";
    const PLIST_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.example.heartbeat</string>
    <key>ProgramArguments</key>
    <array><string>/usr/local/bin/heartbeat</string></array>
    <key>StartInterval</key><integer>900</integer>
</dict>
</plist>"#;

    #[test]
    fn launchd_parses_listing_and_plists() {
        let mut fx = HashMap::new();
        fx.insert("command -v launchctl >/dev/null 2>&1", "");
        fx.insert("launchctl list", LAUNCHCTL_LIST);
        fx.insert(
            LAUNCHD_FIND_CMD,
            "/Library/LaunchAgents/com.example.heartbeat.plist\n",
        );
        fx.insert(
            "cat '/Library/LaunchAgents/com.example.heartbeat.plist' 2>/dev/null",
            PLIST_BODY,
        );
        let tasks = collect_launchd(&FixtureRunner::new(fx), remote_now()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "com.example.heartbeat");
        // launchctl reported exit 0 → Success
        assert!(matches!(tasks[0].last_status, Some(TaskStatus::Success)));
        assert!(matches!(
            tasks[0].schedule,
            ScheduleType::Interval(d) if d == std::time::Duration::from_secs(900)
        ));
        assert_eq!(
            tasks[0].location.as_deref(),
            Some("/Library/LaunchAgents/com.example.heartbeat.plist")
        );
        // Present in `launchctl list` — loaded, so enabled.
        assert_eq!(tasks[0].enabled, Some(true));
    }

    #[test]
    fn launchd_unavailable_when_launchctl_missing() {
        let err =
            collect_launchd(&FixtureRunner::always(Fallback::NotFound), remote_now()).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    #[test]
    fn shell_quote_wraps_in_single_quotes() {
        assert_eq!(shell_quote("simple"), "'simple'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_quote("foo'bar"), r"'foo'\''bar'");
    }

    #[test]
    fn shell_safe_username_rejects_metacharacters() {
        assert!(shell_safe_username("alice"));
        assert!(shell_safe_username("user_1"));
        assert!(!shell_safe_username("alice; rm -rf /"));
        assert!(!shell_safe_username(""));
    }

    #[test]
    fn parallel_map_preserves_input_order_across_chunks() {
        // Larger than the cap so the workers run in waves; confirms
        // results land in input order, not join order.
        let inputs: Vec<i32> = (0..20).collect();
        let outputs = parallel_map(&inputs, 4, |x| x * 2);
        assert_eq!(outputs, (0..20).map(|x| x * 2).collect::<Vec<_>>());
    }

    #[test]
    fn parallel_map_handles_empty_input() {
        let inputs: Vec<i32> = Vec::new();
        let outputs = parallel_map(&inputs, 8, |x| *x);
        assert!(outputs.is_empty());
    }

    #[test]
    fn parallel_map_handles_input_smaller_than_cap() {
        let inputs = vec![1, 2, 3];
        let outputs = parallel_map(&inputs, 8, |x| x + 10);
        assert_eq!(outputs, vec![11, 12, 13]);
    }

    #[test]
    fn parallel_map_actually_runs_in_parallel() {
        // 8 workers × 100ms sleeps; serial bound is 800ms. Threshold is
        // loose enough not to flake on a loaded CI box.
        use std::time::Instant;
        let inputs: Vec<i32> = (0..8).collect();
        let start = Instant::now();
        let _ = parallel_map(&inputs, 8, |_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "expected parallel speedup, took {elapsed:?}"
        );
    }

    fn empty_launchd_task() -> ScheduledTask {
        ScheduledTask {
            id: "com.example.heartbeat".into(),
            name: "com.example.heartbeat".into(),
            source: shuvjobs_core::TaskSourceKind::Launchd,
            schedule: ScheduleType::Interval(std::time::Duration::from_secs(900)),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "/usr/local/bin/heartbeat".into(),
            location: None,
            enabled: None,
        }
    }

    #[test]
    fn apply_launchctl_runtime_zero_exit_is_success() {
        let mut task = empty_launchd_task();
        apply_launchctl_runtime(
            &mut task,
            Some(&LaunchctlEntry {
                pid: None,
                last_exit_status: Some(0),
                label: "com.example.heartbeat".into(),
            }),
        );
        assert!(matches!(task.last_status, Some(TaskStatus::Success)));
    }

    #[test]
    fn apply_launchctl_runtime_nonzero_exit_carries_code() {
        let mut task = empty_launchd_task();
        apply_launchctl_runtime(
            &mut task,
            Some(&LaunchctlEntry {
                pid: None,
                last_exit_status: Some(2),
                label: "com.example.heartbeat".into(),
            }),
        );
        match task.last_status {
            Some(TaskStatus::Failed(ref msg)) => assert_eq!(msg, "exit 2"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn apply_launchctl_runtime_pid_without_exit_is_running() {
        let mut task = empty_launchd_task();
        apply_launchctl_runtime(
            &mut task,
            Some(&LaunchctlEntry {
                pid: Some(1234),
                last_exit_status: None,
                label: "com.example.heartbeat".into(),
            }),
        );
        assert!(matches!(task.last_status, Some(TaskStatus::Running)));
    }

    #[test]
    fn apply_launchctl_runtime_no_entry_is_no_op() {
        let mut task = empty_launchd_task();
        let snapshot = task.clone();
        apply_launchctl_runtime(&mut task, None);
        assert_eq!(task, snapshot);
    }
}
