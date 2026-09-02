//! systemd `*.timer` adapter.
//!
//! `systemctl list-timers` gives us the next/last instants and the bound
//! service. We then `systemctl show` each timer for `OnCalendar=` /
//! `OnUnitActiveSec=` and the bound service for `ExecStart=`, `Result=`,
//! `ActiveState=`/`SubState=`, and the main-process timestamps.
//!
//! Status deliberately comes from the *service*, not the timer: a timer
//! unit's own `Result=` stays `success` even when every activation of
//! the service it fires has failed.

use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use shuvjobs_core::{
    Error, Result, ScheduleType, ScheduledTask, TaskSource, TaskSourceKind, TaskStatus,
};

/// Which systemd manager a timer belongs to. `systemctl` talks to the
/// system manager by default and to the calling user's manager with
/// `--user`; the two namespaces can hold identically named units, so the
/// scope is part of a task's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

/// Prefix that marks a user-scope task id. System-scope ids stay bare so
/// the common case reads as the plain unit name.
const USER_ID_PREFIX: &str = "user/";

impl Scope {
    /// Extra argv passed to every `systemctl` invocation for this scope.
    pub fn systemctl_args(self) -> &'static [&'static str] {
        match self {
            Self::System => &[],
            Self::User => &["--user"],
        }
    }

    /// Stable task id for `unit` in this scope.
    pub fn task_id(self, unit: &str) -> String {
        match self {
            Self::System => unit.to_string(),
            Self::User => format!("{USER_ID_PREFIX}{unit}"),
        }
    }

    /// Display name for `unit` in this scope.
    pub fn task_name(self, unit: &str) -> String {
        let trimmed = trim_unit_suffix(unit);
        match self {
            Self::System => trimmed.to_string(),
            Self::User => format!("{trimmed} (user)"),
        }
    }
}

/// Inverse of [`Scope::task_id`]: map a task id back to the scope and the
/// unit name to pass to `systemctl show`.
pub fn split_task_id(id: &str) -> (Scope, &str) {
    match id.strip_prefix(USER_ID_PREFIX) {
        Some(unit) => (Scope::User, unit),
        None => (Scope::System, id),
    }
}

#[derive(Debug, Default)]
pub struct SystemdAdapter;

