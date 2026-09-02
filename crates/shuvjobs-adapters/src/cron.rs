//! cron adapter.
//!
//! Covers `/etc/crontab`, `/etc/cron.d/*`, the four run-parts dirs, and
//! per-user crontabs via `crontab -l -u <user>`. `last_run` and
//! `last_status` are left `None` — recovering them would require
//! scraping syslog/journalctl, which is permissioned and unreliable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Local, TimeZone, Utc};
use shuvjobs_core::{Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind};

#[derive(Debug, Default)]
pub struct CronAdapter;

impl CronAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Parse a crontab-style file. `has_user_field` is true for
    /// `/etc/crontab` and `/etc/cron.d/*` (column layout includes user).
    /// Per-user crontabs from `crontab -l` omit the user column.
    pub fn parse_crontab(contents: &str, origin: &str, has_user_field: bool) -> Vec<ScheduledTask> {
        Self::parse_crontab_at(contents, origin, has_user_field, Local::now())
    }

    pub fn parse_crontab_at<Tz: TimeZone>(
        contents: &str,
        origin: &str,
        has_user_field: bool,
        now: DateTime<Tz>,
    ) -> Vec<ScheduledTask> {
        let mut out = Vec::new();
        for (idx, raw) in contents.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if is_env_assignment(line) {
                continue;
            }

            let parsed = if line.starts_with('@') {
                parse_shortcut_line(line, has_user_field)
            } else {
                parse_five_field_line(line, has_user_field)
            };

            let Some((schedule_expr, command)) = parsed else {
                eprintln!("warning: skipping malformed cron line {origin}:{}", idx + 1);
                continue;
            };
            if command.is_empty() {
                continue;
            }

            let schedule = ScheduleType::Cron(schedule_expr);
            let next_run = compute_next_run(&schedule, now.clone());
            out.push(ScheduledTask {
                id: format!("{origin}:{}", idx + 1),
                name: command_basename(&command),
                source: TaskSourceKind::Cron,
                schedule,
                last_run: None,
                last_status: None,
                last_duration: None,
                next_run,
                command,
            });
        }
        out
    }

    /// `period` is one of `hourly`/`daily`/`weekly`/`monthly`.
    pub fn parse_run_parts(period: &str, scripts: &[&str], dir: &str) -> Vec<ScheduledTask> {
        Self::parse_run_parts_at(period, scripts, dir, Local::now())
    }

    pub fn parse_run_parts_at<Tz: TimeZone>(
        period: &str,
        scripts: &[&str],
        dir: &str,
        now: DateTime<Tz>,
    ) -> Vec<ScheduledTask> {
        let schedule = ScheduleType::Calendar(format!("@{period}"));
        let next_run = compute_next_run(&schedule, now);
        scripts
            .iter()
            .map(|script| ScheduledTask {
                id: format!("{dir}/{script}"),
                name: (*script).to_string(),
                source: TaskSourceKind::Cron,
                schedule: schedule.clone(),
                last_run: None,
                last_status: None,
                last_duration: None,
                next_run,
                command: format!("{dir}/{script}"),
            })
            .collect()
    }

    /// Usernames from `/etc/passwd` with login shells.
    pub fn parse_passwd(contents: &str) -> Vec<String> {
        contents
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(':');
                let user = parts.next()?;
                let _passwd = parts.next()?;
                let _uid = parts.next()?;
                let _gid = parts.next()?;
                let _gecos = parts.next()?;
                let _home = parts.next()?;
                let shell = parts.next()?.trim();
                if is_login_shell(shell) {
                    Some(user.to_string())
                } else {
                    None
                }
            })
            .collect()
    }
}

