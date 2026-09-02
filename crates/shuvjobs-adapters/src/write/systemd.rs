//! The systemd timer writer.
//!
//! A systemd job is two files, not one: a `.timer` that says when, and a
//! `.service` that says what. Both are generated here and both carry the
//! managed marker on their first line, because the only way to tell a
//! unit we may overwrite from one the operator hand-wrote is to have
//! said so when we wrote it.
//!
//! Everything else follows from where those files live. The scope picks
//! the directory (`~/.config/systemd/user` or `/etc/systemd/system`) and
//! therefore both the privilege and the `--user` flag on every
//! `systemctl` call, and the same three managed directories are the only
//! places update and delete will touch: a vendor unit under `/usr/lib`
//! is refused rather than shadowed, because rewriting it would be undone
//! by the next package upgrade.
//!
//! The plan is checked before it is applied (`systemd-analyze calendar`
//! when we can) and after (`LoadState=loaded`, else the two files are
//! removed again), so a typo in an `OnCalendar=` expression never leaves
//! a broken unit behind.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use shuvjobs_core::host::shell::{mkdir_script, shell_quote};
use shuvjobs_core::host::{Host, Privilege};
use shuvjobs_core::manage::{
    execute, ApplyMode, Change, FailPolicy, JobScope, JobSpec, JobWriter, MutationOutcome,
};
use shuvjobs_core::{Error, Result, ScheduleType, TaskSourceKind};

use crate::ids::parse_systemd_id;
use crate::systemd::{Scope, SystemdAdapter};
use crate::write::calendar::{cron_to_oncalendar, format_systemd_duration};
use crate::write::{validate_command, validate_simple_name, NameKind, MANAGED_MARKER};

/// Where a system-scope unit we write belongs. `/etc` outranks the
/// vendor tree, so a unit here also wins over a packaged one.
pub const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// The administrator's drop-in directory for *user* managers. We never
/// create files here ourselves, but a unit already living here is ours
/// to edit.
pub const SYSTEM_USER_UNIT_DIR: &str = "/etc/systemd/user";

/// Path of the per-user unit directory under `home`.
fn user_unit_dir(home: &str) -> String {
    format!("{}/.config/systemd/user", home.trim_end_matches('/'))
}

/// Properties every locate step asks for. One string so the timer and
/// the service are looked up by the identical command.
const SHOW_PROPERTIES: &str = "Id,FragmentPath,UnitFileState,ActiveState,Triggers";

/// Properties the post-write check asks for.
const LOAD_PROPERTIES: &str = "LoadState,LoadError";

/// Option key for `Persistent=`: catch-up runs after downtime, on by
/// default because a missed backup is usually worth running late.
const PERSISTENT_OPTION: &str = "systemd.persistent";

fn unsupported(id: &str, reason: &str) -> Error {
    Error::Unsupported {
        source_kind: TaskSourceKind::Systemd,
        id: id.to_string(),
        reason: reason.to_string(),
    }
}

/// `systemctl --user` over SSH without a session bus fails in a way that
/// looks like a broken command; it is really a missing precondition.
fn bus_unreachable(id: &str) -> Error {
    unsupported(
        id,
        "user manager unreachable (enable lingering / set XDG_RUNTIME_DIR)",
    )
}

fn is_bus_failure(text: &str) -> bool {
    text.contains("Failed to connect to bus")
}

/// Re-shape a failure that is really "there is no user manager here".
fn map_bus_error(err: Error, id: &str) -> Error {
    if is_bus_failure(&err.to_string()) {
        bus_unreachable(id)
    } else {
        err
    }
}

fn systemctl(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "systemctl",
        Scope::User => "systemctl --user",
    }
}

/// The system manager is root's; your own manager is yours.
fn privilege_for(scope: Scope) -> Privilege {
    match scope {
        Scope::System => Privilege::Root,
        Scope::User => Privilege::User,
    }
}

/// Writing under `/etc` needs root whatever the scope: a user unit an
/// administrator dropped in `/etc/systemd/user` is still a root file.
fn file_privilege(scope: Scope, path: &str) -> Privilege {
    if scope == Scope::System || path.starts_with("/etc/") {
        Privilege::Root
    } else {
        Privilege::User
    }
}

fn scope_of(scope: JobScope) -> Scope {
    match scope {
        JobScope::System => Scope::System,
        JobScope::User => Scope::User,
    }
}

fn daemon_reload(scope: Scope) -> Change {
    Change::Command {
        cmd: format!("{} daemon-reload", systemctl(scope)),
        stdin: None,
        privilege: privilege_for(scope),
        description: "reload the systemd manager".to_string(),
        on_fail: FailPolicy::Error,
    }
}

fn unit_command(
    scope: Scope,
    verb: &str,
    unit: &str,
    description: &str,
    on_fail: FailPolicy,
) -> Change {
    Change::Command {
        cmd: format!("{} {verb} {}", systemctl(scope), shell_quote(unit)),
        stdin: None,
        privilege: privilege_for(scope),
        description: description.to_string(),
        on_fail,
    }
}

fn enable_change(scope: Scope, unit: &str) -> Change {
    unit_command(
        scope,
        "enable --now",
        unit,
        &format!("enable and start {unit}"),
        FailPolicy::Error,
    )
}

fn disable_change(scope: Scope, unit: &str, on_fail: FailPolicy) -> Change {
    unit_command(
        scope,
        "disable --now",
        unit,
        &format!("stop and disable {unit}"),
        on_fail,
    )
}

/// `Key=value` lines from `systemctl show`, last value winning.
fn parse_properties(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            out.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    out
}

fn property<'a>(props: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    props.get(key).map_or("", String::as_str)
}

/// The unit a timer triggers, from `Triggers=`. systemd lists every
/// triggered unit; only a service is a job we can rewrite.
fn first_service(triggers: &str) -> Option<String> {
    triggers
        .split_whitespace()
        .find(|unit| unit.ends_with(".service"))
        .map(str::to_string)
}

/// The directory a unit file sits in, or `""` for a bare name.
fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// What a timer's `[Timer]` section has to say.
///
/// Kept as a value rather than rendered straight to text because the
/// pre-flight `systemd-analyze calendar` check needs the expression on
/// its own, and only the calendar shapes have one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimerRule {
    Calendar(String),
    /// `@reboot`: once, shortly after the manager comes up.
    Boot,
    Every(Duration),
    Once(DateTime<Utc>),
}

