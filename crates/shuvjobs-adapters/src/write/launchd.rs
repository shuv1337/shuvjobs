//! The launchd writer.
//!
//! A launchd job is one plist plus a *domain*: the file says what to
//! run, and `launchctl bootstrap <domain> <path>` is what makes it real.
//! Get the domain wrong and the file is inert, so the scope picks all
//! three things at once — the directory, the domain, and the privilege
//! — and nothing else is negotiable:
//!
//! | scope  | directory                      | domain      |
//! |--------|--------------------------------|-------------|
//! | user   | `~/Library/LaunchAgents`       | `gui/<uid>` |
//! | system | `/Library/LaunchDaemons`       | `system`    |
//! | admin  | `/Library/LaunchAgents`        | `gui/<uid>` |
//!
//! The third row is why the file privilege and the command privilege are
//! tracked separately: an agent an administrator dropped in
//! `/Library/LaunchAgents` is a root-owned file that still loads into
//! *your* GUI domain.
//!
//! Everything under `/System` is refused outright. It is signed and
//! sealed by SIP, so a write there cannot succeed, and the honest answer
//! is `launchctl disable`.
//!
//! Plists we wrote carry `ShuvjobsManaged`. Without it, an update keeps
//! every key the operator had and only replaces the ones it must, so
//! `KeepAlive`, `StandardOutPath` and the rest survive an edit.

use std::sync::Arc;

use plist::{Dictionary, Value};
use shuvjobs_core::host::shell::{mkdir_script, shell_quote};
use shuvjobs_core::host::{Host, HostOs, Privilege};
use shuvjobs_core::manage::{
    execute, ApplyMode, Change, FailPolicy, JobScope, JobSpec, JobWriter, MutationOutcome,
};
use shuvjobs_core::{Error, Result, ScheduleType, TaskSourceKind};

use crate::ids::parse_launchd_id;
use crate::launchd::LaunchdAdapter;
use crate::write::calendar::{cron_to_calendar_interval, parse_formatted_calendar_interval};
use crate::write::{validate_command, validate_simple_name, NameKind};

/// Agents an administrator installs for every user.
pub const SYSTEM_AGENT_DIR: &str = "/Library/LaunchAgents";
/// Daemons: one instance per machine, running as root unless told otherwise.
pub const SYSTEM_DAEMON_DIR: &str = "/Library/LaunchDaemons";

/// The key that marks a plist as ours to regenerate.
pub const MANAGED_KEY: &str = "ShuvjobsManaged";

/// `launchctl bootout` exits 3 when there was nothing to unload, which
/// is exactly the state we want it in.
const NOT_LOADED_EXIT: i32 = 3;

/// Option key for `RunAtLoad`.
const RUN_AT_LOAD_OPTION: &str = "launchd.run_at_load";

/// The two schedule keys. Exactly one of them belongs in a plist we
/// write, so an update that switches shape has to remove the other.
const SCHEDULE_KEYS: [&str; 2] = ["StartInterval", "StartCalendarInterval"];

fn unsupported(id: &str, reason: &str) -> Error {
    Error::Unsupported {
        source_kind: TaskSourceKind::Launchd,
        id: id.to_string(),
        reason: reason.to_string(),
    }
}

/// `launchctl` failures that are really "this domain cannot do that":
/// bootstrapping a GUI agent over SSH with no `Aqua` session, most
/// often. Reported as unsupported with launchctl's own words, because
/// no amount of retrying will change the answer.
fn map_launchctl_error(err: Error, id: &str) -> Error {
    let text = err.to_string();
    if text.contains("Bootstrap failed: 125") || text.contains("Domain does not support") {
        return unsupported(
            id,
            &format!(
                "launchctl refused the domain (log in on the console, or use a daemon): {text}"
            ),
        );
    }
    err
}

fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Where one job's plist lives, and how to talk to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    path: String,
    dir: String,
    /// `gui/501` or `system`.
    domain: String,
    /// What writing the *file* costs.
    file_privilege: Privilege,
    /// What talking to the *domain* costs.
    command_privilege: Privilege,
}

impl Target {
    /// `gui/501/com.example.job`, the service target `launchctl` wants
    /// for everything except `bootstrap`.
    fn service(&self, label: &str) -> String {
        format!("{}/{label}", self.domain)
    }
}

/// Turn a schedule into the one key launchd understands for it.
fn schedule_entry(schedule: &ScheduleType, label: &str) -> Result<(&'static str, Value)> {
    match schedule {
        ScheduleType::Interval(interval) => {
            let secs = interval.as_secs();
            if secs < 1 {
                return Err(Error::Validation(
                    "StartInterval must be at least one second".to_string(),
                ));
            }
            let secs = i64::try_from(secs).map_err(|_| {
                Error::Validation("StartInterval is larger than launchd can hold".to_string())
            })?;
            Ok(("StartInterval", Value::Integer(secs.into())))
        }
        ScheduleType::Cron(expr) => Ok(("StartCalendarInterval", cron_to_calendar_interval(expr)?)),
        ScheduleType::Calendar(text) => Ok((
            "StartCalendarInterval",
            parse_formatted_calendar_interval(text)?,
        )),
        // launchd has no "run once at this instant": the nearest thing
        // is a calendar entry that also fires every year.
        ScheduleType::OneShot(_) => Err(unsupported(
            label,
            "launchd has no one-shot schedule; use `at` for a job that runs once",
        )),
    }
}