impl SystemdAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Parse `systemctl list-timers --output=json` from the system manager.
    pub fn parse_list_timers(json: &str) -> Result<Vec<ScheduledTask>> {
        Self::parse_list_timers_scoped(json, Scope::System)
    }

    /// Parse `systemctl [--user] list-timers --output=json`. `scope`
    /// decides the id and display name so system and user units with the
    /// same name stay distinguishable.
    pub fn parse_list_timers_scoped(json: &str, scope: Scope) -> Result<Vec<ScheduledTask>> {
        // `next`/`last` come back as either microsecond integers or
        // `null` (Ubuntu's `apport-autoreport.timer` is the canonical
        // null case). `left`/`passed` are unused but still optional.
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            next: Option<i64>,
            #[serde(default)]
            #[allow(dead_code)]
            left: Option<i64>,
            #[serde(default)]
            last: Option<i64>,
            #[serde(default)]
            #[allow(dead_code)]
            passed: Option<i64>,
            unit: String,
            #[serde(default)]
            activates: String,
        }

        let rows: Vec<Row> = serde_json::from_str(json).map_err(|e| Error::Parse {
            kind: "systemctl list-timers".into(),
            message: e.to_string(),
        })?;

        let tasks = rows
            .into_iter()
            .map(|r| ScheduledTask {
                id: scope.task_id(&r.unit),
                name: scope.task_name(&r.unit),
                source: TaskSourceKind::Systemd,
                // Filled in later by parse_show_timer.
                schedule: ScheduleType::Calendar(String::new()),
                last_run: micros_to_dt(r.last.unwrap_or(0)),
                last_status: None,
                last_duration: None,
                next_run: micros_to_dt(r.next.unwrap_or(0)),
                command: r.activates,
            })
            .collect();
        Ok(tasks)
    }

    /// Parse `systemctl show <timer> --property=TimersCalendar,TimersMonotonic,Result`.
    pub fn parse_show_timer(text: &str) -> ShowTimer {
        let mut out = ShowTimer::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                // value: { OnCalendar=*-*-* 00:00:00 ; next_elapse=... }
                "TimersCalendar" => {
                    if let Some(expr) = extract_property_subfield(value, "OnCalendar=") {
                        if out.on_calendar.is_none() {
                            out.on_calendar = Some(expr);
                        }
                    }
                }
                // value: { OnBootSec=15min ; next_elapse=... } — any On*Sec key.
                "TimersMonotonic" => {
                    if let Some(raw) = extract_monotonic_value(value) {
                        if out.on_interval.is_none() {
                            out.on_interval = parse_systemd_duration(&raw);
                        }
                    }
                }
                "Result" if !value.is_empty() => {
                    out.result = Some(value.to_string());
                }
                _ => {}
            }
        }
        out
    }

    /// Parse `systemctl show <service> --property=<SERVICE_SHOW_PROPERTIES>`.
    pub fn parse_show_service(text: &str) -> ShowService {
        let mut out = ShowService::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                // value: { path=...; argv[]=... ; ignore_errors=no ; ... }
                "ExecStart" => {
                    if let Some(argv) = extract_property_subfield(value, "argv[]=") {
                        if out.exec_start.is_none() {
                            out.exec_start = Some(argv);
                        }
                    }
                }
                "Result" if !value.is_empty() => out.result = Some(value.to_string()),
                "ActiveState" if !value.is_empty() => out.active_state = Some(value.to_string()),
                "SubState" if !value.is_empty() => out.sub_state = Some(value.to_string()),
                "ExecMainStartTimestampMonotonic" => {
                    out.main_start_us = value.trim().parse().ok().filter(|&us: &u64| us > 0);
                }
                "ExecMainExitTimestampMonotonic" => {
                    out.main_exit_us = value.trim().parse().ok().filter(|&us: &u64| us > 0);
                }
                _ => {}
            }
        }
        out
    }

    /// Merge `systemctl show <timer>` and `systemctl show <service>` output
    /// onto a task produced by [`SystemdAdapter::parse_list_timers`].
    /// Either input may be absent; the task keeps whatever it already had.
    pub fn apply_show(
        task: &mut ScheduledTask,
        timer_text: Option<&str>,
        service_text: Option<&str>,
    ) {
        let timer = timer_text.map(Self::parse_show_timer);
        let service = service_text.map(Self::parse_show_service);

        if let Some(timer) = &timer {
            if let Some(expr) = &timer.on_calendar {
                task.schedule = ScheduleType::Calendar(expr.clone());
            } else if let Some(d) = timer.on_interval {
                task.schedule = ScheduleType::Interval(d);
            }
        }

        if let Some(service) = &service {
            if let Some(cmd) = &service.exec_start {
                task.command = cmd.clone();
            }
            task.last_duration = service.main_duration();
        }

        task.last_status = derive_status(timer.as_ref(), service.as_ref(), task.last_run.is_some());
    }
}

/// Properties requested from the bound service. Kept in one place so the
/// local adapter and the SSH bridge issue the identical `systemctl show`.
pub const SERVICE_SHOW_PROPERTIES: &str = "ExecStart,Result,ActiveState,SubState,\
ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic";

/// Properties requested from the timer unit itself.
pub const TIMER_SHOW_PROPERTIES: &str = "TimersCalendar,TimersMonotonic,Result";

/// Service state wins; the timer's own `Result=` is only a fallback when
/// we could not inspect the service at all. A service that has never
/// been triggered reports `Result=success` too, so with no last run and
/// a clean result we report nothing rather than a misleading check mark.
fn derive_status(
    timer: Option<&ShowTimer>,
    service: Option<&ShowService>,
    has_last_run: bool,
) -> Option<TaskStatus> {
    if let Some(service) = service {
        if service.is_running() {
            return Some(TaskStatus::Running);
        }
        if let Some(result) = &service.result {
            if result == "success" && !has_last_run && service.main_exit_us.is_none() {
                return None;
            }
            return Some(map_result(result.clone()));
        }
    }
    timer
        .and_then(|t| t.result.clone())
        .filter(|_| has_last_run)
        .map(map_result)
}