impl TaskSource for CronAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Cron
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        let etc_crontab = Path::new("/etc/crontab");
        let cron_d = Path::new("/etc/cron.d");
        let run_parts_dirs = [
            ("hourly", "/etc/cron.hourly"),
            ("daily", "/etc/cron.daily"),
            ("weekly", "/etc/cron.weekly"),
            ("monthly", "/etc/cron.monthly"),
        ];
        let crontab_bin_present = which("crontab").is_some();

        let any_run_parts = run_parts_dirs.iter().any(|(_, p)| Path::new(p).exists());
        if !etc_crontab.exists() && !cron_d.exists() && !any_run_parts && !crontab_bin_present {
            return Err(Error::Unavailable(
                "no cron files or `crontab` binary".into(),
            ));
        }

        let mut tasks = Vec::new();

        if etc_crontab.exists() {
            match fs::read_to_string(etc_crontab) {
                Ok(text) => tasks.extend(Self::parse_crontab(&text, "/etc/crontab", true)),
                Err(e) => eprintln!("warning: reading /etc/crontab: {e}"),
            }
        }

        if let Ok(entries) = fs::read_dir(cron_d) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let origin = path.to_string_lossy().into_owned();
                match fs::read_to_string(&path) {
                    Ok(text) => tasks.extend(Self::parse_crontab(&text, &origin, true)),
                    Err(e) => eprintln!("warning: reading {origin}: {e}"),
                }
            }
        }

        for (period, dir) in run_parts_dirs {
            let scripts = list_run_parts_scripts(Path::new(dir));
            if scripts.is_empty() {
                continue;
            }
            let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
            tasks.extend(Self::parse_run_parts(period, &refs, dir));
        }

        if crontab_bin_present {
            let users = fs::read_to_string("/etc/passwd")
                .map(|p| Self::parse_passwd(&p))
                .unwrap_or_default();
            let current = current_username();
            for user in users {
                // `crontab -u` is root-only (cronie and Vixie both refuse
                // it for unprivileged callers, even for yourself), so the
                // invoking user's own crontab must be read with plain
                // `crontab -l`. Other users' crontabs fail silently
                // without privilege; skip them in that case.
                let args = crontab_list_args(&user, current.as_deref());
                let Ok(out) = Command::new("crontab").args(&args).output() else {
                    continue;
                };
                if !out.status.success() {
                    continue;
                }
                let text = String::from_utf8_lossy(&out.stdout);
                tasks.extend(Self::parse_crontab(&text, &format!("user:{user}"), false));
            }
        }

        Ok(tasks)
    }
}

/// Heuristic: a line is an env assignment if its first token starts with
/// an alpha-or-underscore identifier followed by `=`.
fn is_env_assignment(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    if i >= bytes.len() {
        return false;
    }
    let first = bytes[i];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    i += 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'='
}

/// `@reboot`, `@hourly`, ... lines.
fn parse_shortcut_line(line: &str, has_user_field: bool) -> Option<(String, String)> {
    let shortcut = line.split_whitespace().next()?.to_string();
    let fields_before_command = if has_user_field { 2 } else { 1 };
    let bytes = line.as_bytes();
    let mut i = 0;
    for _ in 0..fields_before_command {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    let command = line[i..].trim();
    if command.is_empty() {
        return None;
    }
    Some((shortcut, command.to_string()))
}

fn parse_five_field_line(line: &str, has_user_field: bool) -> Option<(String, String)> {
    // Track byte offsets so we can keep the command's original whitespace intact.
    let mut field_count = 0;
    let needed_fields = if has_user_field { 6 } else { 5 };
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && field_count < needed_fields {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        field_count += 1;
    }
    if field_count < needed_fields {
        return None;
    }
    let command = line[i..].trim();
    if command.is_empty() {
        return None;
    }
    let schedule_tokens: Vec<&str> = line.split_whitespace().take(5).collect();
    if schedule_tokens.len() < 5 {
        return None;
    }
    Some((schedule_tokens.join(" "), command.to_string()))
}

/// Arguments for listing `user`'s crontab: plain `-l` when it is the
/// invoking user, `-l -u <user>` (root-only) otherwise.
pub fn crontab_list_args(user: &str, current: Option<&str>) -> Vec<String> {
    if current == Some(user) {
        vec!["-l".into()]
    } else {
        vec!["-l".into(), "-u".into(), user.into()]
    }
}

fn current_username() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|s| !s.is_empty())
}

fn command_basename(cmd: &str) -> String {
    let first = cmd.split_whitespace().next().unwrap_or(cmd);
    first.rsplit('/').next().unwrap_or(first).to_string()
}

fn is_login_shell(shell: &str) -> bool {
    !matches!(
        shell,
        "" | "/usr/sbin/nologin"
            | "/sbin/nologin"
            | "/usr/bin/nologin"
            | "/bin/false"
            | "/usr/bin/false"
    )
}