/// The plist body for a job, in launchd's conventional key order.
///
/// `user` is only ever `Some` for a daemon: `UserName` in a LaunchAgent
/// is ignored at best and a load failure at worst.
pub fn render_plist(
    label: &str,
    command: &str,
    schedule: &ScheduleType,
    user: Option<&str>,
    run_at_load: bool,
) -> Result<String> {
    validate_command(command)?;
    let mut dict = Dictionary::new();
    dict.insert("Label".to_string(), Value::String(label.to_string()));
    dict.insert(
        "ProgramArguments".to_string(),
        Value::Array(vec![
            Value::String("/bin/sh".to_string()),
            Value::String("-c".to_string()),
            Value::String(command.trim().to_string()),
        ]),
    );
    let (key, value) = schedule_entry(schedule, label)?;
    dict.insert(key.to_string(), value);
    if let Some(user) = user {
        dict.insert("UserName".to_string(), Value::String(user.to_string()));
    }
    dict.insert("RunAtLoad".to_string(), Value::Boolean(run_at_load));
    dict.insert(MANAGED_KEY.to_string(), Value::Boolean(true));
    render_dictionary(&dict)
}

fn render_dictionary(dict: &Dictionary) -> Result<String> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &Value::Dictionary(dict.clone()))
        .map_err(|err| Error::Other(format!("could not render the plist: {err}")))?;
    let mut text = String::from_utf8(bytes)
        .map_err(|err| Error::Other(format!("plist is not valid UTF-8: {err}")))?;
    // `to_writer_xml` stops at the closing tag; every plist on disk ends
    // with a newline.
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn is_managed(dict: &Dictionary) -> bool {
    dict.get(MANAGED_KEY).and_then(Value::as_boolean) == Some(true)
}

/// Creates, edits, and removes launchd jobs on one [`Host`].
pub struct LaunchdWriter {
    host: Arc<dyn Host>,
}