impl TimerRule {
    /// The `OnCalendar=` value, when there is one to validate.
    fn oncalendar(&self) -> Option<String> {
        match self {
            Self::Calendar(expr) => Some(expr.clone()),
            Self::Once(when) => Some(when.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
            Self::Boot | Self::Every(_) => None,
        }
    }

    /// The body of the `[Timer]` section.
    ///
    /// `Persistent=` only means anything for a calendar rule: monotonic
    /// timers have no missed wall-clock elapse to catch up on, and a
    /// one-shot that already passed should stay passed.
    fn lines(&self, persistent: bool) -> Vec<String> {
        match self {
            Self::Calendar(expr) => vec![
                format!("OnCalendar={expr}"),
                format!("Persistent={persistent}"),
            ],
            Self::Boot => vec!["OnBootSec=0".to_string()],
            Self::Every(interval) => {
                let every = format_systemd_duration(*interval);
                vec![
                    format!("OnBootSec={every}"),
                    format!("OnUnitActiveSec={every}"),
                ]
            }
            Self::Once(when) => vec![
                format!("OnCalendar={}", when.format("%Y-%m-%d %H:%M:%S UTC")),
                "Persistent=false".to_string(),
            ],
        }
    }
}

/// run-parts spells the shorthands with an `@`; systemd does not.
fn calendar_alias(text: &str) -> Option<&'static str> {
    Some(match text.to_ascii_lowercase().as_str() {
        "@hourly" => "hourly",
        "@daily" => "daily",
        "@weekly" => "weekly",
        "@monthly" => "monthly",
        "@yearly" | "@annually" => "yearly",
        _ => return None,
    })
}

fn timer_rule(schedule: &ScheduleType) -> Result<TimerRule> {
    match schedule {
        ScheduleType::Calendar(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Err(Error::Validation("schedule must not be empty".into()));
            }
            Ok(TimerRule::Calendar(
                calendar_alias(text).unwrap_or(text).to_string(),
            ))
        }
        ScheduleType::Cron(expr) => {
            if expr.trim().eq_ignore_ascii_case("@reboot") {
                return Ok(TimerRule::Boot);
            }
            Ok(TimerRule::Calendar(cron_to_oncalendar(expr)?))
        }
        ScheduleType::Interval(interval) => {
            if interval.as_secs() == 0 {
                return Err(Error::Validation(
                    "a repeating timer needs an interval longer than zero".into(),
                ));
            }
            Ok(TimerRule::Every(*interval))
        }
        ScheduleType::OneShot(when) => Ok(TimerRule::Once(*when)),
    }
}

/// The `.timer` file for a job.
pub fn render_timer(name: &str, schedule: &ScheduleType, persistent: bool) -> Result<String> {
    Ok(render_timer_rule(name, &timer_rule(schedule)?, persistent))
}