fn list_run_parts_scripts(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // run-parts itself skips dot-containing names but we surface
            // them anyway — orphaned scripts are exactly the kind of
            // thing the audit is for.
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Next run after `now` for cron and `@hourly`/`@daily`/etc aliases.
/// Returns `None` for `@reboot`, non-cron schedules, and unparseable
/// expressions (better to show the row with no next-run than drop it).
pub fn compute_next_run<Tz: TimeZone>(
    schedule: &ScheduleType,
    now: DateTime<Tz>,
) -> Option<DateTime<Utc>> {
    let expr = match schedule {
        ScheduleType::Cron(s) => s.as_str(),
        // `@hourly` etc come from parse_run_parts. Bare systemd
        // OnCalendar expressions skip this match arm and return None.
        ScheduleType::Calendar(s) if s.starts_with('@') => s.as_str(),
        _ => return None,
    };
    if expr.eq_ignore_ascii_case("@reboot") {
        return None;
    }
    let cron = croner::Cron::new(expr).parse().ok()?;
    cron.find_next_occurrence(&now, false)
        .ok()
        .map(|next| next.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETC_CRONTAB: &str = "\
# /etc/crontab: system-wide crontab
SHELL=/bin/sh
PATH=/usr/local/sbin:/usr/local/bin:/sbin:/bin:/usr/sbin:/usr/bin

# m h dom mon dow user  command
17 *    * * *   root    cd / && run-parts --report /etc/cron.hourly
25 6    * * *   root    test -x /usr/sbin/anacron || ( cd / && run-parts --report /etc/cron.daily )
@reboot root    /usr/local/bin/warmup-cache
";

    #[test]
    fn parses_etc_crontab_format_with_user_field() {
        let tasks = CronAdapter::parse_crontab(ETC_CRONTAB, "/etc/crontab", true);
        assert_eq!(tasks.len(), 3, "expected 3 jobs, got {tasks:?}");

        let hourly = &tasks[0];
        assert_eq!(hourly.source, TaskSourceKind::Cron);
        assert_eq!(hourly.id, "/etc/crontab:6");
        assert!(matches!(hourly.schedule, ScheduleType::Cron(ref s) if s == "17 * * * *"));
        assert!(hourly.command.starts_with("cd / && run-parts"));
        assert_eq!(hourly.name, "cd"); // first token basename

        let daily = &tasks[1];
        assert!(matches!(daily.schedule, ScheduleType::Cron(ref s) if s == "25 6 * * *"));

        let reboot = &tasks[2];
        assert!(matches!(reboot.schedule, ScheduleType::Cron(ref s) if s == "@reboot"));
        assert_eq!(reboot.command, "/usr/local/bin/warmup-cache");
        assert_eq!(reboot.name, "warmup-cache");
    }

    #[test]
    fn parses_aligned_shortcut_without_leaking_user_into_command() {
        let tasks = CronAdapter::parse_crontab(
            "@daily   root   /usr/local/bin/backup --full\n",
            "/etc/crontab",
            true,
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "backup");
        assert_eq!(tasks[0].command, "/usr/local/bin/backup --full");
    }

    const CRON_D_ENTRY: &str = "\
# This file is managed by puppet
*/5 * * * * nobody /usr/local/bin/metrics-flush --batch
";

    #[test]
    fn parses_cron_d_entry() {
        let tasks = CronAdapter::parse_crontab(CRON_D_ENTRY, "/etc/cron.d/metrics", true);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert!(matches!(t.schedule, ScheduleType::Cron(ref s) if s == "*/5 * * * *"));
        assert_eq!(t.command, "/usr/local/bin/metrics-flush --batch");
        assert_eq!(t.name, "metrics-flush");
    }

    const PER_USER_CRONTAB: &str = "\
# alice's crontab
0 9 * * 1-5 /home/alice/bin/standup-reminder
";

    #[test]
    fn parses_per_user_crontab_without_user_field() {
        let tasks = CronAdapter::parse_crontab(PER_USER_CRONTAB, "user:alice", false);
        assert_eq!(tasks.len(), 1);
        let t = &tasks[0];
        assert!(matches!(t.schedule, ScheduleType::Cron(ref s) if s == "0 9 * * 1-5"));
        assert_eq!(t.command, "/home/alice/bin/standup-reminder");
    }

    #[test]
    fn run_parts_constructs_tasks_with_implied_period() {
        let scripts = ["logrotate", "mlocate"];
        let tasks = CronAdapter::parse_run_parts("daily", &scripts, "/etc/cron.daily");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "/etc/cron.daily/logrotate");
        assert!(matches!(tasks[0].schedule, ScheduleType::Calendar(ref s) if s == "@daily"));
        assert_eq!(tasks[0].command, "/etc/cron.daily/logrotate");
        assert_eq!(tasks[0].name, "logrotate");
    }

    #[test]
    fn skips_comments_and_env_lines() {
        let text = "# comment\nFOO=bar\nBAZ=qux value\n* * * * * root /bin/true\n";
        let tasks = CronAdapter::parse_crontab(text, "/tmp/x", true);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].command, "/bin/true");
    }

    #[test]
    fn skips_malformed_line_does_not_panic() {
        let text = "not enough fields here\n* * * * * root /bin/echo ok\n";
        let tasks = CronAdapter::parse_crontab(text, "/tmp/x", true);
        // Malformed first line is dropped; valid second line survives.
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].command, "/bin/echo ok");
    }

    const PASSWD_FIXTURE: &str = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