impl LaunchdWriter {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }

    /// launchd exists only on macOS, and pretending otherwise would let
    /// a Linux host accumulate plists nothing will ever read.
    fn require_macos(&self, id: &str) -> Result<()> {
        if self.host.os()? == HostOs::MacOs {
            return Ok(());
        }
        Err(unsupported(id, "launchd is only available on macOS"))
    }

    fn user_agent_dir(&self) -> Result<String> {
        Ok(format!(
            "{}/Library/LaunchAgents",
            self.host.home_dir()?.trim_end_matches('/')
        ))
    }

    /// The three directories whose plists are ours to write, in the
    /// order a lookup should prefer them: your own agents first, then
    /// the machine's agents, then its daemons.
    fn writable_dirs(&self) -> Result<Vec<String>> {
        Ok(vec![
            self.user_agent_dir()?,
            SYSTEM_AGENT_DIR.to_string(),
            SYSTEM_DAEMON_DIR.to_string(),
        ])
    }

    fn target_for_dir(&self, dir: &str, label: &str) -> Result<Target> {
        let domain = if dir == SYSTEM_DAEMON_DIR {
            "system".to_string()
        } else {
            format!("gui/{}", self.host.current_uid()?)
        };
        // Anything under /Library is root's; your own home is yours.
        let file_privilege = if dir.starts_with("/Library/") {
            Privilege::Root
        } else {
            Privilege::User
        };
        let command_privilege = if domain == "system" {
            Privilege::Root
        } else {
            Privilege::User
        };
        Ok(Target {
            path: format!("{dir}/{label}.plist"),
            dir: dir.to_string(),
            domain,
            file_privilege,
            command_privilege,
        })
    }

    /// The directory an explicit `location` names, checked against the
    /// ones we are willing to write.
    fn dir_from_location(&self, location: &str, label: &str) -> Result<String> {
        let trimmed = location.trim_end_matches('/');
        let dir = if trimmed.ends_with(".plist") {
            parent_dir(trimmed).to_string()
        } else {
            trimmed.to_string()
        };
        if dir.starts_with("/System/") || dir == "/System" {
            return Err(unsupported(
                label,
                "/System is sealed by SIP; use `launchctl disable` instead of editing it",
            ));
        }
        let dirs = self.writable_dirs()?;
        if !dirs.contains(&dir) {
            return Err(unsupported(
                label,
                &format!(
                    "`{dir}` is not a launchd directory shuvjobs writes: expected one of {}",
                    dirs.join(", ")
                ),
            ));
        }
        Ok(dir)
    }

    /// Where a *new* job goes.
    fn create_target(&self, spec: &JobSpec, label: &str) -> Result<Target> {
        let dir = match spec.location.as_deref() {
            Some(location) => self.dir_from_location(location, label)?,
            None => match spec.scope {
                JobScope::User => self.user_agent_dir()?,
                JobScope::System => SYSTEM_DAEMON_DIR.to_string(),
            },
        };
        self.target_for_dir(&dir, label)
    }

    /// Where an *existing* job lives: the hint if there is one, else the
    /// first writable directory that has the plist.
    fn locate(&self, label: &str, location: Option<&str>) -> Result<Target> {
        if let Some(location) = location {
            let dir = self.dir_from_location(location, label)?;
            return self.target_for_dir(&dir, label);
        }
        for dir in self.writable_dirs()? {
            let target = self.target_for_dir(&dir, label)?;
            if self.host.exists(&target.path, target.file_privilege)? {
                return Ok(target);
            }
        }
        Err(Error::Conflict(format!(
            "{label}: no plist found in {}",
            self.writable_dirs()?.join(", ")
        )))
    }

    /// The plist at `path`, parsed. `Ok(None)` when there is no file.
    fn read_plist(&self, path: &str, privilege: Privilege) -> Result<Option<Dictionary>> {
        let Some(bytes) = self.host.read_file(path, privilege)? else {
            return Ok(None);
        };
        let value: Value = plist::from_bytes(&bytes).map_err(|err| Error::Parse {
            kind: "launchd plist".to_string(),
            message: format!("{path}: {err}"),
        })?;
        match value {
            Value::Dictionary(dict) => Ok(Some(dict)),
            _ => Err(Error::Parse {
                kind: "launchd plist".to_string(),
                message: format!("{path}: top level is not a dictionary"),
            }),
        }
    }

    /// An existing plist we may overwrite, or `None` when the path is
    /// free. A plist without our key belongs to somebody else.
    fn read_managed(&self, target: &Target, label: &str) -> Result<Option<Dictionary>> {
        let Some(dict) = self.read_plist(&target.path, target.file_privilege)? else {
            return Ok(None);
        };
        if !is_managed(&dict) {
            return Err(Error::Conflict(format!(
                "{label}: {} already exists and was not written by shuvjobs; \
                 move it aside or pick another label",
                target.path
            )));
        }
        Ok(Some(dict))
    }

    /// Labels launchd currently knows about in the caller's own domain.
    ///
    /// A failure here is not fatal: `launchctl list` is a courtesy check
    /// against clobbering a running job, and a host that cannot answer
    /// simply does not get it.
    fn loaded_labels(&self) -> Vec<String> {
        let Ok(output) = self.host.run("launchctl list", None, Privilege::User) else {
            return Vec::new();
        };
        if !output.success() {
            return Vec::new();
        }
        LaunchdAdapter::parse_launchctl_list(&output.stdout_str())
            .into_iter()
            .map(|entry| entry.label)
            .collect()
    }

    fn bootout_change(target: &Target, label: &str) -> Change {
        Change::Command {
            cmd: format!("launchctl bootout {}", shell_quote(&target.service(label))),
            stdin: None,
            privilege: target.command_privilege,
            description: format!("unload {label}"),
            // Exit 3 is "no such service": already in the state we want.
            on_fail: FailPolicy::AllowExit(vec![NOT_LOADED_EXIT]),
        }
    }

    fn enable_change(target: &Target, label: &str) -> Change {
        Change::Command {
            cmd: format!("launchctl enable {}", shell_quote(&target.service(label))),
            stdin: None,
            privilege: target.command_privilege,
            description: format!("clear any disabled override for {label}"),
            // A label that was never disabled has no override to clear.
            on_fail: FailPolicy::Ignore,
        }
    }

    fn bootstrap_change(target: &Target, label: &str) -> Change {
        Change::Command {
            cmd: format!(
                "launchctl bootstrap {} {}",
                shell_quote(&target.domain),
                shell_quote(&target.path)
            ),
            stdin: None,
            privilege: target.command_privilege,
            description: format!("load {label} into {}", target.domain),
            on_fail: FailPolicy::Error,
        }
    }

    fn disable_change(target: &Target, label: &str) -> Change {
        Change::Command {
            cmd: format!("launchctl disable {}", shell_quote(&target.service(label))),
            stdin: None,
            privilege: target.command_privilege,
            description: format!("disable {label}"),
            on_fail: FailPolicy::Error,
        }
    }

    /// `UserName`, which only means anything for a daemon.
    fn plist_user<'a>(
        target: &Target,
        spec: &'a JobSpec,
        notes: &mut Vec<String>,
    ) -> Option<&'a str> {
        let user = spec.user.as_deref()?;
        if target.dir == SYSTEM_DAEMON_DIR {
            return Some(user);
        }
        notes.push(format!(
            "UserName={user} was dropped: a LaunchAgent always runs as the logged-in user"
        ));
        None
    }

    fn run_at_load(spec: &JobSpec) -> bool {
        spec.options
            .get(RUN_AT_LOAD_OPTION)
            .is_some_and(|value| value == "true")
    }

    /// Rewrite an unmanaged plist in place: keep every key the operator
    /// had, replace only the ones this edit is about, and make sure the
    /// schedule shape we did *not* choose is gone.
    fn merge_into(
        dict: &mut Dictionary,
        label: &str,
        spec: &JobSpec,
        user: Option<&str>,
    ) -> Result<()> {
        validate_command(&spec.command)?;
        dict.insert("Label".to_string(), Value::String(label.to_string()));
        dict.insert(
            "ProgramArguments".to_string(),
            Value::Array(vec![
                Value::String("/bin/sh".to_string()),
                Value::String("-c".to_string()),
                Value::String(spec.command.trim().to_string()),
            ]),
        );
        let (key, value) = schedule_entry(&spec.schedule, label)?;
        dict.insert(key.to_string(), value);
        for other in SCHEDULE_KEYS.iter().filter(|other| **other != key) {
            dict.remove(other);
        }
        match user {
            Some(user) => {
                dict.insert("UserName".to_string(), Value::String(user.to_string()));
            }
            None => {
                dict.remove("UserName");
            }
        }
        Ok(())
    }

    /// A note when the label breaks the reverse-DNS convention. Not an
    /// error: launchd accepts it, and shadowing an Apple label is the
    /// only real hazard.
    fn label_note(label: &str, notes: &mut Vec<String>) {
        if !label.contains('.') {
            notes.push(format!(
                "`{label}` has no dot; launchd labels are conventionally reverse-DNS, \
                 for example com.example.{label}"
            ));
        }
    }
}

