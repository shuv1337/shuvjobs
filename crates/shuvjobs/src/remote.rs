//! Remote collection over SSH. No binary is uploaded — we shell out to
//! the remote host's own `systemctl` / `crontab` / `atq` / `cat` / `find`
//! and feed the captured stdout into the same `parse_*` functions the
//! local adapters use.
//!
//! SSH connection multiplexing (`ControlMaster=auto` + `ControlPersist`)
//! keeps the per-call cost negligible after the initial handshake, and
//! `BatchMode=yes` makes us fail fast on missing key auth instead of
//! hanging on a password prompt.

use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{Datelike, FixedOffset, TimeZone, Utc};
use shuvjobs_adapters::{
    launchd::LaunchctlEntry, AnacronAdapter, AtAdapter, CronAdapter, LaunchdAdapter, SystemdAdapter,
};
use shuvjobs_core::{ScheduleType, ScheduledTask, TaskStatus};

/// Cap below sshd's default `MaxSessions` (10) so we never get refused
/// channels on the remote side, even on the worst case of many small
/// commands flying over the same multiplex master.
const REMOTE_PARALLELISM: usize = 8;

/// Construct once per `--host` invocation and reuse for the lifetime of
/// the run — the control socket lives across `run_command` calls.
#[derive(Debug, Clone)]
pub struct RemoteCollector {
    pub host: String,
    pub port: Option<u16>,
    pub key_path: Option<PathBuf>,
    /// SSH multiplex control socket. Per-process so concurrent `shuvjobs`
    /// invocations against the same host don't collide.
    control_path: PathBuf,
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

impl RemoteCollector {
    pub fn new(host: String, port: Option<u16>, key_path: Option<PathBuf>) -> Self {
        let control_path =
            std::env::temp_dir().join(format!("shuvjobs-ssh-{}", std::process::id()));
        Self {
            host,
            port,
            key_path,
            control_path,
        }
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

    /// Run `cmd` on the remote host and return its stdout.
    pub fn run_command(&self, cmd: &str) -> std::result::Result<String, RemoteCmdError> {
        let out = Command::new("ssh")
            .args(self.ssh_argv(cmd))
            .output()
            .map_err(|e| RemoteCmdError::Ssh(anyhow!("invoking ssh: {e}")))?;
        classify(out.status.code(), out.stdout, out.stderr)
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

        let systemd_result = self.collect_systemd_parallel();
        let cron_result = self.collect_cron_parallel(remote_offset);
        let launchd_result = self.collect_launchd_parallel();

        let mut runner = |cmd: &str| self.run_command(cmd);
        let at_result = collect_at_via(&mut runner, &remote_offset);
        let anacron_result = collect_anacron_via(&mut runner);

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

        self.close();
        Ok(tasks)
    }

    // Parallel per-source methods. The serial closure-based equivalents
    // (`collect_<source>_via` below) still exist for fixture-driven tests.

    fn collect_systemd_parallel(
        &self,
    ) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
        match self.run_command("command -v systemctl >/dev/null 2>&1 && echo present") {
            Ok(s) if s.trim() == "present" => {}
            Ok(_) => return Err(RemoteSourceError::Unavailable),
            Err(RemoteCmdError::NotFound) | Err(RemoteCmdError::Failed { .. }) => {
                return Err(RemoteSourceError::Unavailable);
            }
            Err(RemoteCmdError::Ssh(e)) => return Err(RemoteSourceError::Transport(e)),
        }

        let json = match self.run_command("systemctl list-timers --all --output=json --no-pager") {
            Ok(s) => s,
            Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
            Err(e) => return Err(remote_source_error("systemctl list-timers", e)),
        };
        let mut tasks = SystemdAdapter::parse_list_timers(&json)
            .map_err(|e| RemoteSourceError::Other(anyhow!("parse list-timers: {e}")))?;

        // Two `systemctl show` calls per timer, fanned out — 30 serial
        // round-trips become ceil(N/8) waves of 8 in parallel.
        let work: Vec<(String, String)> = tasks
            .iter()
            .map(|t| (t.id.clone(), t.command.clone()))
            .collect();

        let results: Vec<(Option<String>, Option<String>)> =
            parallel_map(&work, REMOTE_PARALLELISM, |(unit_id, service)| {
                let timer_text = optional_remote_output(self.run_command(&format!(
                    "systemctl show {} --property=TimersCalendar,TimersMonotonic,Result --no-pager",
                    shell_quote(unit_id)
                )));
                let service_text = if !service.is_empty() {
                    optional_remote_output(self.run_command(&format!(
                        "systemctl show {} --property=ExecStart --no-pager",
                        shell_quote(service)
                    )))
                } else {
                    Ok(None)
                };
                (timer_text, service_text)
            })
            .into_iter()
            .map(|(timer, service)| Ok((timer?, service?)))
            .collect::<std::result::Result<Vec<_>, RemoteSourceError>>()?;

        for (i, (timer_text, service_text)) in results.into_iter().enumerate() {
            apply_systemd_show(
                &mut tasks[i],
                timer_text.as_deref(),
                service_text.as_deref(),
            );
        }

        Ok(tasks)
    }