alice:x:1000:1000:Alice:/home/alice:/bin/zsh
nobody:x:65534:65534:nobody:/nonexistent:/bin/false
";

    #[test]
    fn parse_passwd_keeps_only_login_shells() {
        let users = CronAdapter::parse_passwd(PASSWD_FIXTURE);
        assert_eq!(users, vec!["root", "alice"]);
    }

    #[test]
    fn crontab_list_args_uses_plain_list_for_current_user() {
        assert_eq!(crontab_list_args("alice", Some("alice")), vec!["-l"]);
        assert_eq!(
            crontab_list_args("bob", Some("alice")),
            vec!["-l", "-u", "bob"]
        );
        assert_eq!(crontab_list_args("bob", None), vec!["-l", "-u", "bob"]);
    }

    #[test]
    fn adapter_reports_cron_kind() {
        assert_eq!(CronAdapter::new().kind(), TaskSourceKind::Cron);
    }

    // ---- next_run computation ----

    #[test]
    fn compute_next_run_two_am_daily_is_in_the_future() {
        let now = Utc::now();
        let next = compute_next_run(&ScheduleType::Cron("0 2 * * *".into()), now).unwrap();
        assert!(next > now);
        // Sanity ceiling: at most 24 hours away (the period of the rule).
        assert!(next - now <= chrono::Duration::hours(24));
    }

    #[test]
    fn compute_next_run_uses_the_supplied_local_timezone() {
        let timezone = chrono::FixedOffset::east_opt(3 * 60 * 60).unwrap();
        let now = timezone.with_ymd_and_hms(2026, 4, 14, 1, 30, 0).unwrap();
        let next = compute_next_run(&ScheduleType::Cron("0 2 * * *".into()), now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 4, 13, 23, 0, 0).unwrap());
    }

    #[test]
    fn parse_crontab_uses_the_supplied_local_timezone() {
        let timezone = chrono::FixedOffset::west_opt(7 * 60 * 60).unwrap();
        let now = timezone.with_ymd_and_hms(2026, 4, 14, 1, 30, 0).unwrap();
        let tasks =
            CronAdapter::parse_crontab_at("0 2 * * * root /bin/true\n", "/etc/crontab", true, now);
        assert_eq!(
            tasks[0].next_run,
            Some(Utc.with_ymd_and_hms(2026, 4, 14, 9, 0, 0).unwrap())
        );
    }

    #[test]
    fn compute_next_run_every_five_minutes_is_within_five_minutes() {
        let now = Utc::now();
        let next = compute_next_run(&ScheduleType::Cron("*/5 * * * *".into()), now).unwrap();
        let delta = next - now;
        assert!(delta > chrono::Duration::zero());
        assert!(
            delta <= chrono::Duration::minutes(5),
            "delta was {delta} for next={next} now={now}"
        );
    }

    #[test]
    fn compute_next_run_daily_alias_is_within_24_hours() {
        let now = Utc::now();
        let next = compute_next_run(&ScheduleType::Calendar("@daily".into()), now).unwrap();
        let delta = next - now;
        assert!(delta > chrono::Duration::zero());
        assert!(delta <= chrono::Duration::hours(24));
    }

    #[test]
    fn compute_next_run_reboot_is_none() {
        assert!(compute_next_run(&ScheduleType::Cron("@reboot".into()), Utc::now()).is_none());
    }

    #[test]
    fn compute_next_run_invalid_expression_is_silent_none() {
        let s = ScheduleType::Cron("not a cron expression".into());
        assert!(compute_next_run(&s, Utc::now()).is_none());
    }

    #[test]
    fn compute_next_run_systemd_calendar_is_none() {
        // `Calendar(...)` without a leading `@` is systemd OnCalendar,
        // not cron syntax — croner would reject it anyway.
        let s = ScheduleType::Calendar("*-*-* 00:00:00".into());
        assert!(compute_next_run(&s, Utc::now()).is_none());
    }

    #[test]
    fn compute_next_run_with_range_step_expression() {
        let now = Utc::now();
        let next = compute_next_run(&ScheduleType::Cron("5-55/10 * * * *".into()), now).unwrap();
        let delta = next - now;
        assert!(delta > chrono::Duration::zero());
        assert!(delta <= chrono::Duration::minutes(60));
    }

    #[test]
    fn parse_crontab_populates_next_run_for_valid_expression() {
        let text = "*/5 * * * * root /bin/true\n";
        let tasks = CronAdapter::parse_crontab(text, "/etc/crontab", true);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].next_run.is_some());
    }

    #[test]
    fn parse_crontab_leaves_reboot_next_run_none() {
        let text = "@reboot root /usr/local/bin/warmup\n";
        let tasks = CronAdapter::parse_crontab(text, "/etc/crontab", true);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].next_run.is_none());
    }

    #[test]
    fn parse_run_parts_populates_next_run_for_period_alias() {
        let scripts = ["logrotate"];
        let tasks = CronAdapter::parse_run_parts("hourly", &scripts, "/etc/cron.hourly");
        assert!(tasks[0].next_run.is_some());
    }
}