fn render_timer_rule(name: &str, rule: &TimerRule, persistent: bool) -> String {
    let body = rule.lines(persistent).join("\n");
    format!(
        "{MANAGED_MARKER}\n\
         [Unit]\n\
         Description={name} (managed by shuvjobs)\n\
         \n\
         [Timer]\n\
         {body}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// Escape a command for the double-quoted `ExecStart=` argument.
///
/// systemd's own unescaping runs first (`\` and `"`), and `%` and `$`
/// are its specifier and environment expansions, so all four have to be
/// doubled or backslashed or the job runs something the operator never
/// typed.
pub fn escape_exec_arg(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$")
}

/// The `.service` file for a job.
///
/// `User=` is only written for a real system service: it is rejected by
/// a user manager, and `root` is the default there anyway.
pub fn render_service(
    name: &str,
    command: &str,
    user: Option<&str>,
    description: Option<&str>,
) -> Result<String> {
    validate_command(command)?;
    let description = description.map_or_else(
        || format!("{name} (managed by shuvjobs)"),
        |text| text.trim().to_string(),
    );
    let mut out = format!(
        "{MANAGED_MARKER}\n\
         [Unit]\n\
         Description={description}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=/bin/sh -c \"{}\"\n",
        escape_exec_arg(command.trim())
    );
    if let Some(user) = user.filter(|user| *user != "root") {
        out.push_str(&format!("User={user}\n"));
    }
    Ok(out)
}

/// A unit we found on the host, and the service it drives.
#[derive(Debug, Clone)]
struct Located {
    timer_path: String,
    service_unit: String,
    /// `None` when the service has no fragment of its own (generated, or
    /// simply missing).
    service_path: Option<String>,
}

/// Creates, edits, and removes systemd timers on one [`Host`].
pub struct SystemdWriter {
    host: Arc<dyn Host>,
}

impl SystemdWriter {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }

    /// The three directories whose units are ours to rewrite. Anything
    /// else — `/usr/lib/systemd`, `/run/systemd` — belongs to a package
    /// or to the manager itself.
    fn managed_dirs(&self) -> Result<Vec<String>> {
        Ok(vec![
            SYSTEM_UNIT_DIR.to_string(),
            SYSTEM_USER_UNIT_DIR.to_string(),
            user_unit_dir(&self.host.home_dir()?),
        ])
    }

    /// Where a new unit goes: the scope's own directory, or an explicit
    /// location as long as it is one we manage.
    fn target_dir(&self, scope: Scope, location: Option<&str>) -> Result<String> {
        let dirs = self.managed_dirs()?;
        let Some(location) = location else {
            return Ok(match scope {
                Scope::System => SYSTEM_UNIT_DIR.to_string(),
                Scope::User => user_unit_dir(&self.host.home_dir()?),
            });
        };
        let location = location.trim_end_matches('/').to_string();
        if dirs.contains(&location) {
            return Ok(location);
        }
        Err(Error::Validation(format!(
            "`{location}` is not a systemd unit directory shuvjobs manages: expected one of {}",
            dirs.join(", ")
        )))
    }

    fn is_managed_dir(&self, path: &str) -> Result<bool> {
        Ok(self
            .managed_dirs()?
            .iter()
            .any(|dir| dir == parent_dir(path)))
    }

    /// Raw `systemctl show` output for one unit.
    fn show_text(&self, scope: Scope, unit: &str, properties: &str) -> Result<String> {
        let cmd = format!(
            "{} show {} --property={properties} --no-pager",
            systemctl(scope),
            shell_quote(unit)
        );
        let output = self
            .host
            .run(&cmd, None, privilege_for(scope))
            .map_err(|err| map_bus_error(err, unit))?;
        if !output.success() {
            if is_bus_failure(&output.stderr) {
                return Err(bus_unreachable(&scope.task_id(unit)));
            }
            return output.require_success(&cmd);
        }
        Ok(output.stdout_str().into_owned())
    }

    fn show(&self, scope: Scope, unit: &str, properties: &str) -> Result<BTreeMap<String, String>> {
        Ok(parse_properties(&self.show_text(scope, unit, properties)?))
    }

    /// Find a timer's files, refusing anything we would be shadowing
    /// rather than editing.
    fn locate(&self, scope: Scope, unit: &str, id: &str, vendor_reason: &str) -> Result<Located> {
        let props = self.show(scope, unit, SHOW_PROPERTIES)?;
        let timer_path = property(&props, "FragmentPath").to_string();
        if timer_path.is_empty() {
            return Err(Error::Conflict(format!("{id}: unit not found")));
        }
        if !self.is_managed_dir(&timer_path)? {
            return Err(unsupported(id, vendor_reason));
        }
        let stem = unit.strip_suffix(".timer").unwrap_or(unit);
        let service_unit = first_service(property(&props, "Triggers"))
            .unwrap_or_else(|| format!("{stem}.service"));
        let service_props = self.show(scope, &service_unit, SHOW_PROPERTIES)?;
        let service_path = Some(property(&service_props, "FragmentPath").to_string())
            .filter(|path| !path.is_empty());
        Ok(Located {
            timer_path,
            service_unit,
            service_path,
        })
    }

    /// An existing unit file we may overwrite, or `None` when the path is
    /// free. A file without our marker is somebody else's.
    fn read_existing(&self, path: &str, privilege: Privilege, id: &str) -> Result<Option<String>> {
        let Some(text) = self.host.read_to_string(path, privilege)? else {
            return Ok(None);
        };
        if !text.contains(MANAGED_MARKER) {
            return Err(Error::Conflict(format!(
                "{id}: {path} already exists and was not written by shuvjobs; \
                 move it aside or pick another name"
            )));
        }
        Ok(Some(text))
    }

    /// Ask `systemd-analyze` whether the expression parses.
    ///
    /// `Ok(Some(change))` keeps the check in the plan so it runs again at
    /// apply time; `Ok(None)` means the binary is missing, which is not
    /// the operator's fault and must not block the write.
    fn check_calendar(&self, scope: Scope, expr: &str) -> Result<Option<Change>> {
        let cmd = format!("systemd-analyze calendar {}", shell_quote(expr));
        let output = self.host.run(&cmd, None, privilege_for(scope))?;
        if output.code == Some(127) {
            return Ok(None);
        }
        if !output.success() {
            let detail = output.stderr.trim();
            return Err(Error::Validation(format!(
                "`{expr}` is not a valid OnCalendar expression: {detail}"
            )));
        }
        Ok(Some(Change::Command {
            cmd,
            stdin: None,
            privilege: privilege_for(scope),
            description: format!("check the calendar expression `{expr}`"),
            on_fail: FailPolicy::Error,
        }))
    }

    /// After the write: a unit that did not load is worse than no unit,
    /// so take the two files back out and say why.
    fn verify_loaded(
        &self,
        scope: Scope,
        unit: &str,
        timer_path: &str,
        service_path: &str,
    ) -> Result<()> {
        let props = self.show(scope, unit, LOAD_PROPERTIES)?;
        if property(&props, "LoadState") == "loaded" {
            return Ok(());
        }
        for path in [timer_path, service_path] {
            let _ = self.host.remove_file(path, file_privilege(scope, path));
        }
        let _ = self.host.run(
            &format!("{} daemon-reload", systemctl(scope)),
            None,
            privilege_for(scope),
        );
        let detail = match property(&props, "LoadError") {
            "" => "unit failed to load".to_string(),
            error => error.to_string(),
        };
        Err(Error::Validation(format!(
            "{unit} did not load after it was written ({detail}); the unit files were removed"
        )))
    }

    /// `Persistent=`, unless the operator turned it off.
    fn persistent(spec: &JobSpec) -> bool {
        spec.options
            .get(PERSISTENT_OPTION)
            .is_none_or(|value| value != "false")
    }

    /// `User=` for the service, refusing it where it cannot work.
    fn service_user(scope: Scope, spec: &JobSpec) -> Result<Option<&str>> {
        match (scope, spec.user.as_deref()) {
            (Scope::User, Some(user)) => Err(Error::Validation(format!(
                "a user timer always runs as you: `User={user}` needs --scope system"
            ))),
            (Scope::User, None) => Ok(None),
            (Scope::System, user) => Ok(user),
        }
    }

    /// Leave an unmanaged service alone when it already runs what the
    /// spec asks for, and refuse to guess when it does not.
    fn require_same_command(
        &self,
        scope: Scope,
        service_unit: &str,
        command: &str,
        id: &str,
    ) -> Result<()> {
        let text = self.show_text(scope, service_unit, "ExecStart")?;
        let current = SystemdAdapter::parse_show_service(&text)
            .exec_start
            .unwrap_or_default();
        if commands_match(&current, command) {
            return Ok(());
        }
        Err(unsupported(
            id,
            "service is not managed by shuvjobs; edit ExecStart manually",
        ))
    }
}

/// Whether a service's `ExecStart` argv already runs `command`.
///
/// systemd reports the argv, so our own `/bin/sh -c <command>` comes
/// back with the wrapper still on the front.
fn commands_match(exec_start: &str, command: &str) -> bool {
    let exec = exec_start.trim();
    let want = command.trim();
    if exec == want {
        return true;
    }
    ["/bin/sh -c ", "/usr/bin/sh -c ", "sh -c "]
        .iter()
        .any(|prefix| exec.strip_prefix(prefix).is_some_and(|r| r.trim() == want))
}

impl JobWriter for SystemdWriter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Systemd
    }

    fn create(&self, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        let scope = scope_of(spec.scope);
        let name = spec.name.as_deref().ok_or_else(|| {
            Error::Validation(
                "a systemd timer needs a name: it becomes <name>.timer and <name>.service".into(),
            )
        })?;
        validate_simple_name(name, NameKind::Systemd)?;
        let unit = format!("{name}.timer");
        let id = scope.task_id(&unit);

        let dir = self.target_dir(scope, spec.location.as_deref())?;
        let timer_path = format!("{dir}/{name}.timer");
        let service_path = format!("{dir}/{name}.service");
        let privilege = file_privilege(scope, &timer_path);

        let rule = timer_rule(&spec.schedule)?;
        let user = Self::service_user(scope, spec)?;
        let timer = render_timer_rule(name, &rule, Self::persistent(spec));
        let service = render_service(name, &spec.command, user, spec.description.as_deref())?;

        let before_timer = self.read_existing(&timer_path, privilege, &id)?;
        let before_service = self.read_existing(&service_path, privilege, &id)?;

        let mut changes = Vec::new();
        let mut notes = Vec::new();
        if scope == Scope::User {
            changes.push(Change::Command {
                cmd: mkdir_script(&dir),
                stdin: None,
                privilege: Privilege::User,
                description: format!("create {dir}"),
                on_fail: FailPolicy::Error,
            });
        }
        if let Some(expr) = rule.oncalendar() {
            match self.check_calendar(scope, &expr)? {
                Some(change) => changes.push(change),
                None => notes
                    .push("systemd-analyze unavailable; expression not pre-validated".to_string()),
            }
        }
        changes.push(Change::WriteFile {
            path: timer_path.clone(),
            before: before_timer,
            after: timer,
            mode: 0o644,
            privilege,
        });
        changes.push(Change::WriteFile {
            path: service_path.clone(),
            before: before_service,
            after: service,
            mode: 0o644,
            privilege,
        });
        changes.push(daemon_reload(scope));
        if spec.enabled {
            changes.push(enable_change(scope, &unit));
        }

        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_bus_error(err, &id))?;
        outcome.notes.extend(notes);
        outcome.id = Some(id);
        if mode == ApplyMode::Apply {
            self.verify_loaded(scope, &unit, &timer_path, &service_path)?;
        }
        Ok(outcome)
    }

    fn update(&self, id: &str, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        validate_command(&spec.command)?;
        let parsed = parse_systemd_id(id)?;
        let scope = parsed.scope;
        let unit = parsed.unit;
        let stem = unit.strip_suffix(".timer").unwrap_or(&unit).to_string();

        if let Some(name) = spec.name.as_deref() {
            if name != stem {
                return Err(unsupported(
                    id,
                    "renaming a timer is not supported yet: create the new name and \
                     delete this one",
                ));
            }
        }

        let located = self.locate(
            scope,
            &unit,
            id,
            "vendor unit; copy it to /etc/systemd/system first, or use disable",
        )?;
        let user = Self::service_user(scope, spec)?;
        let timer_privilege = file_privilege(scope, &located.timer_path);
        let before_timer = self
            .host
            .read_to_string(&located.timer_path, timer_privilege)?;

        let mut changes = vec![Change::WriteFile {
            path: located.timer_path.clone(),
            before: before_timer,
            after: render_timer(&stem, &spec.schedule, Self::persistent(spec))?,
            mode: 0o644,
            privilege: timer_privilege,
        }];

        // A service we wrote is regenerated; one we did not is left
        // exactly as it is, and only accepted at all because it already
        // runs the command the spec asks for.
        match &located.service_path {
            Some(path) => {
                let privilege = file_privilege(scope, path);
                let before = self.host.read_to_string(path, privilege)?;
                if before
                    .as_deref()
                    .is_some_and(|text| text.contains(MANAGED_MARKER))
                {
                    changes.push(Change::WriteFile {
                        path: path.clone(),
                        before,
                        after: render_service(
                            &stem,
                            &spec.command,
                            user,
                            spec.description.as_deref(),
                        )?,
                        mode: 0o644,
                        privilege,
                    });
                } else {
                    self.require_same_command(scope, &located.service_unit, &spec.command, id)?;
                }
            }
            None => self.require_same_command(scope, &located.service_unit, &spec.command, id)?,
        }

        changes.push(daemon_reload(scope));
        changes.push(if spec.enabled {
            enable_change(scope, &unit)
        } else {
            disable_change(scope, &unit, FailPolicy::Error)
        });
        // The timer is already running with the old schedule; restarting
        // it is what makes the new one take effect, but a timer that was
        // deliberately left stopped must stay stopped.
        changes.push(unit_command(
            scope,
            "try-restart",
            &unit,
            &format!("restart {unit} if it is running"),
            FailPolicy::Ignore,
        ));

        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_bus_error(err, id))?;
        outcome.id = Some(id.to_string());
        Ok(outcome)
    }

    fn delete(&self, id: &str, mode: ApplyMode) -> Result<MutationOutcome> {
        let parsed = parse_systemd_id(id)?;
        let scope = parsed.scope;
        let unit = parsed.unit;
        let located = self.locate(
            scope,
            &unit,
            id,
            "vendor unit: use `shuvjobs disable` or `systemctl mask`",
        )?;

        let timer_privilege = file_privilege(scope, &located.timer_path);
        let before_timer = self
            .host
            .read_to_string(&located.timer_path, timer_privilege)?;

        // Stop first: removing the file under a running manager leaves
        // the unit loaded but unfindable.
        let mut changes = vec![
            disable_change(scope, &unit, FailPolicy::Ignore),
            Change::RemoveFile {
                path: located.timer_path.clone(),
                before: before_timer,
                privilege: timer_privilege,
            },
        ];
        if let Some(path) = &located.service_path {
            let privilege = file_privilege(scope, path);
            let before = self.host.read_to_string(path, privilege)?;
            if before
                .as_deref()
                .is_some_and(|text| text.contains(MANAGED_MARKER))
            {
                changes.push(Change::RemoveFile {
                    path: path.clone(),
                    before,
                    privilege,
                });
            }
        }
        changes.push(daemon_reload(scope));
        changes.push(unit_command(
            scope,
            "reset-failed",
            &unit,
            &format!("clear any failed state for {unit}"),
            FailPolicy::Ignore,
        ));

        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_bus_error(err, id))?;
        outcome.id = Some(id.to_string());
        Ok(outcome)
    }

    fn set_enabled(&self, id: &str, enabled: bool, mode: ApplyMode) -> Result<MutationOutcome> {
        let parsed = parse_systemd_id(id)?;
        let scope = parsed.scope;
        let unit = parsed.unit;
        // Enabling is a file operation on somebody's unit directory, but
        // it goes through systemctl, so a vendor unit is fair game here.
        let props = self.show(scope, &unit, SHOW_PROPERTIES)?;
        let change = match (property(&props, "UnitFileState"), enabled) {
            ("masked" | "masked-runtime", _) => {
                return Err(unsupported(id, "masked; unmask it first"))
            }
            // A static timer has no [Install] section, so there is
            // nothing to enable — but it can still be running.
            ("static", true) => {
                return Err(unsupported(
                    id,
                    "static timer; start it with systemctl start",
                ))
            }
            ("static", false) => unit_command(
                scope,
                "stop",
                &unit,
                &format!("stop {unit}"),
                FailPolicy::Error,
            ),
            (_, true) => enable_change(scope, &unit),
            (_, false) => disable_change(scope, &unit, FailPolicy::Error),
        };
        let mut outcome =
            execute(&*self.host, vec![change], mode).map_err(|err| map_bus_error(err, id))?;
        outcome.id = Some(id.to_string());
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuvjobs_core::host::fake::Call;
    use shuvjobs_core::host::{FakeHost, PrivilegePolicy};

    const USER_DIR: &str = "/home/tester/.config/systemd/user";

    fn sudoer() -> PrivilegePolicy {
        PrivilegePolicy {
            is_root: false,
            sudo: true,
        }
    }

    fn writer(host: &Arc<FakeHost>) -> SystemdWriter {
        SystemdWriter::new(Arc::clone(host) as Arc<dyn Host>)
    }

    fn spec(name: &str, schedule: ScheduleType, command: &str) -> JobSpec {
        let mut spec = JobSpec::new(TaskSourceKind::Systemd, schedule, command.to_string());
        spec.name = Some(name.to_string());
        spec
    }

    fn calendar_spec(name: &str, expr: &str, command: &str) -> JobSpec {
        spec(name, ScheduleType::Calendar(expr.to_string()), command)
    }

    fn call(cmd: &str, privilege: Privilege) -> Call {
        Call {
            cmd: cmd.to_string(),
            stdin: None,
            privilege,
        }
    }

    fn commands(host: &FakeHost) -> Vec<String> {
        host.calls().into_iter().map(|call| call.cmd).collect()
    }

    /// A host that answers every command a user-scope create issues.
    fn user_host_parts(expr: &str, name: &str) -> FakeHost {
        FakeHost::new()
            .script(&format!("systemd-analyze calendar '{expr}'"), 0, "", "")
            .script(&format!("mkdir -p '{USER_DIR}'"), 0, "", "")
            .script("systemctl --user daemon-reload", 0, "", "")
            .script(
                &format!("systemctl --user enable --now '{name}.timer'"),
                0,
                "",
                "",
            )
            .script(
                &format!(
                    "systemctl --user show '{name}.timer' \
                     --property=LoadState,LoadError --no-pager"
                ),
                0,
                "LoadState=loaded\nLoadError=\n",
                "",
            )
    }

    fn user_host(expr: &str, name: &str) -> Arc<FakeHost> {
        Arc::new(user_host_parts(expr, name))
    }

    // -- rendering -------------------------------------------------------

    #[test]
    fn a_cron_expression_becomes_a_calendar_timer() {
        assert_eq!(
            render_timer("backup", &ScheduleType::Cron("0 9 * * 1-5".into()), true).unwrap(),
            "\
# managed by shuvjobs
[Unit]
Description=backup (managed by shuvjobs)

[Timer]
OnCalendar=Mon..Fri *-*-* 09:00:00
Persistent=true

[Install]
WantedBy=timers.target
"
        );
    }

    #[test]
    fn a_calendar_expression_is_written_verbatim_and_aliases_lose_the_at_sign() {
        let rendered = render_timer(
            "backup",
            &ScheduleType::Calendar("*-*-* 03:00:00".into()),
            false,
        )
        .unwrap();
        assert!(
            rendered.contains("OnCalendar=*-*-* 03:00:00\nPersistent=false\n"),
            "{rendered}"
        );
        for (alias, expected) in [
            ("@hourly", "hourly"),
            ("@daily", "daily"),
            ("@weekly", "weekly"),
            ("@monthly", "monthly"),
            ("@yearly", "yearly"),
            ("@annually", "yearly"),
        ] {
            let rendered = render_timer("x", &ScheduleType::Calendar(alias.into()), true).unwrap();
            assert!(
                rendered.contains(&format!("OnCalendar={expected}\n")),
                "{alias}: {rendered}"
            );
        }
    }

    #[test]
    fn an_interval_becomes_a_monotonic_timer_with_no_persistent_line() {
        let rendered = render_timer(
            "poll",
            &ScheduleType::Interval(Duration::from_secs(900)),
            true,
        )
        .unwrap();
        assert_eq!(
            rendered,
            "\
# managed by shuvjobs
[Unit]
Description=poll (managed by shuvjobs)

[Timer]
OnBootSec=15min
OnUnitActiveSec=15min

[Install]
WantedBy=timers.target
"
        );
    }

    #[test]
    fn reboot_becomes_onbootsec_zero() {
        let rendered = render_timer("warm", &ScheduleType::Cron("@reboot".into()), true).unwrap();
        assert!(
            rendered.contains("[Timer]\nOnBootSec=0\n\n[Install]"),
            "{rendered}"
        );
        assert!(!rendered.contains("Persistent"), "{rendered}");
    }

    #[test]
    fn a_one_shot_is_an_absolute_utc_calendar_that_never_catches_up() {
        let when = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
            .unwrap()
            .with_timezone(&Utc);
        let rendered = render_timer("once", &ScheduleType::OneShot(when), true).unwrap();
        assert!(
            rendered.contains("OnCalendar=2026-03-04 05:06:07 UTC\nPersistent=false\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_service_wraps_the_command_in_sh_and_names_the_user_only_for_system_jobs() {
        assert_eq!(
            render_service("backup", " /usr/bin/backup --all ", None, None).unwrap(),
            "\
# managed by shuvjobs
[Unit]
Description=backup (managed by shuvjobs)

[Service]
Type=oneshot
ExecStart=/bin/sh -c \"/usr/bin/backup --all\"
"
        );
        let with_user =
            render_service("backup", "true", Some("nobody"), Some("Nightly backup")).unwrap();
        assert!(
            with_user.contains("Description=Nightly backup\n"),
            "{with_user}"
        );
        assert!(with_user.ends_with("User=nobody\n"), "{with_user}");
        // `root` is the default, so saying it would only be noise.
        let as_root = render_service("backup", "true", Some("root"), None).unwrap();
        assert!(!as_root.contains("User="), "{as_root}");
        assert!(render_service("backup", "  ", None, None).is_err());
    }

    #[test]
    fn exec_arguments_escape_what_systemd_would_otherwise_expand() {
        for (raw, escaped) in [
            (r"a\b", r"a\\b"),
            ("say \"hi\"", "say \\\"hi\\\""),
            ("date +%F", "date +%%F"),
            ("echo $HOME", "echo $$HOME"),
            (r#"$a"b\c%d"#, r#"$$a\"b\\c%%d"#),
            ("plain", "plain"),
        ] {
            assert_eq!(escape_exec_arg(raw), escaped, "escaping {raw:?}");
        }
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn a_user_timer_is_written_under_the_home_directory_unprivileged() {
        let host = user_host("daily", "backup");
        let outcome = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .unwrap();

        assert_eq!(outcome.id.as_deref(), Some("user/backup.timer"));
        assert!(outcome.applied);
        assert_eq!(
            host.calls(),
            vec![
                // Planned once, so it is checked while planning and
                // again when the plan is applied.
                call("systemd-analyze calendar 'daily'", Privilege::User),
                call(&format!("mkdir -p '{USER_DIR}'"), Privilege::User),
                call("systemd-analyze calendar 'daily'", Privilege::User),
                call("systemctl --user daemon-reload", Privilege::User),
                call(
                    "systemctl --user enable --now 'backup.timer'",
                    Privilege::User
                ),
                call(
                    "systemctl --user show 'backup.timer' \
                     --property=LoadState,LoadError --no-pager",
                    Privilege::User
                ),
            ]
        );
        let timer = host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .expect("timer written");
        assert!(timer.starts_with(MANAGED_MARKER), "{timer}");
        assert!(timer.contains("OnCalendar=daily\n"), "{timer}");
        assert_eq!(
            host.file_mode(&format!("{USER_DIR}/backup.timer")),
            Some(0o644)
        );
        let service = host
            .file_contents(&format!("{USER_DIR}/backup.service"))
            .expect("service written");
        assert!(
            service.contains("ExecStart=/bin/sh -c \"/usr/bin/backup\"\n"),
            "{service}"
        );
        assert_eq!(
            host.file_mode(&format!("{USER_DIR}/backup.service")),
            Some(0o644)
        );
    }

    #[test]
    fn a_disabled_job_is_written_but_never_enabled() {
        let host = user_host("daily", "backup");
        let mut spec = calendar_spec("backup", "@daily", "/usr/bin/backup");
        spec.enabled = false;
        writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert!(
            !commands(&host)
                .iter()
                .any(|cmd| cmd.contains("enable --now")),
            "{:?}",
            commands(&host)
        );
    }

    #[test]
    fn a_system_timer_is_root_everywhere() {
        let host = Arc::new(
            FakeHost::new()
                .with_policy(sudoer())
                .root_only("/etc/")
                .script("systemd-analyze calendar '*-*-* 03:00:00'", 0, "", "")
                .script("systemctl daemon-reload", 0, "", "")
                .script("systemctl enable --now 'backup.timer'", 0, "", "")
                .script(
                    "systemctl show 'backup.timer' --property=LoadState,LoadError --no-pager",
                    0,
                    "LoadState=loaded\n",
                    "",
                ),
        );
        let mut spec = calendar_spec("backup", "*-*-* 03:00:00", "/usr/bin/backup");
        spec.scope = JobScope::System;
        spec.user = Some("nobody".to_string());
        let outcome = writer(&host).create(&spec, ApplyMode::Apply).unwrap();

        assert_eq!(outcome.id.as_deref(), Some("backup.timer"));
        assert!(
            host.calls().iter().all(|c| c.privilege == Privilege::Root),
            "{:?}",
            host.calls()
        );
        // No mkdir: /etc/systemd/system is always there.
        assert_eq!(
            commands(&host),
            vec![
                "systemd-analyze calendar '*-*-* 03:00:00'".to_string(),
                "systemd-analyze calendar '*-*-* 03:00:00'".to_string(),
                "systemctl daemon-reload".to_string(),
                "systemctl enable --now 'backup.timer'".to_string(),
                "systemctl show 'backup.timer' --property=LoadState,LoadError --no-pager"
                    .to_string(),
            ]
        );
        let service = host
            .file_contents("/etc/systemd/system/backup.service")
            .expect("service written");
        assert!(service.ends_with("User=nobody\n"), "{service}");
        assert!(host
            .file_contents("/etc/systemd/system/backup.timer")
            .is_some());
    }

    #[test]
    fn a_system_timer_without_sudo_needs_root() {
        let host = Arc::new(FakeHost::new().root_only("/etc/"));
        let mut spec = calendar_spec("backup", "*-*-* 03:00:00", "/usr/bin/backup");
        spec.scope = JobScope::System;
        let err = writer(&host)
            .create(&spec, ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::NeedsRoot { .. }), "got {err:?}");
        assert!(host
            .file_contents("/etc/systemd/system/backup.timer")
            .is_none());
    }

    #[test]
    fn a_user_scope_job_may_not_ask_to_run_as_somebody_else() {
        let host = user_host("daily", "backup");
        let mut spec = calendar_spec("backup", "@daily", "/usr/bin/backup");
        spec.user = Some("nobody".to_string());
        let err = writer(&host)
            .create(&spec, ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    #[test]
    fn a_job_needs_a_name_and_the_location_must_be_one_we_manage() {
        let host = user_host("daily", "backup");
        let mut spec = calendar_spec("backup", "@daily", "true");
        spec.name = None;
        assert!(matches!(
            writer(&host).create(&spec, ApplyMode::DryRun),
            Err(Error::Validation(_))
        ));

        let mut spec = calendar_spec("backup", "@daily", "true");
        spec.location = Some("/usr/lib/systemd/system".to_string());
        assert!(matches!(
            writer(&host).create(&spec, ApplyMode::DryRun),
            Err(Error::Validation(_))
        ));

        let mut spec = calendar_spec("backup", "@daily", "true");
        spec.location = Some(format!("{USER_DIR}/"));
        let outcome = writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert_eq!(outcome.id.as_deref(), Some("user/backup.timer"));
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_some());
    }

    #[test]
    fn an_existing_unit_we_did_not_write_is_a_conflict() {
        let host = Arc::new(user_host_parts("daily", "backup").file(
            &format!("{USER_DIR}/backup.timer"),
            "[Timer]\nOnCalendar=daily\n",
        ));
        let err = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .expect_err("must refuse");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert!(host.calls().is_empty());
    }

    #[test]
    fn a_unit_we_wrote_before_is_overwritten_with_its_old_text_captured() {
        let old = format!("{MANAGED_MARKER}\n[Timer]\nOnCalendar=weekly\n");
        let host = Arc::new(
            user_host_parts("daily", "backup")
                .file(&format!("{USER_DIR}/backup.timer"), &old)
                .file(
                    &format!("{USER_DIR}/backup.service"),
                    &format!("{MANAGED_MARKER}\n[Service]\n"),
                ),
        );
        let outcome = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .unwrap();
        let backups = outcome.backup_targets();
        assert_eq!(backups.len(), 2, "{backups:?}");
        assert_eq!(
            backups[0],
            (format!("{USER_DIR}/backup.timer").as_str(), old.as_str())
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .unwrap()
            .contains("OnCalendar=daily"));
    }

    #[test]
    fn a_missing_systemd_analyze_is_noted_rather_than_fatal() {
        let host = Arc::new(
            FakeHost::new()
                .script(&format!("mkdir -p '{USER_DIR}'"), 0, "", "")
                .script("systemd-analyze calendar 'daily'", 127, "", "not found")
                .script("systemctl --user daemon-reload", 0, "", "")
                .script("systemctl --user enable --now 'backup.timer'", 0, "", "")
                .script(
                    "systemctl --user show 'backup.timer' \
                     --property=LoadState,LoadError --no-pager",
                    0,
                    "LoadState=loaded\n",
                    "",
                ),
        );
        let outcome = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .unwrap();
        assert_eq!(
            outcome.notes,
            vec!["systemd-analyze unavailable; expression not pre-validated".to_string()]
        );
        // The check is dropped from the plan, so it is not run twice.
        assert_eq!(
            commands(&host)
                .iter()
                .filter(|cmd| cmd.starts_with("systemd-analyze"))
                .count(),
            1
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_some());
    }

    #[test]
    fn a_rejected_calendar_expression_stops_before_anything_is_written() {
        let host = Arc::new(FakeHost::new().script(
            "systemd-analyze calendar 'nonsense'",
            1,
            "",
            "Failed to parse calendar expression: Invalid argument",
        ));
        let err = writer(&host)
            .create(
                &calendar_spec("backup", "nonsense", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .expect_err("must refuse");
        match err {
            Error::Validation(message) => {
                assert!(message.contains("Invalid argument"), "{message}")
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_none());
    }

    #[test]
    fn a_unit_that_does_not_load_is_taken_back_out() {
        let host = Arc::new(
            FakeHost::new()
                .script(&format!("mkdir -p '{USER_DIR}'"), 0, "", "")
                .script("systemd-analyze calendar 'daily'", 0, "", "")
                .script("systemctl --user daemon-reload", 0, "", "")
                .script("systemctl --user enable --now 'backup.timer'", 0, "", "")
                .script(
                    "systemctl --user show 'backup.timer' \
                     --property=LoadState,LoadError --no-pager",
                    0,
                    "LoadState=error\nLoadError=org.freedesktop.systemd1.UnitFileBad \
                     \"Invalid argument\"\n",
                    "",
                ),
        );
        let err = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .expect_err("must refuse");
        match err {
            Error::Validation(message) => assert!(message.contains("UnitFileBad"), "{message}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_none());
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.service"))
            .is_none());
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("systemctl --user daemon-reload")
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_and_lists_the_plan() {
        let host = user_host("daily", "backup");
        let outcome = writer(&host)
            .create(
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::DryRun,
            )
            .unwrap();
        assert!(!outcome.applied);
        assert_eq!(
            outcome
                .changes
                .iter()
                .map(Change::summary)
                .collect::<Vec<_>>(),
            vec![
                format!("run: create {USER_DIR}"),
                "run: check the calendar expression `daily`".to_string(),
                format!("write {USER_DIR}/backup.timer (644)"),
                format!("write {USER_DIR}/backup.service (644)"),
                "run: reload the systemd manager".to_string(),
                "run: enable and start backup.timer".to_string(),
            ]
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_none());
        // Planning reads; it never enables or reloads.
        assert_eq!(commands(&host), vec!["systemd-analyze calendar 'daily'"]);
    }

    // -- update ----------------------------------------------------------

    const SHOW_PROPS: &str = "Id,FragmentPath,UnitFileState,ActiveState,Triggers";

    fn show_cmd(unit: &str) -> String {
        format!("systemctl --user show '{unit}' --property={SHOW_PROPS} --no-pager")
    }

    /// A user timer that already exists, with `service_body` on disk for
    /// the service it triggers.
    fn existing_host(timer_state: &str, service_body: &str) -> FakeHost {
        FakeHost::new()
            .file(
                &format!("{USER_DIR}/backup.timer"),
                &format!("{MANAGED_MARKER}\n[Timer]\nOnCalendar=weekly\n"),
            )
            .file(&format!("{USER_DIR}/backup.service"), service_body)
            .script(
                &show_cmd("backup.timer"),
                0,
                &format!(
                    "Id=backup.timer\nFragmentPath={USER_DIR}/backup.timer\n\
                     UnitFileState={timer_state}\nActiveState=active\nTriggers=backup.service\n"
                ),
                "",
            )
            .script(
                &show_cmd("backup.service"),
                0,
                &format!(
                    "Id=backup.service\nFragmentPath={USER_DIR}/backup.service\n\
                     UnitFileState=static\nActiveState=inactive\nTriggers=\n"
                ),
                "",
            )
            .script("systemctl --user daemon-reload", 0, "", "")
            .script("systemctl --user enable --now 'backup.timer'", 0, "", "")
            .script("systemctl --user disable --now 'backup.timer'", 0, "", "")
            .script("systemctl --user try-restart 'backup.timer'", 0, "", "")
            .script("systemctl --user reset-failed 'backup.timer'", 0, "", "")
    }

    fn managed_service() -> String {
        render_service("backup", "/usr/bin/backup", None, None).unwrap()
    }

    #[test]
    fn updating_a_managed_pair_rewrites_both_files_and_restarts_the_timer() {
        let host = Arc::new(existing_host("enabled", &managed_service()));
        let outcome = writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("backup", "@daily", "/usr/bin/backup --all"),
                ApplyMode::Apply,
            )
            .unwrap();

        assert_eq!(outcome.id.as_deref(), Some("user/backup.timer"));
        assert_eq!(
            commands(&host),
            vec![
                show_cmd("backup.timer"),
                show_cmd("backup.service"),
                "systemctl --user daemon-reload".to_string(),
                "systemctl --user enable --now 'backup.timer'".to_string(),
                "systemctl --user try-restart 'backup.timer'".to_string(),
            ]
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .unwrap()
            .contains("OnCalendar=daily"));
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.service"))
            .unwrap()
            .contains("ExecStart=/bin/sh -c \"/usr/bin/backup --all\""));
        // The old timer text is kept so it can be backed up.
        assert!(outcome
            .backup_targets()
            .iter()
            .any(|(path, before)| path.ends_with("backup.timer") && before.contains("weekly")));
    }

    #[test]
    fn updating_a_disabled_job_disables_it_rather_than_enabling_it() {
        let host = Arc::new(existing_host("disabled", &managed_service()));
        let mut spec = calendar_spec("backup", "@daily", "/usr/bin/backup");
        spec.enabled = false;
        writer(&host)
            .update("user/backup.timer", &spec, ApplyMode::Apply)
            .unwrap();
        assert!(
            commands(&host).contains(&"systemctl --user disable --now 'backup.timer'".to_string())
        );
    }

    #[test]
    fn updating_refuses_a_vendor_unit() {
        let host = Arc::new(FakeHost::new().script(
            &show_cmd("backup.timer"),
            0,
            "Id=backup.timer\nFragmentPath=/usr/lib/systemd/user/backup.timer\n\
             UnitFileState=enabled\nActiveState=active\nTriggers=backup.service\n",
            "",
        ));
        let err = writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("backup", "@daily", "true"),
                ApplyMode::DryRun,
            )
            .expect_err("must refuse");
        match err {
            Error::Unsupported { reason, .. } => {
                assert!(reason.contains("vendor unit"), "{reason}")
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn updating_refuses_a_timer_that_is_not_installed() {
        let host = Arc::new(FakeHost::new().script(
            &show_cmd("backup.timer"),
            0,
            "Id=backup.timer\nFragmentPath=\nUnitFileState=\nActiveState=inactive\n",
            "",
        ));
        let err = writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("backup", "@daily", "true"),
                ApplyMode::DryRun,
            )
            .expect_err("must refuse");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn an_unmanaged_service_survives_an_update_that_keeps_its_command() {
        let hand_written = "[Unit]\nDescription=mine\n\n[Service]\nExecStart=/usr/bin/backup\n";
        let host = Arc::new(existing_host("enabled", hand_written).script(
            "systemctl --user show 'backup.service' --property=ExecStart --no-pager",
            0,
            "ExecStart={ path=/bin/sh ; argv[]=/bin/sh -c /usr/bin/backup ; ignore_errors=no }\n",
            "",
        ));
        writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("backup", "@daily", "/usr/bin/backup"),
                ApplyMode::Apply,
            )
            .unwrap();
        assert_eq!(
            host.file_contents(&format!("{USER_DIR}/backup.service"))
                .as_deref(),
            Some(hand_written)
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .unwrap()
            .contains("OnCalendar=daily"));
    }

    #[test]
    fn an_unmanaged_service_blocks_an_update_that_changes_the_command() {
        let hand_written = "[Service]\nExecStart=/usr/bin/backup\n";
        let host = Arc::new(existing_host("enabled", hand_written).script(
            "systemctl --user show 'backup.service' --property=ExecStart --no-pager",
            0,
            "ExecStart={ path=/bin/sh ; argv[]=/bin/sh -c /usr/bin/backup ; ignore_errors=no }\n",
            "",
        ));
        let err = writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("backup", "@daily", "/usr/bin/something-else"),
                ApplyMode::Apply,
            )
            .expect_err("must refuse");
        match err {
            Error::Unsupported { reason, .. } => {
                assert!(reason.contains("edit ExecStart manually"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .unwrap()
            .contains("weekly"));
    }

    #[test]
    fn renaming_is_refused_for_now() {
        let host = Arc::new(existing_host("enabled", &managed_service()));
        let err = writer(&host)
            .update(
                "user/backup.timer",
                &calendar_spec("archive", "@daily", "/usr/bin/backup"),
                ApplyMode::DryRun,
            )
            .expect_err("must refuse");
        assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
    }

    // -- delete ----------------------------------------------------------

    #[test]
    fn deleting_removes_both_files_when_we_wrote_the_service() {
        let host = Arc::new(existing_host("enabled", &managed_service()));
        let outcome = writer(&host)
            .delete("user/backup.timer", ApplyMode::Apply)
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("user/backup.timer"));
        assert_eq!(
            commands(&host),
            vec![
                show_cmd("backup.timer"),
                show_cmd("backup.service"),
                "systemctl --user disable --now 'backup.timer'".to_string(),
                "systemctl --user daemon-reload".to_string(),
                "systemctl --user reset-failed 'backup.timer'".to_string(),
            ]
        );
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_none());
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.service"))
            .is_none());
    }

    #[test]
    fn deleting_leaves_a_service_we_did_not_write() {
        let hand_written = "[Service]\nExecStart=/usr/bin/backup\n";
        let host = Arc::new(existing_host("enabled", hand_written));
        writer(&host)
            .delete("user/backup.timer", ApplyMode::Apply)
            .unwrap();
        assert!(host
            .file_contents(&format!("{USER_DIR}/backup.timer"))
            .is_none());
        assert_eq!(
            host.file_contents(&format!("{USER_DIR}/backup.service"))
                .as_deref(),
            Some(hand_written)
        );
    }

    #[test]
    fn deleting_refuses_a_vendor_unit_and_says_what_to_do_instead() {
        let host = Arc::new(FakeHost::new().script(
            &show_cmd("backup.timer"),
            0,
            "Id=backup.timer\nFragmentPath=/usr/lib/systemd/user/backup.timer\nTriggers=backup.service\n",
            "",
        ));
        let err = writer(&host)
            .delete("user/backup.timer", ApplyMode::DryRun)
            .expect_err("must refuse");
        match err {
            Error::Unsupported { reason, .. } => {
                assert!(reason.contains("systemctl mask"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // -- set_enabled -----------------------------------------------------

    fn toggling_host(state: &str) -> Arc<FakeHost> {
        Arc::new(
            FakeHost::new()
                .script(
                    &show_cmd("backup.timer"),
                    0,
                    &format!(
                        "Id=backup.timer\nFragmentPath=/usr/lib/systemd/user/backup.timer\n\
                         UnitFileState={state}\nActiveState=active\nTriggers=backup.service\n"
                    ),
                    "",
                )
                .script("systemctl --user enable --now 'backup.timer'", 0, "", "")
                .script("systemctl --user disable --now 'backup.timer'", 0, "", "")
                .script("systemctl --user stop 'backup.timer'", 0, "", ""),
        )
    }

    #[test]
    fn toggling_works_on_a_vendor_unit_too() {
        let host = toggling_host("enabled");
        let outcome = writer(&host)
            .set_enabled("user/backup.timer", false, ApplyMode::Apply)
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some("user/backup.timer"));
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("systemctl --user disable --now 'backup.timer'")
        );

        let host = toggling_host("disabled");
        writer(&host)
            .set_enabled("user/backup.timer", true, ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("systemctl --user enable --now 'backup.timer'")
        );
    }

    #[test]
    fn a_static_timer_can_be_stopped_but_not_enabled() {
        let host = toggling_host("static");
        writer(&host)
            .set_enabled("user/backup.timer", false, ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("systemctl --user stop 'backup.timer'")
        );

        let host = toggling_host("static");
        let err = writer(&host)
            .set_enabled("user/backup.timer", true, ApplyMode::Apply)
            .expect_err("must refuse");
        match err {
            Error::Unsupported { reason, .. } => assert!(reason.contains("static"), "{reason}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_masked_timer_is_refused_either_way() {
        for state in ["masked", "masked-runtime"] {
            for enabled in [true, false] {
                let host = toggling_host(state);
                let err = writer(&host)
                    .set_enabled("user/backup.timer", enabled, ApplyMode::Apply)
                    .expect_err("must refuse");
                match err {
                    Error::Unsupported { reason, .. } => {
                        assert!(reason.contains("masked"), "{state}: {reason}");
                    }
                    other => panic!("{state}: expected Unsupported, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_missing_user_manager_is_reported_as_a_precondition() {
        let host = Arc::new(FakeHost::new().script(
            &show_cmd("backup.timer"),
            1,
            "",
            "Failed to connect to bus: No medium found",
        ));
        let err = writer(&host)
            .set_enabled("user/backup.timer", true, ApplyMode::Apply)
            .expect_err("must refuse");
        match err {
            Error::Unsupported { reason, .. } => {
                assert_eq!(
                    reason,
                    "user manager unreachable (enable lingering / set XDG_RUNTIME_DIR)"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_bus_failure_while_applying_is_mapped_too() {
        let host = Arc::new(
            FakeHost::new()
                .script(
                    &show_cmd("backup.timer"),
                    0,
                    &format!(
                        "Id=backup.timer\nFragmentPath={USER_DIR}/backup.timer\n\
                         UnitFileState=enabled\nActiveState=active\n"
                    ),
                    "",
                )
                .script(
                    "systemctl --user enable --now 'backup.timer'",
                    1,
                    "",
                    "Failed to connect to bus: No medium found",
                ),
        );
        let err = writer(&host)
            .set_enabled("user/backup.timer", true, ApplyMode::Apply)
            .expect_err("must refuse");
        assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
    }
}