impl TaskSource for SystemdAdapter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Systemd
    }

    fn collect(&self) -> Result<Vec<ScheduledTask>> {
        let probe = Command::new("systemctl").arg("--version").output();
        match probe {
            Ok(o) if o.status.success() => {}
            _ => {
                return Err(Error::Unavailable(
                    "`systemctl` not available on this host".into(),
                ))
            }
        }

        let listing = Command::new("systemctl")
            .args(["list-timers", "--all", "--output=json", "--no-pager"])
            .output()
            .map_err(|e| Error::Command {
                command: "systemctl list-timers".into(),
                message: e.to_string(),
            })?;
        if !listing.status.success() {
            return Err(Error::Command {
                command: "systemctl list-timers".into(),
                message: String::from_utf8_lossy(&listing.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&listing.stdout);
        let mut tasks = Self::parse_list_timers_scoped(&stdout, Scope::System)?;

        // The user manager is optional: no session bus, no lingering, or
        // running as root without XDG_RUNTIME_DIR all make `systemctl
        // --user` exit non-zero. That is "nothing to report", never an
        // adapter error.
        if let Some(json) = run_list_timers(Scope::User) {
            if let Ok(user_tasks) = Self::parse_list_timers_scoped(&json, Scope::User) {
                tasks.extend(user_tasks);
            }
        }

        for task in &mut tasks {
            let (scope, unit) = split_task_id(&task.id);
            let timer_text = run_show(scope, unit, TIMER_SHOW_PROPERTIES);
            let service = task.command.clone();
            let service_text = if service.is_empty() {
                None
            } else {
                run_show(scope, &service, SERVICE_SHOW_PROPERTIES)
            };
            Self::apply_show(task, timer_text.as_deref(), service_text.as_deref());
        }

        Ok(tasks)
    }
}

/// `systemctl [--user] list-timers` stdout, or `None` when that manager
/// is not reachable.
fn run_list_timers(scope: Scope) -> Option<String> {
    let out = Command::new("systemctl")
        .args(scope.systemctl_args())
        .args(["list-timers", "--all", "--output=json", "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShowTimer {
    pub on_calendar: Option<String>,
    pub on_interval: Option<Duration>,
    pub result: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShowService {
    pub exec_start: Option<String>,
    /// `Result=` of the service: `success`, `exit-code`, `signal`,
    /// `timeout`, `oom-kill`, `start-limit-hit`, ...
    pub result: Option<String>,
    pub active_state: Option<String>,
    pub sub_state: Option<String>,
    /// `ExecMainStartTimestampMonotonic` / `ExecMainExitTimestampMonotonic`
    /// in microseconds since boot; `None` when systemd reports `0`.
    pub main_start_us: Option<u64>,
    pub main_exit_us: Option<u64>,
}

impl ShowService {
    /// `activating` covers oneshot services mid-run; `active`+`running`
    /// covers simple services. `active`+`exited` (RemainAfterExit) is
    /// finished, not running.
    pub fn is_running(&self) -> bool {
        matches!(
            (self.active_state.as_deref(), self.sub_state.as_deref()),
            (Some("activating"), _) | (Some("deactivating"), _) | (Some("active"), Some("running"))
        )
    }

    /// Wall time of the last main-process run, when both stamps are set.
    pub fn main_duration(&self) -> Option<Duration> {
        let (start, exit) = (self.main_start_us?, self.main_exit_us?);
        (exit >= start).then(|| Duration::from_micros(exit - start))
    }
}

fn run_show(scope: Scope, unit: &str, properties: &str) -> Option<String> {
    let prop_arg = format!("--property={properties}");
    let out = Command::new("systemctl")
        .args(scope.systemctl_args())
        .args(["show", unit, &prop_arg, "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn map_result(result: String) -> TaskStatus {
    match result.as_str() {
        "success" => TaskStatus::Success,
        other => TaskStatus::Failed(other.to_string()),
    }
}

fn micros_to_dt(us: i64) -> Option<DateTime<Utc>> {
    if us <= 0 {
        return None;
    }
    let secs = us / 1_000_000;
    let nanos = ((us % 1_000_000) * 1000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

fn trim_unit_suffix(unit: &str) -> &str {
    unit.strip_suffix(".timer").unwrap_or(unit)
}

/// Pull a `key=value` subfield out of a systemd record like
/// `{ path=...; argv[]=...; ignore_errors=no }`. Value runs up to the next ` ; `.
fn extract_property_subfield(record: &str, key: &str) -> Option<String> {
    let trimmed = record.trim().trim_start_matches('{').trim_end_matches('}');
    let pos = trimmed.find(key)?;
    let after = &trimmed[pos + key.len()..];
    let end = after.find(" ; ").unwrap_or(after.len());
    let value = after[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn extract_monotonic_value(record: &str) -> Option<String> {
    let trimmed = record.trim().trim_start_matches('{').trim_end_matches('}');
    let segment = trimmed.split(" ; ").next()?.trim();
    let (_, value) = segment.split_once('=')?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// systemd duration: `15min`, `1h`, `30s`, `1h30min`, `500ms`.
fn parse_systemd_duration(s: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut digits = String::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(c);
            chars.next();
            continue;
        }
        if !digits.is_empty() {
            let mut unit = String::new();
            while let Some(&u) = chars.peek() {
                if u.is_ascii_digit() {
                    break;
                }
                unit.push(u);
                chars.next();
            }
            let n: u64 = digits.parse().ok()?;
            digits.clear();
            let factor = match unit.trim() {
                "ms" => return Some(total + Duration::from_millis(n)),
                "s" | "sec" | "secs" | "second" | "seconds" => 1,
                "m" | "min" | "mins" | "minute" | "minutes" => 60,
                "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
                "d" | "day" | "days" => 86_400,
                "w" | "week" | "weeks" => 7 * 86_400,
                _ => return None,
            };
            total += Duration::from_secs(n * factor);
        } else {
            return None;
        }
    }
    if digits.is_empty() && total > Duration::ZERO {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Three rows covering the shapes systemctl actually produces:
    //   - snapper-cleanup: integer `next`, `last: 0` (never run)
    //   - logrotate: integer `next` and `last`
    //   - apport-autoreport: `next: null`, `last: 0` — Ubuntu's
    //     canonical "no scheduled elapse" shape that broke us once.
    const LIST_TIMERS_FIXTURE: &str = r#"[
        {"next":1775916894043109,"left":1775916894043109,"last":0,"passed":0,"unit":"snapper-cleanup.timer","activates":"snapper-cleanup.service"},
        {"next":1775919186922121,"left":1775919186922121,"last":1775499594111976,"passed":0,"unit":"logrotate.timer","activates":"logrotate.service"},
        {"next":null,"left":null,"last":0,"passed":0,"unit":"apport-autoreport.timer","activates":"apport-autoreport.service"}
    ]"#;

    #[test]
    fn parse_list_timers_extracts_units_and_timestamps() {
        let tasks = SystemdAdapter::parse_list_timers(LIST_TIMERS_FIXTURE).unwrap();
        assert_eq!(tasks.len(), 3);

        let snapper = &tasks[0];
        assert_eq!(snapper.id, "snapper-cleanup.timer");
        assert_eq!(snapper.name, "snapper-cleanup");
        assert_eq!(snapper.source, TaskSourceKind::Systemd);
        assert!(snapper.last_run.is_none());
        assert!(snapper.next_run.is_some());
        assert_eq!(snapper.command, "snapper-cleanup.service");

        let logrotate = &tasks[1];
        assert!(logrotate.last_run.is_some());
        assert_eq!(logrotate.command, "logrotate.service");

        let apport = &tasks[2];
        assert_eq!(apport.id, "apport-autoreport.timer");
        assert!(apport.next_run.is_none());
        assert!(apport.last_run.is_none());
    }

    #[test]
    fn parse_list_timers_inactive_next_is_none() {
        let json = r#"[{"next":0,"left":0,"last":0,"passed":0,"unit":"foo.timer","activates":"foo.service"}]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert!(tasks[0].next_run.is_none());
        assert!(tasks[0].last_run.is_none());
    }

    /// Regression: newer `systemctl list-timers --output=json` returns
    /// JSON `null` (not `0`) for unset elapse times. Older shuvjobs versions
    /// declared the field as `i64` and panicked at deserialization.
    #[test]
    fn parse_list_timers_handles_null_next_and_last() {
        let json = r#"[{"next":null,"left":null,"last":null,"passed":null,"unit":"foo.timer","activates":"foo.service"}]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].next_run.is_none());
        assert!(tasks[0].last_run.is_none());
        assert_eq!(tasks[0].id, "foo.timer");
    }

    #[test]
    fn parse_list_timers_handles_mixed_null_and_int() {
        let json = r#"[
            {"next":1775919186922121,"left":null,"last":null,"passed":null,"unit":"never-run.timer","activates":"never-run.service"},
            {"next":1775919186922121,"left":null,"last":1775499594111976,"passed":null,"unit":"once.timer","activates":"once.service"}
        ]"#;
        let tasks = SystemdAdapter::parse_list_timers(json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].next_run.is_some());
        assert!(tasks[0].last_run.is_none());
        assert!(tasks[1].next_run.is_some());
        assert!(tasks[1].last_run.is_some());
    }

    #[test]
    fn parse_list_timers_returns_empty_for_empty_array() {
        let tasks = SystemdAdapter::parse_list_timers("[]").unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_list_timers_surfaces_parse_errors() {
        let err = SystemdAdapter::parse_list_timers("not json").unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    const SHOW_TIMER_SUCCESS: &str =
        "TimersCalendar={ OnCalendar=*-*-* 00:00:00 ; next_elapse=Sat 2026-04-11 17:04:57 +03 }\n\
                                       LastTriggerUSec=Mon 2026-04-06 21:19:54 +03\n\
                                       Result=success\n";

    #[test]
    fn parse_show_timer_extracts_oncalendar_and_success() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_SUCCESS);
        assert_eq!(parsed.on_calendar.as_deref(), Some("*-*-* 00:00:00"));
        assert!(parsed.on_interval.is_none());
        assert_eq!(parsed.result.as_deref(), Some("success"));
    }

    const SHOW_TIMER_FAILED: &str =
        "TimersCalendar={ OnCalendar=daily ; next_elapse=Sat 2026-04-11 00:00:00 +03 }\n\
                                      Result=failed\n";

    #[test]
    fn parse_show_timer_maps_failed_result() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_FAILED);
        assert_eq!(parsed.result.as_deref(), Some("failed"));
        let mapped = map_result(parsed.result.unwrap());
        assert!(matches!(mapped, TaskStatus::Failed(ref s) if s == "failed"));
    }

    // Interval timer (no calendar entry, OnBootSec/OnUnitActiveSec only).
    const SHOW_TIMER_INTERVAL: &str = "TimersCalendar=\n\
                                        TimersMonotonic={ OnBootSec=15min ; next_elapse=4h 12min left }\n\
                                        Result=success\n";

    #[test]
    fn parse_show_timer_extracts_interval() {
        let parsed = SystemdAdapter::parse_show_timer(SHOW_TIMER_INTERVAL);
        assert!(parsed.on_calendar.is_none());
        assert_eq!(parsed.on_interval, Some(Duration::from_secs(15 * 60)));
    }

    const SHOW_SERVICE_FIXTURE: &str = "ExecStart={ path=/usr/sbin/logrotate ; argv[]=/usr/sbin/logrotate /etc/logrotate.conf ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";

    #[test]
    fn parse_show_service_pulls_argv() {
        let parsed = SystemdAdapter::parse_show_service(SHOW_SERVICE_FIXTURE);
        assert_eq!(
            parsed.exec_start.as_deref(),
            Some("/usr/sbin/logrotate /etc/logrotate.conf")
        );
        assert!(parsed.result.is_none());
        assert!(parsed.main_duration().is_none());
    }

    // Captured from `systemctl show snapper-cleanup.service` after a
    // successful oneshot run: 24.6ms between main-process start and exit.
    const SHOW_SERVICE_FULL: &str = "\
ExecStart={ path=/usr/lib/snapper/systemd-helper ; argv[]=/usr/lib/snapper/systemd-helper --cleanup ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }
Result=success
ActiveState=inactive
SubState=dead
ExecMainStartTimestampMonotonic=40202351753
ExecMainExitTimestampMonotonic=40202376368
";

    #[test]
    fn parse_show_service_reads_result_state_and_timestamps() {
        let parsed = SystemdAdapter::parse_show_service(SHOW_SERVICE_FULL);
        assert_eq!(parsed.result.as_deref(), Some("success"));
        assert_eq!(parsed.active_state.as_deref(), Some("inactive"));
        assert_eq!(parsed.sub_state.as_deref(), Some("dead"));
        assert_eq!(parsed.main_duration(), Some(Duration::from_micros(24_615)));
        assert!(!parsed.is_running());
    }

    #[test]
    fn parse_show_service_treats_zero_timestamps_as_absent() {
        let text = "ExecMainStartTimestampMonotonic=0\nExecMainExitTimestampMonotonic=0\n";
        let parsed = SystemdAdapter::parse_show_service(text);
        assert!(parsed.main_start_us.is_none());
        assert!(parsed.main_duration().is_none());
    }

    fn listed_task(last_run: bool) -> ScheduledTask {
        ScheduledTask {
            id: "logrotate.timer".into(),
            name: "logrotate".into(),
            source: TaskSourceKind::Systemd,
            schedule: ScheduleType::Calendar(String::new()),
            last_run: last_run.then(|| Utc.timestamp_opt(1_775_499_594, 0).single().unwrap()),
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "logrotate.service".into(),
        }
    }

    #[test]
    fn apply_show_merges_calendar_command_status_and_duration() {
        let mut task = listed_task(true);
        SystemdAdapter::apply_show(&mut task, Some(SHOW_TIMER_SUCCESS), Some(SHOW_SERVICE_FULL));
        assert!(matches!(task.schedule, ScheduleType::Calendar(ref s) if s == "*-*-* 00:00:00"));
        assert_eq!(task.command, "/usr/lib/snapper/systemd-helper --cleanup");
        assert_eq!(task.last_status, Some(TaskStatus::Success));
        assert_eq!(task.last_duration, Some(Duration::from_micros(24_615)));
    }

    /// The whole point of reading the service: a failed activation must
    /// surface even though the timer unit itself still says `success`.
    #[test]
    fn apply_show_reports_service_failure_over_timer_success() {
        let service = "Result=exit-code\nActiveState=failed\nSubState=failed\n";
        let mut task = listed_task(true);
        SystemdAdapter::apply_show(&mut task, Some(SHOW_TIMER_SUCCESS), Some(service));
        assert_eq!(
            task.last_status,
            Some(TaskStatus::Failed("exit-code".into()))
        );
    }

    #[test]
    fn apply_show_reports_running_for_activating_oneshot() {
        let service = "Result=success\nActiveState=activating\nSubState=start\n";
        let mut task = listed_task(true);
        SystemdAdapter::apply_show(&mut task, None, Some(service));
        assert_eq!(task.last_status, Some(TaskStatus::Running));
    }

    #[test]
    fn apply_show_remain_after_exit_is_not_running() {
        let service = "Result=success\nActiveState=active\nSubState=exited\n";
        let mut task = listed_task(true);
        SystemdAdapter::apply_show(&mut task, None, Some(service));
        assert_eq!(task.last_status, Some(TaskStatus::Success));
    }

    #[test]
    fn apply_show_never_triggered_service_has_no_status() {
        let service = "Result=success\nActiveState=inactive\nSubState=dead\n\
                       ExecMainStartTimestampMonotonic=0\nExecMainExitTimestampMonotonic=0\n";
        let mut task = listed_task(false);
        SystemdAdapter::apply_show(&mut task, Some(SHOW_TIMER_SUCCESS), Some(service));
        assert_eq!(task.last_status, None);
    }

    #[test]
    fn apply_show_falls_back_to_timer_result_without_service() {
        let mut task = listed_task(true);
        SystemdAdapter::apply_show(&mut task, Some(SHOW_TIMER_FAILED), None);
        assert_eq!(task.last_status, Some(TaskStatus::Failed("failed".into())));

        let mut never = listed_task(false);
        SystemdAdapter::apply_show(&mut never, Some(SHOW_TIMER_SUCCESS), None);
        assert_eq!(never.last_status, None);
    }

    #[test]
    fn apply_show_is_no_op_without_inputs() {
        let mut task = listed_task(true);
        let snapshot = task.clone();
        SystemdAdapter::apply_show(&mut task, None, None);
        assert_eq!(task, snapshot);
    }

    #[test]
    fn parse_systemd_duration_handles_common_forms() {
        assert_eq!(
            parse_systemd_duration("15min"),
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            parse_systemd_duration("1h"),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(parse_systemd_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_systemd_duration("1h30min"),
            Some(Duration::from_secs(5400))
        );
        assert_eq!(parse_systemd_duration("garbage"), None);
    }

    // Captured verbatim from `systemctl --user list-timers --all
    // --output=json --no-pager` on an Arch host with a user manager.
    const USER_LIST_TIMERS_FIXTURE: &str = r#"[
        {"next":1788338408746484,"left":1788338408746484,"last":1788336608745417,"passed":43508749618,"unit":"bun-compile-tmp-cleanup.timer","activates":"bun-compile-tmp-cleanup.service"},
        {"next":1788394500000000,"left":1788394500000000,"last":1788308100017251,"passed":15000021453,"unit":"radar-daily.timer","activates":"radar-daily.service"},
        {"next":1788741000000000,"left":1788741000000000,"last":1788136200512834,"passed":0,"unit":"radar-weekly.timer","activates":"radar-weekly.service"}
    ]"#;

    #[test]
    fn parse_list_timers_scoped_user_prefixes_ids_and_labels_names() {
        let tasks = SystemdAdapter::parse_list_timers_scoped(USER_LIST_TIMERS_FIXTURE, Scope::User)
            .unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "user/bun-compile-tmp-cleanup.timer");
        assert_eq!(tasks[0].name, "bun-compile-tmp-cleanup (user)");
        assert_eq!(tasks[0].command, "bun-compile-tmp-cleanup.service");
        assert!(tasks[0].last_run.is_some());
        assert!(tasks[1].next_run.is_some());
        assert_eq!(tasks[2].id, "user/radar-weekly.timer");
    }

    /// The same unit name in both managers must not collide.
    #[test]
    fn scoped_ids_are_unique_across_scopes() {
        let json = r#"[{"next":0,"left":0,"last":0,"passed":0,"unit":"backup.timer","activates":"backup.service"}]"#;
        let system = SystemdAdapter::parse_list_timers_scoped(json, Scope::System).unwrap();
        let user = SystemdAdapter::parse_list_timers_scoped(json, Scope::User).unwrap();
        assert_eq!(system[0].id, "backup.timer");
        assert_eq!(user[0].id, "user/backup.timer");
        assert_ne!(system[0].id, user[0].id);
        assert_ne!(system[0].name, user[0].name);
    }

    #[test]
    fn parse_list_timers_defaults_to_system_scope() {
        let tasks = SystemdAdapter::parse_list_timers(LIST_TIMERS_FIXTURE).unwrap();
        let scoped =
            SystemdAdapter::parse_list_timers_scoped(LIST_TIMERS_FIXTURE, Scope::System).unwrap();
        assert_eq!(tasks, scoped);
    }

    #[test]
    fn split_task_id_round_trips_both_scopes() {
        assert_eq!(
            split_task_id(&Scope::System.task_id("logrotate.timer")),
            (Scope::System, "logrotate.timer")
        );
        assert_eq!(
            split_task_id(&Scope::User.task_id("radar-daily.timer")),
            (Scope::User, "radar-daily.timer")
        );
    }

    #[test]
    fn scope_supplies_the_user_flag() {
        assert!(Scope::System.systemctl_args().is_empty());
        assert_eq!(Scope::User.systemctl_args(), ["--user"]);
    }

    #[test]
    fn adapter_reports_systemd_kind() {
        assert_eq!(SystemdAdapter::new().kind(), TaskSourceKind::Systemd);
    }
}