impl JobWriter for LaunchdWriter {
    fn kind(&self) -> TaskSourceKind {
        TaskSourceKind::Launchd
    }

    fn create(&self, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        let label = spec.name.as_deref().ok_or_else(|| {
            Error::Validation(
                "a launchd job needs a name: it becomes the label and the plist file name".into(),
            )
        })?;
        validate_simple_name(label, NameKind::Launchd)?;
        self.require_macos(label)?;

        let target = self.create_target(spec, label)?;
        let existing = self.read_managed(&target, label)?;
        let listed = self.loaded_labels().iter().any(|other| other == label);
        if listed && existing.is_none() {
            return Err(Error::Conflict(format!(
                "{label}: already loaded in launchd; pick another label or remove that job first"
            )));
        }

        let mut notes = Vec::new();
        Self::label_note(label, &mut notes);
        let user = Self::plist_user(&target, spec, &mut notes);
        let after = render_plist(
            label,
            &spec.command,
            &spec.schedule,
            user,
            Self::run_at_load(spec),
        )?;
        let before = existing.as_ref().map(render_dictionary).transpose()?;

        let mut changes = Vec::new();
        // Replacing a plist that is already loaded means unloading the
        // old definition first; bootstrap would otherwise refuse.
        if listed {
            changes.push(Self::bootout_change(&target, label));
        }
        if target.file_privilege == Privilege::User {
            changes.push(Change::Command {
                cmd: mkdir_script(&target.dir),
                stdin: None,
                privilege: Privilege::User,
                description: format!("create {}", target.dir),
                on_fail: FailPolicy::Error,
            });
        }
        changes.push(Change::WriteFile {
            path: target.path.clone(),
            before,
            after,
            mode: 0o644,
            privilege: target.file_privilege,
        });
        if spec.enabled {
            changes.push(Self::enable_change(&target, label));
            changes.push(Self::bootstrap_change(&target, label));
        }

        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_launchctl_error(err, label))?;
        outcome.notes.extend(notes);
        outcome.id = Some(label.to_string());
        Ok(outcome)
    }

    fn update(&self, id: &str, spec: &JobSpec, mode: ApplyMode) -> Result<MutationOutcome> {
        spec.validate()?;
        let label = parse_launchd_id(id)?;
        self.require_macos(&label)?;
        if let Some(name) = spec.name.as_deref() {
            if name != label {
                return Err(unsupported(
                    id,
                    "renaming a launchd job is not supported yet: create the new label and \
                     delete this one",
                ));
            }
        }

        let target = self.locate(&label, spec.location.as_deref())?;
        let existing = self
            .read_plist(&target.path, target.file_privilege)?
            .ok_or_else(|| Error::Conflict(format!("{label}: {} is gone", target.path)))?;

        let mut notes = Vec::new();
        let user = Self::plist_user(&target, spec, &mut notes);
        let before = render_dictionary(&existing)?;
        let after = if is_managed(&existing) {
            render_plist(
                &label,
                &spec.command,
                &spec.schedule,
                user,
                Self::run_at_load(spec),
            )?
        } else {
            // Somebody else's plist: keep their keys and change only
            // what this edit is actually about.
            let mut merged = existing.clone();
            Self::merge_into(&mut merged, &label, spec, user)?;
            notes.push(format!(
                "{} was not written by shuvjobs; its other keys were preserved",
                target.path
            ));
            render_dictionary(&merged)?
        };

        let mut changes = vec![
            Self::bootout_change(&target, &label),
            Change::WriteFile {
                path: target.path.clone(),
                before: Some(before),
                after,
                mode: 0o644,
                privilege: target.file_privilege,
            },
        ];
        if spec.enabled {
            changes.push(Self::enable_change(&target, &label));
            changes.push(Self::bootstrap_change(&target, &label));
        }

        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_launchctl_error(err, &label))?;
        outcome.notes.extend(notes);
        outcome.id = Some(label);
        Ok(outcome)
    }

    fn delete(&self, id: &str, mode: ApplyMode) -> Result<MutationOutcome> {
        let label = parse_launchd_id(id)?;
        self.require_macos(&label)?;
        let target = self.locate(&label, None)?;
        let before = self
            .host
            .read_to_string(&target.path, target.file_privilege)?;

        let changes = vec![
            Self::bootout_change(&target, &label),
            Change::RemoveFile {
                path: target.path.clone(),
                before,
                privilege: target.file_privilege,
            },
        ];
        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_launchctl_error(err, &label))?;
        outcome.id = Some(label);
        Ok(outcome)
    }

    fn set_enabled(&self, id: &str, enabled: bool, mode: ApplyMode) -> Result<MutationOutcome> {
        let label = parse_launchd_id(id)?;
        self.require_macos(&label)?;
        let target = self.locate(&label, None)?;

        // `disable` writes a persistent override, so it has to follow
        // the unload; `enable` clears one, so it has to precede the load.
        let changes = if enabled {
            vec![
                Self::enable_change(&target, &label),
                Self::bootstrap_change(&target, &label),
            ]
        } else {
            vec![
                Self::bootout_change(&target, &label),
                Self::disable_change(&target, &label),
            ]
        };
        let mut outcome =
            execute(&*self.host, changes, mode).map_err(|err| map_launchctl_error(err, &label))?;
        outcome.id = Some(label);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use shuvjobs_core::host::fake::Call;
    use shuvjobs_core::host::{FakeHost, PrivilegePolicy};

    const LABEL: &str = "com.example.backup";
    const USER_DIR: &str = "/home/tester/Library/LaunchAgents";
    const USER_PATH: &str = "/home/tester/Library/LaunchAgents/com.example.backup.plist";
    const DAEMON_PATH: &str = "/Library/LaunchDaemons/com.example.backup.plist";

    fn sudoer() -> PrivilegePolicy {
        PrivilegePolicy {
            is_root: false,
            sudo: true,
        }
    }

    /// A macOS host that answers the commands a user-scope create makes.
    fn mac() -> FakeHost {
        FakeHost::new()
            .with_os(HostOs::MacOs)
            .script(
                "launchctl list",
                0,
                "PID\tStatus\tLabel\n-\t0\tcom.apple.other\n",
                "",
            )
            .script(&mkdir_script(USER_DIR), 0, "", "")
            .script(&format!("launchctl enable 'gui/1000/{LABEL}'"), 0, "", "")
            .script(
                &format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
                0,
                "",
                "",
            )
            .script(&format!("launchctl bootout 'gui/1000/{LABEL}'"), 0, "", "")
            .script(&format!("launchctl disable 'gui/1000/{LABEL}'"), 0, "", "")
            .script(&format!("launchctl enable 'system/{LABEL}'"), 0, "", "")
            .script(
                &format!("launchctl bootstrap 'system' '{DAEMON_PATH}'"),
                0,
                "",
                "",
            )
            .script(&format!("launchctl bootout 'system/{LABEL}'"), 0, "", "")
    }

    fn writer(host: &Arc<FakeHost>) -> LaunchdWriter {
        LaunchdWriter::new(Arc::clone(host) as Arc<dyn Host>)
    }

    fn spec(schedule: ScheduleType, command: &str) -> JobSpec {
        let mut spec = JobSpec::new(TaskSourceKind::Launchd, schedule, command.to_string());
        spec.name = Some(LABEL.to_string());
        spec
    }

    fn interval_spec() -> JobSpec {
        spec(ScheduleType::Interval(Duration::from_secs(300)), "echo hi")
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

    // -- rendering -------------------------------------------------------

    #[test]
    fn an_interval_job_renders_to_a_pinned_plist() {
        let rendered = render_plist(
            LABEL,
            " echo hi ",
            &ScheduleType::Interval(Duration::from_secs(300)),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            rendered,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.example.backup</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>-c</string>
		<string>echo hi</string>
	</array>
	<key>StartInterval</key>
	<integer>300</integer>
	<key>RunAtLoad</key>
	<false/>
	<key>ShuvjobsManaged</key>
	<true/>
</dict>
</plist>
"#
        );
    }

    #[test]
    fn a_single_cron_time_renders_one_calendar_dictionary() {
        let rendered = render_plist(
            LABEL,
            "echo hi",
            &ScheduleType::Cron("30 9 * * *".into()),
            Some("nobody"),
            true,
        )
        .unwrap();
        assert!(
            rendered.contains(
                "\t<key>StartCalendarInterval</key>\n\
                 \t<dict>\n\
                 \t\t<key>Minute</key>\n\
                 \t\t<integer>30</integer>\n\
                 \t\t<key>Hour</key>\n\
                 \t\t<integer>9</integer>\n\
                 \t</dict>\n\
                 \t<key>UserName</key>\n\
                 \t<string>nobody</string>\n\
                 \t<key>RunAtLoad</key>\n\
                 \t<true/>\n"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn a_cron_list_renders_an_array_of_calendar_dictionaries() {
        let rendered = render_plist(
            LABEL,
            "echo hi",
            &ScheduleType::Calendar("Hour=9 Minute=0 | Hour=17 Minute=0".into()),
            None,
            false,
        )
        .unwrap();
        assert!(
            rendered.contains("\t<key>StartCalendarInterval</key>\n\t<array>\n"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("<key>Hour</key>").count(), 2, "{rendered}");
    }

    #[test]
    fn a_one_shot_belongs_to_at_and_a_zero_interval_is_invalid() {
        let when = chrono::Utc::now();
        assert!(matches!(
            render_plist(LABEL, "x", &ScheduleType::OneShot(when), None, false),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            render_plist(
                LABEL,
                "x",
                &ScheduleType::Interval(Duration::from_secs(0)),
                None,
                false
            ),
            Err(Error::Validation(_))
        ));
        assert!(render_plist(LABEL, "  ", &interval_spec().schedule, None, false).is_err());
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn a_user_job_is_written_to_launch_agents_and_bootstrapped_into_the_gui_domain() {
        let host = Arc::new(mac());
        let outcome = writer(&host)
            .create(&interval_spec(), ApplyMode::Apply)
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some(LABEL));
        assert!(outcome.applied);
        assert_eq!(
            host.calls(),
            vec![
                call("launchctl list", Privilege::User),
                call(&mkdir_script(USER_DIR), Privilege::User),
                call(
                    &format!("launchctl enable 'gui/1000/{LABEL}'"),
                    Privilege::User
                ),
                call(
                    &format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
                    Privilege::User
                ),
            ]
        );
        assert_eq!(host.file_mode(USER_PATH), Some(0o644));
        assert!(host
            .file_contents(USER_PATH)
            .unwrap()
            .contains("<key>ShuvjobsManaged</key>"));
    }

    #[test]
    fn a_system_job_is_a_daemon_written_and_loaded_as_root() {
        let host = Arc::new(mac().with_policy(sudoer()).root_only("/Library"));
        let mut spec = interval_spec();
        spec.scope = JobScope::System;
        spec.user = Some("nobody".to_string());
        writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert_eq!(
            host.calls(),
            vec![
                call("launchctl list", Privilege::User),
                call(
                    &format!("launchctl enable 'system/{LABEL}'"),
                    Privilege::Root
                ),
                call(
                    &format!("launchctl bootstrap 'system' '{DAEMON_PATH}'"),
                    Privilege::Root
                ),
            ]
        );
        let plist = host.file_contents(DAEMON_PATH).unwrap();
        assert!(
            plist.contains("<key>UserName</key>\n\t<string>nobody</string>"),
            "{plist}"
        );
    }

    #[test]
    fn an_admin_agent_is_a_root_file_in_your_own_domain() {
        let mut spec = interval_spec();
        spec.location = Some(SYSTEM_AGENT_DIR.to_string());
        spec.user = Some("nobody".to_string());
        let path = "/Library/LaunchAgents/com.example.backup.plist";
        let host = Arc::new(mac().with_policy(sudoer()).root_only("/Library").script(
            &format!("launchctl bootstrap 'gui/1000' '{path}'"),
            0,
            "",
            "",
        ));
        let outcome = writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert!(outcome.applied);
        // The file needed root; the domain did not.
        assert_eq!(
            host.calls(),
            vec![
                call("launchctl list", Privilege::User),
                call(
                    &format!("launchctl enable 'gui/1000/{LABEL}'"),
                    Privilege::User
                ),
                call(
                    &format!("launchctl bootstrap 'gui/1000' '{path}'"),
                    Privilege::User
                ),
            ]
        );
        assert!(host.file_contents(path).is_some());
        // A LaunchAgent ignores UserName, so it is dropped with a note.
        assert!(!host.file_contents(path).unwrap().contains("UserName"));
        assert!(outcome.notes.iter().any(|note| note.contains("UserName")));
    }

    #[test]
    fn a_disabled_job_is_written_but_not_loaded() {
        let host = Arc::new(mac());
        let mut spec = interval_spec();
        spec.enabled = false;
        writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert_eq!(
            commands(&host),
            vec!["launchctl list".to_string(), mkdir_script(USER_DIR)]
        );
        assert!(host.file_contents(USER_PATH).is_some());
    }

    #[test]
    fn a_label_launchd_already_knows_is_a_conflict() {
        let host = Arc::new(mac().script(
            "launchctl list",
            0,
            &format!("PID\tStatus\tLabel\n-\t0\t{LABEL}\n"),
            "",
        ));
        let err = writer(&host)
            .create(&interval_spec(), ApplyMode::Apply)
            .expect_err("must refuse to shadow a loaded job");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
        assert_eq!(host.file_contents(USER_PATH), None);
    }

    #[test]
    fn replacing_our_own_loaded_job_unloads_it_first() {
        let managed = render_plist(
            LABEL,
            "echo old",
            &ScheduleType::Interval(Duration::from_secs(60)),
            None,
            false,
        )
        .unwrap();
        let host = Arc::new(mac().file(USER_PATH, &managed).script(
            "launchctl list",
            0,
            &format!("PID\tStatus\tLabel\n-\t0\t{LABEL}\n"),
            "",
        ));
        writer(&host)
            .create(&interval_spec(), ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            commands(&host),
            vec![
                "launchctl list".to_string(),
                format!("launchctl bootout 'gui/1000/{LABEL}'"),
                mkdir_script(USER_DIR),
                format!("launchctl enable 'gui/1000/{LABEL}'"),
                format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
            ]
        );
        assert!(host.file_contents(USER_PATH).unwrap().contains("echo hi"));
    }

    #[test]
    fn an_unmanaged_plist_at_the_same_path_is_a_conflict() {
        let host = Arc::new(mac().file(
            USER_PATH,
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>Label</key><string>com.example.backup</string></dict></plist>",
        ));
        let err = writer(&host)
            .create(&interval_spec(), ApplyMode::Apply)
            .expect_err("must not overwrite somebody else's plist");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn a_label_without_a_dot_is_written_with_a_note() {
        let host = Arc::new(
            FakeHost::new()
                .with_os(HostOs::MacOs)
                .script("launchctl list", 0, "", "")
                .script(&mkdir_script(USER_DIR), 0, "", "")
                .script("launchctl enable 'gui/1000/backup'", 0, "", "")
                .script(
                    &format!("launchctl bootstrap 'gui/1000' '{USER_DIR}/backup.plist'"),
                    0,
                    "",
                    "",
                ),
        );
        let mut spec = interval_spec();
        spec.name = Some("backup".to_string());
        let outcome = writer(&host).create(&spec, ApplyMode::Apply).unwrap();
        assert!(outcome
            .notes
            .iter()
            .any(|note| note.contains("reverse-DNS")));
    }

    #[test]
    fn a_dry_run_makes_no_calls_and_writes_nothing() {
        let host = Arc::new(mac());
        let outcome = writer(&host)
            .create(&interval_spec(), ApplyMode::DryRun)
            .unwrap();
        // `launchctl list` is a planning read, not a change.
        assert_eq!(commands(&host), vec!["launchctl list".to_string()]);
        assert!(!outcome.applied);
        assert_eq!(host.file_contents(USER_PATH), None);
        assert_eq!(outcome.changes.len(), 4);
    }

    #[test]
    fn every_operation_is_unsupported_off_macos() {
        let host = Arc::new(FakeHost::new());
        let writer = writer(&host);
        let mut results = vec![
            writer.create(&interval_spec(), ApplyMode::Apply),
            writer.update(LABEL, &interval_spec(), ApplyMode::Apply),
            writer.delete(LABEL, ApplyMode::Apply),
        ];
        results.push(writer.set_enabled(LABEL, true, ApplyMode::Apply));
        for result in results {
            match result.expect_err("must refuse") {
                Error::Unsupported { reason, .. } => {
                    assert_eq!(reason, "launchd is only available on macOS")
                }
                other => panic!("expected Unsupported, got {other:?}"),
            }
        }
        assert!(host.calls().is_empty());
    }

    // -- update ----------------------------------------------------------

    #[test]
    fn an_update_regenerates_a_managed_plist_between_bootout_and_bootstrap() {
        let managed = render_plist(
            LABEL,
            "echo old",
            &ScheduleType::Interval(Duration::from_secs(60)),
            None,
            false,
        )
        .unwrap();
        let host = Arc::new(mac().file(USER_PATH, &managed));
        let outcome = writer(&host)
            .update(LABEL, &interval_spec(), ApplyMode::Apply)
            .unwrap();
        assert_eq!(outcome.id.as_deref(), Some(LABEL));
        assert_eq!(
            commands(&host),
            vec![
                format!("launchctl bootout 'gui/1000/{LABEL}'"),
                format!("launchctl enable 'gui/1000/{LABEL}'"),
                format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
            ]
        );
        let plist = host.file_contents(USER_PATH).unwrap();
        assert!(plist.contains("echo hi"), "{plist}");
        assert!(plist.contains("<integer>300</integer>"), "{plist}");
    }

    #[test]
    fn an_update_of_an_unmanaged_plist_keeps_the_keys_it_does_not_own() {
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.example.backup</string>
	<key>ProgramArguments</key>
	<array><string>/usr/bin/true</string></array>
	<key>StartCalendarInterval</key>
	<dict><key>Hour</key><integer>3</integer></dict>
	<key>StandardOutPath</key>
	<string>/tmp/backup.log</string>
	<key>KeepAlive</key>
	<true/>
	<key>UserName</key>
	<string>nobody</string>
</dict>
</plist>"#;
        let host = Arc::new(mac().file(USER_PATH, existing));
        let outcome = writer(&host)
            .update(LABEL, &interval_spec(), ApplyMode::Apply)
            .unwrap();
        let plist = host.file_contents(USER_PATH).unwrap();
        assert!(plist.contains("/tmp/backup.log"), "{plist}");
        assert!(plist.contains("<key>KeepAlive</key>"), "{plist}");
        // The schedule shape changed, so the calendar key is gone.
        assert!(!plist.contains("StartCalendarInterval"), "{plist}");
        assert!(plist.contains("<key>StartInterval</key>"), "{plist}");
        assert!(plist.contains("echo hi"), "{plist}");
        // An agent cannot honour UserName, so an edit takes it out.
        assert!(!plist.contains("UserName"), "{plist}");
        // And it stays somebody else's file.
        assert!(!plist.contains(MANAGED_KEY), "{plist}");
        assert!(outcome
            .notes
            .iter()
            .any(|note| note.contains("not written by shuvjobs")));
    }

    #[test]
    fn an_update_finds_a_daemon_when_there_is_no_agent() {
        let managed = render_plist(
            LABEL,
            "echo old",
            &ScheduleType::Interval(Duration::from_secs(60)),
            None,
            false,
        )
        .unwrap();
        let host = Arc::new(
            mac()
                .with_policy(sudoer())
                .root_only("/Library")
                .file(DAEMON_PATH, &managed),
        );
        writer(&host)
            .update(LABEL, &interval_spec(), ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            host.calls(),
            vec![
                call(
                    &format!("launchctl bootout 'system/{LABEL}'"),
                    Privilege::Root
                ),
                call(
                    &format!("launchctl enable 'system/{LABEL}'"),
                    Privilege::Root
                ),
                call(
                    &format!("launchctl bootstrap 'system' '{DAEMON_PATH}'"),
                    Privilege::Root
                ),
            ]
        );
    }

    #[test]
    fn a_job_that_is_nowhere_is_a_conflict() {
        let host = Arc::new(mac());
        let err = writer(&host)
            .update(LABEL, &interval_spec(), ApplyMode::Apply)
            .expect_err("must not invent a path");
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn a_system_plist_is_sip_protected() {
        let host = Arc::new(mac());
        let mut spec = interval_spec();
        spec.location = Some("/System/Library/LaunchDaemons".to_string());
        let err = writer(&host)
            .update(LABEL, &spec, ApplyMode::Apply)
            .expect_err("must refuse /System");
        match err {
            Error::Unsupported { reason, .. } => assert!(reason.contains("SIP"), "{reason}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(host.calls().is_empty());
    }

    #[test]
    fn renaming_is_not_supported_yet() {
        let host = Arc::new(mac());
        let mut spec = interval_spec();
        spec.name = Some("com.example.other".to_string());
        let err = writer(&host)
            .update(LABEL, &spec, ApplyMode::Apply)
            .expect_err("must refuse a rename");
        assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
    }

    // -- delete and enable -----------------------------------------------

    #[test]
    fn delete_unloads_then_removes_the_plist() {
        let managed = render_plist(
            LABEL,
            "echo hi",
            &ScheduleType::Interval(Duration::from_secs(60)),
            None,
            false,
        )
        .unwrap();
        let host = Arc::new(mac().file(USER_PATH, &managed));
        let outcome = writer(&host).delete(LABEL, ApplyMode::Apply).unwrap();
        assert_eq!(outcome.id.as_deref(), Some(LABEL));
        assert_eq!(
            commands(&host),
            vec![format!("launchctl bootout 'gui/1000/{LABEL}'")]
        );
        assert_eq!(host.file_contents(USER_PATH), None);
        // The removed contents are carried for the backup and the diff.
        assert_eq!(outcome.backup_targets().len(), 1);
    }

    #[test]
    fn a_dry_run_delete_touches_nothing() {
        let host = Arc::new(mac().file(USER_PATH, "<plist/>"));
        let outcome = writer(&host).delete(LABEL, ApplyMode::DryRun).unwrap();
        assert!(host.calls().is_empty());
        assert!(!outcome.applied);
        assert_eq!(outcome.changes.len(), 2);
        assert!(host.file_contents(USER_PATH).is_some());
    }

    #[test]
    fn disabling_unloads_and_then_writes_the_override() {
        let host = Arc::new(mac().file(USER_PATH, "<plist/>"));
        writer(&host)
            .set_enabled(LABEL, false, ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            commands(&host),
            vec![
                format!("launchctl bootout 'gui/1000/{LABEL}'"),
                format!("launchctl disable 'gui/1000/{LABEL}'"),
            ]
        );
    }

    #[test]
    fn enabling_clears_the_override_and_then_loads() {
        let host = Arc::new(mac().file(USER_PATH, "<plist/>"));
        writer(&host)
            .set_enabled(LABEL, true, ApplyMode::Apply)
            .unwrap();
        assert_eq!(
            commands(&host),
            vec![
                format!("launchctl enable 'gui/1000/{LABEL}'"),
                format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
            ]
        );
    }

    #[test]
    fn a_domain_that_cannot_be_bootstrapped_is_unsupported() {
        let host = Arc::new(mac().file(USER_PATH, "<plist/>").script(
            &format!("launchctl bootstrap 'gui/1000' '{USER_PATH}'"),
            5,
            "",
            "Bootstrap failed: 125: Domain does not support specified action\n",
        ));
        let err = writer(&host)
            .set_enabled(LABEL, true, ApplyMode::Apply)
            .expect_err("must fail");
        match err {
            Error::Unsupported { reason, .. } => assert!(reason.contains("125"), "{reason}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_label_never_reaches_the_host() {
        let host = Arc::new(mac());
        assert!(matches!(
            writer(&host).delete("no spaces allowed", ApplyMode::Apply),
            Err(Error::Validation(_))
        ));
        assert!(host.calls().is_empty());
    }

    #[test]
    fn the_writer_reports_its_own_source() {
        let host = Arc::new(mac());
        assert_eq!(writer(&host).kind(), TaskSourceKind::Launchd);
    }
}