    fn collect_cron_parallel(
        &self,
        remote_offset: FixedOffset,
    ) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
        let mut tasks = Vec::new();
        let mut any_present = false;
        let now = Utc::now().with_timezone(&remote_offset);

        if let Some(text) =
            optional_remote_output(self.run_command("cat /etc/crontab 2>/dev/null"))?
        {
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
        if let Some(listing) =
            optional_remote_output(self.run_command("ls -1 /etc/cron.d 2>/dev/null"))?
        {
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
                (path, optional_remote_output(self.run_command(&cmd)))
            });
            for (path, content) in cron_d_results {
                if let Some(text) = content? {
                    tasks.extend(CronAdapter::parse_crontab_at(&text, &path, true, now));
                }
            }
        }

        // run-parts directories. Just 4 ls calls — keep serial.
        for (period, dir) in [
            ("hourly", "/etc/cron.hourly"),
            ("daily", "/etc/cron.daily"),
            ("weekly", "/etc/cron.weekly"),
            ("monthly", "/etc/cron.monthly"),
        ] {
            let cmd = format!("ls -1 {} 2>/dev/null", shell_quote(dir));
            if let Some(listing) = optional_remote_output(self.run_command(&cmd))? {
                any_present = true;
                let scripts: Vec<&str> = listing
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if !scripts.is_empty() {
                    tasks.extend(CronAdapter::parse_run_parts_at(period, &scripts, dir, now));
                }
            }
        }

        // Per-user crontabs in parallel.
        if optional_remote_output(self.run_command("command -v crontab >/dev/null 2>&1"))?.is_some()
        {
            any_present = true;
            if let Some(passwd) =
                optional_remote_output(self.run_command("cat /etc/passwd 2>/dev/null"))?
            {
                let users: Vec<String> = CronAdapter::parse_passwd(&passwd)
                    .into_iter()
                    .filter(|u| shell_safe_username(u))
                    .collect();
                let user_results: Vec<(
                    String,
                    std::result::Result<Option<String>, RemoteSourceError>,
                )> = parallel_map(&users, REMOTE_PARALLELISM, |user| {
                    let cmd = format!("crontab -l -u {user} 2>/dev/null");
                    (user.clone(), optional_remote_output(self.run_command(&cmd)))
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

    fn collect_launchd_parallel(
        &self,
    ) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError> {
        if optional_remote_output(self.run_command("command -v launchctl >/dev/null 2>&1"))?
            .is_none()
        {
            return Err(RemoteSourceError::Unavailable);
        }
        let listing = match self.run_command("launchctl list") {
            Ok(s) => s,
            Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
            Err(e) => return Err(remote_source_error("launchctl list", e)),
        };
        let runtime: std::collections::HashMap<String, LaunchctlEntry> =
            LaunchdAdapter::parse_launchctl_list(&listing)
                .into_iter()
                .map(|e| (e.label.clone(), e))
                .collect();

        let find_cmd = "find $HOME/Library/LaunchAgents /Library/LaunchAgents \
                        /Library/LaunchDaemons /System/Library/LaunchAgents \
                        /System/Library/LaunchDaemons -maxdepth 1 -name '*.plist' 2>/dev/null";
        let plist_paths_str =
            optional_remote_output(self.run_command(find_cmd))?.unwrap_or_default();
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
            (path.clone(), optional_remote_output(self.run_command(&cmd)))
        });

        let mut tasks = Vec::new();
        for (path, content_opt) in plist_results {
            let Some(content) = content_opt? else {
                continue;
            };
            match LaunchdAdapter::parse_plist(content.as_bytes()) {
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

    /// Best-effort teardown of the multiplex master.
    fn close(&self) {
        let mut args = self.ssh_options();
        args.push("-O".into());
        args.push("exit".into());
        args.push(self.host.clone());
        let _ = Command::new("ssh").args(args).output();
    }
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

// Closure-based per-source collectors. These take a generic `FnMut`
// runner so tests can inject a fixture-driven router. Production paths
// use the parallel methods on `RemoteCollector` instead, so these are
// only live under `cfg(test)`.

#[allow(dead_code)]
fn collect_systemd_via<F>(run: &mut F) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError>
where
    F: FnMut(&str) -> std::result::Result<String, RemoteCmdError>,
{
    match run("command -v systemctl >/dev/null 2>&1 && echo present") {
        Ok(s) if s.trim() == "present" => {}
        Ok(_) => return Err(RemoteSourceError::Unavailable),
        Err(RemoteCmdError::NotFound) | Err(RemoteCmdError::Failed { .. }) => {
            return Err(RemoteSourceError::Unavailable);
        }
        Err(RemoteCmdError::Ssh(e)) => return Err(RemoteSourceError::Transport(e)),
    }

    let json = match run("systemctl list-timers --all --output=json --no-pager") {
        Ok(s) => s,
        Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
        Err(e) => return Err(remote_source_error("systemctl list-timers", e)),
    };
    let mut tasks = SystemdAdapter::parse_list_timers(&json)
        .map_err(|e| RemoteSourceError::Other(anyhow!("parse list-timers: {e}")))?;

    // show errors are non-fatal — keep the task with what list-timers gave us.
    for task in &mut tasks {
        let timer_cmd = format!(
            "systemctl show {} --property=TimersCalendar,TimersMonotonic,Result --no-pager",
            shell_quote(&task.id)
        );
        if let Ok(text) = run(&timer_cmd) {
            let timer = SystemdAdapter::parse_show_timer(&text);
            if let Some(expr) = timer.on_calendar {
                task.schedule = ScheduleType::Calendar(expr);
            } else if let Some(d) = timer.on_interval {
                task.schedule = ScheduleType::Interval(d);
            }
            task.last_status = timer.result.map(map_systemd_result);
        }

        let service = task.command.clone();
        if !service.is_empty() {
            let svc_cmd = format!(
                "systemctl show {} --property=ExecStart --no-pager",
                shell_quote(&service)
            );
            if let Ok(text) = run(&svc_cmd) {
                let svc = SystemdAdapter::parse_show_service(&text);
                if let Some(c) = svc.exec_start {
                    task.command = c;
                }
            }
        }
    }

    Ok(tasks)
}

#[allow(dead_code)]
fn collect_cron_via<F>(run: &mut F) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError>
where
    F: FnMut(&str) -> std::result::Result<String, RemoteCmdError>,
{
    let mut tasks = Vec::new();
    let mut any_present = false;

    if let Ok(text) = run("cat /etc/crontab 2>/dev/null") {
        any_present = true;
        if !text.is_empty() {
            tasks.extend(CronAdapter::parse_crontab(&text, "/etc/crontab", true));
        }
    }

    if let Ok(listing) = run("ls -1 /etc/cron.d 2>/dev/null") {
        any_present = true;
        for filename in listing.lines().map(str::trim).filter(|s| !s.is_empty()) {
            let path = format!("/etc/cron.d/{filename}");
            let cmd = format!("cat {} 2>/dev/null", shell_quote(&path));
            if let Ok(text) = run(&cmd) {
                tasks.extend(CronAdapter::parse_crontab(&text, &path, true));
            }
        }
    }

    for (period, dir) in [
        ("hourly", "/etc/cron.hourly"),
        ("daily", "/etc/cron.daily"),
        ("weekly", "/etc/cron.weekly"),
        ("monthly", "/etc/cron.monthly"),
    ] {
        let cmd = format!("ls -1 {} 2>/dev/null", shell_quote(dir));
        if let Ok(listing) = run(&cmd) {
            any_present = true;
            let scripts: Vec<&str> = listing
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !scripts.is_empty() {
                tasks.extend(CronAdapter::parse_run_parts(period, &scripts, dir));
            }
        }
    }

    if run("command -v crontab >/dev/null 2>&1").is_ok() {
        any_present = true;
        if let Ok(passwd) = run("cat /etc/passwd 2>/dev/null") {
            for user in CronAdapter::parse_passwd(&passwd) {
                if !shell_safe_username(&user) {
                    continue;
                }
                let cmd = format!("crontab -l -u {user} 2>/dev/null");
                if let Ok(text) = run(&cmd) {
                    if !text.is_empty() {
                        tasks.extend(CronAdapter::parse_crontab(
                            &text,
                            &format!("user:{user}"),
                            false,
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

fn collect_at_via<F, Tz: TimeZone>(
    run: &mut F,
    timezone: &Tz,
) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError>
where
    F: FnMut(&str) -> std::result::Result<String, RemoteCmdError>,
{
    if optional_remote_output(run("command -v atq >/dev/null 2>&1"))?.is_none() {
        return Err(RemoteSourceError::Unavailable);
    }
    let listing = match run("atq") {
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
        if let Some(text) = optional_remote_output(run(&cmd))? {
            if let Some(c) = AtAdapter::parse_at_c(&text) {
                task.command = c;
            }
        }
    }
    Ok(tasks)
}

fn collect_anacron_via<F>(run: &mut F) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError>
where
    F: FnMut(&str) -> std::result::Result<String, RemoteCmdError>,
{
    let text = match optional_remote_output(run("cat /etc/anacrontab 2>/dev/null"))? {
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
        if let Some(spool) = optional_remote_output(run(&cmd))? {
            if let Some(date) = AnacronAdapter::parse_spool_file(&spool) {
                if let Some(last) = Utc
                    .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
                    .single()
                {
                    task.last_run = Some(last);
                    if let Some(d) = anacron_period_advance(&task.schedule) {
                        task.next_run = Some(last + d);
                    }
                }
            }
        }
    }

    Ok(tasks)
}

#[allow(dead_code)]
fn collect_launchd_via<F>(run: &mut F) -> std::result::Result<Vec<ScheduledTask>, RemoteSourceError>
where
    F: FnMut(&str) -> std::result::Result<String, RemoteCmdError>,
{
    if run("command -v launchctl >/dev/null 2>&1").is_err() {
        return Err(RemoteSourceError::Unavailable);
    }
    let listing = match run("launchctl list") {
        Ok(s) => s,
        Err(RemoteCmdError::NotFound) => return Err(RemoteSourceError::Unavailable),
        Err(e) => return Err(remote_source_error("launchctl list", e)),
    };
    let runtime: std::collections::HashMap<String, LaunchctlEntry> =
        LaunchdAdapter::parse_launchctl_list(&listing)
            .into_iter()
            .map(|e| (e.label.clone(), e))
            .collect();

    // `$HOME` is expanded by the remote shell.
    let find_cmd = "find $HOME/Library/LaunchAgents /Library/LaunchAgents \
                    /Library/LaunchDaemons /System/Library/LaunchAgents \
                    /System/Library/LaunchDaemons -maxdepth 1 -name '*.plist' 2>/dev/null";
    let plist_paths = run(find_cmd).unwrap_or_default();

    let mut tasks = Vec::new();
    for path in plist_paths.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let cmd = format!("cat {} 2>/dev/null", shell_quote(path));
        let Ok(content) = run(&cmd) else { continue };
        match LaunchdAdapter::parse_plist(content.as_bytes()) {
            Ok(Some(mut task)) => {
                if let Some(rt) = runtime.get(&task.id) {
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
                tasks.push(task);
            }
            Ok(None) => {}
            Err(e) => eprintln!("warning: parse {path}: {e}"),
        }
    }

    Ok(tasks)
}

fn map_systemd_result(s: String) -> TaskStatus {
    match s.as_str() {
        "success" => TaskStatus::Success,
        other => TaskStatus::Failed(other.to_string()),
    }
}

// Mirror of the private helper in `shuvjobs-adapters::anacron`.
fn anacron_period_advance(schedule: &ScheduleType) -> Option<chrono::Duration> {
    match schedule {
        ScheduleType::Interval(d) => chrono::Duration::from_std(*d).ok(),
        ScheduleType::Calendar(s) if s == "@monthly" => Some(chrono::Duration::days(30)),
        _ => None,
    }
}

/// POSIX-safe single-quoting: wraps `s` in `'...'` and escapes any
/// embedded `'` by closing the literal, inserting `\'`, and reopening.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Whitelist for usernames before interpolating them into a shell command.
fn shell_safe_username(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn shell_safe_filename(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

// Keeps `chrono::Duration` referenced from this module even if the
// dead-code helper above gets eliminated in some build profile.
const _: Option<Duration> = None;

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

/// Merge `systemctl show <timer>` and `systemctl show <service>`
/// outputs onto an already-parsed task.
fn apply_systemd_show(
    task: &mut ScheduledTask,
    timer_text: Option<&str>,
    service_text: Option<&str>,
) {
    if let Some(text) = timer_text {
        let timer = SystemdAdapter::parse_show_timer(text);
        if let Some(expr) = timer.on_calendar {
            task.schedule = ScheduleType::Calendar(expr);
        } else if let Some(d) = timer.on_interval {
            task.schedule = ScheduleType::Interval(d);
        }
        task.last_status = timer.result.map(map_systemd_result);
    }
    if let Some(text) = service_text {
        let svc = SystemdAdapter::parse_show_service(text);
        if let Some(c) = svc.exec_start {
            task.command = c;
        }
    }
}

/// Merge `launchctl list` runtime state onto a plist-parsed task.
fn apply_launchctl_runtime(task: &mut ScheduledTask, entry: Option<&LaunchctlEntry>) {
    let Some(rt) = entry else { return };
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
    use std::collections::HashMap;

    fn collector() -> RemoteCollector {
        RemoteCollector {
            host: "alice@host".into(),
            port: None,
            key_path: None,
            control_path: PathBuf::from("/tmp/shuvjobs-ssh-1"),
        }
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
        let coll = RemoteCollector {
            host: "alice@host".into(),
            port: Some(2222),
            key_path: Some(PathBuf::from("/k")),
            control_path: PathBuf::from("/tmp/shuvjobs-ssh-1"),
        };
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
    fn at_via_propagates_ssh_transport_errors() {
        let mut run = |_cmd: &str| Err(RemoteCmdError::Ssh(anyhow!("connection lost")));
        let result = collect_at_via(&mut run, &Utc);
        assert!(matches!(result, Err(RemoteSourceError::Transport(_))));
    }

    /// Build a runner that looks up commands in a fixed map. Unknown
    /// commands fall through to `Failed{1}`, which the collectors treat
    /// as "this slice of the source isn't present".
    fn router(
        fixtures: HashMap<&'static str, &'static str>,
    ) -> impl FnMut(&str) -> std::result::Result<String, RemoteCmdError> {
        move |cmd: &str| match fixtures.get(cmd) {
            Some(s) => Ok((*s).to_string()),
            None => Err(RemoteCmdError::Failed {
                exit_code: 1,
                stderr: String::new(),
            }),
        }
    }

    const SYSTEMD_LIST: &str = r#"[
        {"next":1775919186922121,"left":0,"last":1775499594111976,"passed":0,"unit":"logrotate.timer","activates":"logrotate.service"}
    ]"#;
    const SYSTEMD_SHOW_TIMER: &str =
        "TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Sat 2026-04-11 17:04:57 +03 }\n\
         Result=success\n";
    const SYSTEMD_SHOW_SERVICE: &str =
        "ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";

    #[test]
    fn systemd_via_runs_full_pipeline() {
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
            "systemctl show 'logrotate.timer' --property=TimersCalendar,TimersMonotonic,Result --no-pager",
            SYSTEMD_SHOW_TIMER,
        );
        fx.insert(
            "systemctl show 'logrotate.service' --property=ExecStart --no-pager",
            SYSTEMD_SHOW_SERVICE,
        );
        let mut run = router(fx);

        let tasks = collect_systemd_via(&mut run).unwrap();
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert_eq!(t.id, "logrotate.timer");
        assert_eq!(t.command, "/usr/sbin/logrotate /etc/logrotate.conf");
        assert!(matches!(t.schedule, ScheduleType::Calendar(ref s) if s == "*-*-* 00:00:00"));
        assert!(matches!(t.last_status, Some(TaskStatus::Success)));
    }

    #[test]
    fn systemd_via_unavailable_when_systemctl_missing() {
        let mut run = |_cmd: &str| -> std::result::Result<String, RemoteCmdError> {
            Err(RemoteCmdError::NotFound)
        };
        let err = collect_systemd_via(&mut run).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    const ETC_CRONTAB: &str = "\
# m h dom mon dow user  command
17 *    * * *   root    cd / && run-parts --report /etc/cron.hourly
@reboot root    /usr/local/bin/warmup-cache
";

    #[test]
    fn cron_via_parses_etc_crontab_through_router() {
        let mut fx = HashMap::new();
        fx.insert("cat /etc/crontab 2>/dev/null", ETC_CRONTAB);
        let mut run = router(fx);
        let tasks = collect_cron_via(&mut run).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(matches!(tasks[0].schedule, ScheduleType::Cron(ref s) if s == "17 * * * *"));
        assert!(matches!(tasks[1].schedule, ScheduleType::Cron(ref s) if s == "@reboot"));
    }

    #[test]
    fn cron_via_unavailable_when_nothing_found() {
        let mut run = |_cmd: &str| -> std::result::Result<String, RemoteCmdError> {
            Err(RemoteCmdError::Failed {
                exit_code: 1,
                stderr: String::new(),
            })
        };
        let err = collect_cron_via(&mut run).unwrap_err();
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
    fn at_via_parses_atq_and_at_c() {
        let mut fx = HashMap::new();
        fx.insert("command -v atq >/dev/null 2>&1", "");
        fx.insert("atq", ATQ_OUTPUT);
        fx.insert("at -c 12 2>/dev/null", AT_C_OUTPUT);
        let mut run = router(fx);
        let tasks = collect_at_via(&mut run, &Utc).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "at:12");
        assert_eq!(tasks[0].command, "echo hello");
    }

    #[test]
    fn at_via_unavailable_when_atq_missing() {
        let mut run = |_cmd: &str| -> std::result::Result<String, RemoteCmdError> {
            Err(RemoteCmdError::NotFound)
        };
        let err = collect_at_via(&mut run, &Utc).unwrap_err();
        assert!(matches!(err, RemoteSourceError::Unavailable));
    }

    // -- anacron --

    const ANACRONTAB: &str = "\
1 5 cron.daily run-parts /etc/cron.daily
@monthly 45 cron.monthly run-parts /etc/cron.monthly
";

    #[test]
    fn anacron_via_parses_anacrontab_and_spool() {
        let mut fx = HashMap::new();
        fx.insert("cat /etc/anacrontab 2>/dev/null", ANACRONTAB);
        fx.insert(
            "cat /var/spool/anacron/cron.daily 2>/dev/null",
            "20260410\n",
        );
        let mut run = router(fx);
        let tasks = collect_anacron_via(&mut run).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "cron.daily");
        assert!(tasks[0].last_run.is_some());
        assert!(tasks[0].next_run > tasks[0].last_run);
        // monthly entry has no spool fixture
        assert!(tasks[1].last_run.is_none());
    }

    #[test]
    fn anacron_via_unavailable_when_anacrontab_missing() {
        let mut run = |_cmd: &str| -> std::result::Result<String, RemoteCmdError> {
            Err(RemoteCmdError::Failed {
                exit_code: 1,
                stderr: String::new(),
            })
        };
        let err = collect_anacron_via(&mut run).unwrap_err();
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
    fn launchd_via_parses_listing_and_plists() {
        let mut fx = HashMap::new();
        fx.insert("command -v launchctl >/dev/null 2>&1", "");
        fx.insert("launchctl list", LAUNCHCTL_LIST);
        let find_cmd = "find $HOME/Library/LaunchAgents /Library/LaunchAgents \
                        /Library/LaunchDaemons /System/Library/LaunchAgents \
                        /System/Library/LaunchDaemons -maxdepth 1 -name '*.plist' 2>/dev/null";
        fx.insert(
            find_cmd,
            "/Library/LaunchAgents/com.example.heartbeat.plist\n",
        );
        fx.insert(
            "cat '/Library/LaunchAgents/com.example.heartbeat.plist' 2>/dev/null",
            PLIST_BODY,
        );
        let mut run = router(fx);
        let tasks = collect_launchd_via(&mut run).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "com.example.heartbeat");
        // launchctl reported exit 0 → Success
        assert!(matches!(tasks[0].last_status, Some(TaskStatus::Success)));
        assert!(matches!(
            tasks[0].schedule,
            ScheduleType::Interval(d) if d == std::time::Duration::from_secs(900)
        ));
    }

    #[test]
    fn launchd_via_unavailable_when_launchctl_missing() {
        let mut run = |_cmd: &str| -> std::result::Result<String, RemoteCmdError> {
            Err(RemoteCmdError::NotFound)
        };
        let err = collect_launchd_via(&mut run).unwrap_err();
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

    fn empty_systemd_task() -> ScheduledTask {
        ScheduledTask {
            id: "logrotate.timer".into(),
            name: "logrotate".into(),
            source: shuvjobs_core::TaskSourceKind::Systemd,
            schedule: ScheduleType::Calendar(String::new()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "logrotate.service".into(),
        }
    }

    #[test]
    fn apply_systemd_show_merges_calendar_status_and_command() {
        let mut task = empty_systemd_task();
        apply_systemd_show(
            &mut task,
            Some(SYSTEMD_SHOW_TIMER),
            Some(SYSTEMD_SHOW_SERVICE),
        );
        assert!(matches!(task.schedule, ScheduleType::Calendar(ref s) if s == "*-*-* 00:00:00"));
        assert!(matches!(task.last_status, Some(TaskStatus::Success)));
        assert_eq!(task.command, "/usr/sbin/logrotate /etc/logrotate.conf");
    }

    #[test]
    fn apply_systemd_show_no_op_when_both_inputs_none() {
        let mut task = empty_systemd_task();
        let snapshot = task.clone();
        apply_systemd_show(&mut task, None, None);
        assert_eq!(task, snapshot);
    }

    #[test]
    fn apply_systemd_show_skips_service_merge_when_service_is_empty() {
        let mut task = empty_systemd_task();
        task.command = String::new();
        apply_systemd_show(&mut task, Some(SYSTEMD_SHOW_TIMER), None);
        assert_eq!(task.command, "");
        assert!(matches!(task.schedule, ScheduleType::Calendar(_)));
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
