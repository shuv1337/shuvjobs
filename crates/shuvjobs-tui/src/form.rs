//! The add/edit form's state machine, kept free of ratatui so the whole
//! thing is testable without a terminal: keys in, [`FormEvent`] out, and
//! a [`JobSpec`] once the fields validate.
//!
//! Which fields exist at all depends on the source — `at` has no scope
//! and no name, anacron has no scope — so [`Form::visible_fields`] is
//! the single answer used by focus cycling, rendering, and `to_spec`.

use crossterm::event::KeyCode;
use shuvjobs_core::manage::{parse_schedule, JobScope, JobSpec};
use shuvjobs_core::{ScheduleType, ScheduledTask, TaskSourceKind};

/// Sources the form can create a job for, in the order `◂`/`▸` walks them.
pub const SOURCES: [TaskSourceKind; 5] = [
    TaskSourceKind::Systemd,
    TaskSourceKind::Cron,
    TaskSourceKind::At,
    TaskSourceKind::Anacron,
    TaskSourceKind::Launchd,
];

/// A single-line editor. The cursor is a *character* index, so a
/// multi-byte value never lands the caret inside a code point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    pub value: String,
    pub cursor: usize,
}

impl TextInput {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn len_chars(&self) -> usize {
        self.value.chars().count()
    }

    /// Byte offset of character index `at`, for `String::insert`/`remove`.
    fn byte_of(&self, at: usize) -> usize {
        self.value
            .char_indices()
            .nth(at)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, ch: char) {
        let at = self.byte_of(self.cursor);
        self.value.insert(at, ch);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_of(self.cursor);
        self.value.remove(at);
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.len_chars() {
            return;
        }
        let at = self.byte_of(self.cursor);
        self.value.remove(at);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len_chars());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len_chars();
    }
}

/// One row of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Source,
    Scope,
    Name,
    Schedule,
    Command,
    User,
    Enabled,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Source => "Source",
            Field::Scope => "Scope",
            Field::Name => "Name",
            Field::Schedule => "Schedule",
            Field::Command => "Command",
            Field::User => "User",
            Field::Enabled => "Enabled",
        }
    }

    /// Pickers are cycled with `◂`/`▸`; everything else is a text input.
    pub fn is_picker(self) -> bool {
        matches!(self, Field::Source | Field::Scope | Field::Enabled)
    }
}

/// Whether the form creates a job or rewrites an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Add,
    Edit { id: String },
}

/// What the caller must do after a key.
#[derive(Debug)]
pub enum FormEvent {
    /// The form handled it; nothing else to do.
    Consumed,
    /// The fields validated: plan this spec.
    Submit(JobSpec),
    /// Esc: close without asking the host anything.
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub intent: Intent,
    pub source: TaskSourceKind,
    pub scope: JobScope,
    pub name: TextInput,
    pub schedule: TextInput,
    pub command: TextInput,
    pub user: TextInput,
    pub enabled: bool,
    pub focus: Field,
    /// Last validation or planning failure, shown under the fields.
    pub error: Option<String>,
    /// Set while a plan request is in flight: every key is ignored so
    /// the spec the worker is planning cannot drift under it.
    pub locked: bool,
}

impl Form {
    pub fn new_add(source: TaskSourceKind) -> Self {
        let mut form = Self {
            intent: Intent::Add,
            source,
            scope: JobScope::User,
            name: TextInput::default(),
            schedule: TextInput::default(),
            command: TextInput::default(),
            user: TextInput::default(),
            enabled: true,
            focus: Field::Source,
            error: None,
            locked: false,
        };
        form.focus = form.first_field();
        form
    }

    /// Prefill from the selected row. The source cannot change on an
    /// edit — moving a job between schedulers is a delete plus an add —
    /// so `Source` is not among the visible fields.
    pub fn from_task(task: &ScheduledTask) -> Self {
        let mut form = Self {
            intent: Intent::Edit {
                id: task.id.clone(),
            },
            source: task.source,
            scope: scope_of(task),
            name: TextInput::new(task.name.clone()),
            schedule: TextInput::new(schedule_text(&task.schedule)),
            command: TextInput::new(task.command.clone()),
            user: TextInput::default(),
            enabled: task.enabled.unwrap_or(true),
            focus: Field::Schedule,
            error: None,
            locked: false,
        };
        form.focus = form.first_field();
        form
    }

    fn first_field(&self) -> Field {
        self.visible_fields()
            .first()
            .copied()
            .unwrap_or(Field::Command)
    }

    /// Which rows this source actually has. `Source` only appears on an
    /// add; `User` only where running a job as somebody else is a thing
    /// the scheduler can express.
    pub fn visible_fields(&self) -> Vec<Field> {
        let mut fields = Vec::new();
        if self.intent == Intent::Add {
            fields.push(Field::Source);
        }
        match self.source {
            TaskSourceKind::Systemd => {
                fields.extend([
                    Field::Scope,
                    Field::Name,
                    Field::Schedule,
                    Field::Command,
                    Field::Enabled,
                ]);
            }
            TaskSourceKind::Cron => {
                fields.extend([Field::Scope, Field::Schedule, Field::Command]);
                if self.scope == JobScope::System {
                    fields.push(Field::User);
                }
                fields.push(Field::Enabled);
            }
            TaskSourceKind::At => fields.extend([Field::Schedule, Field::Command]),
            TaskSourceKind::Anacron => {
                fields.extend([Field::Name, Field::Schedule, Field::Command]);
            }
            TaskSourceKind::Launchd => {
                fields.extend([
                    Field::Scope,
                    Field::Name,
                    Field::Schedule,
                    Field::Command,
                    Field::Enabled,
                ]);
            }
        }
        fields
    }

    pub fn title(&self) -> String {
        match &self.intent {
            Intent::Add => format!("add {} job", self.source),
            Intent::Edit { id } => format!("edit {id}"),
        }
    }

    /// Placeholder shown while the schedule is empty: the shortest thing
    /// this source would actually accept.
    pub fn schedule_hint(&self) -> &'static str {
        schedule_hint(self.source)
    }

    pub fn input(&self, field: Field) -> Option<&TextInput> {
        match field {
            Field::Name => Some(&self.name),
            Field::Schedule => Some(&self.schedule),
            Field::Command => Some(&self.command),
            Field::User => Some(&self.user),
            _ => None,
        }
    }

    fn input_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Schedule => Some(&mut self.schedule),
            Field::Command => Some(&mut self.command),
            Field::User => Some(&mut self.user),
            _ => None,
        }
    }

    /// The picker's current value, as rendered between `◂` and `▸`.
    pub fn picker_value(&self, field: Field) -> String {
        match field {
            Field::Source => self.source.to_string(),
            Field::Scope => match self.scope {
                JobScope::System => "system".to_string(),
                JobScope::User => "user".to_string(),
            },
            Field::Enabled => if self.enabled { "yes" } else { "no" }.to_string(),
            _ => String::new(),
        }
    }

    fn cycle_focus(&mut self, delta: isize) {
        let fields = self.visible_fields();
        if fields.is_empty() {
            return;
        }
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        let len = fields.len() as isize;
        let next = (at + delta).rem_euclid(len) as usize;
        self.focus = fields[next];
    }

    fn cycle_picker(&mut self, delta: isize) {
        match self.focus {
            Field::Source => {
                let at = SOURCES.iter().position(|k| *k == self.source).unwrap_or(0) as isize;
                let len = SOURCES.len() as isize;
                self.source = SOURCES[(at + delta).rem_euclid(len) as usize];
                // The new source may not have the row the focus sits on
                // any more, and `Source` itself is always first.
                self.focus = Field::Source;
            }
            Field::Scope => {
                self.scope = match self.scope {
                    JobScope::System => JobScope::User,
                    JobScope::User => JobScope::System,
                };
            }
            Field::Enabled => self.enabled = !self.enabled,
            _ => {}
        }
    }

    /// Every key while a plan is in flight is dropped: `locked` is set
    /// exactly then, and the worker is already planning this spec.
    pub fn handle_key(&mut self, code: KeyCode) -> FormEvent {
        if self.locked {
            return FormEvent::Consumed;
        }
        match code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Enter => {
                return match self.to_spec() {
                    Ok(spec) => FormEvent::Submit(spec),
                    Err(message) => {
                        self.error = Some(message);
                        FormEvent::Consumed
                    }
                };
            }
            KeyCode::Tab | KeyCode::Down => self.cycle_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.cycle_focus(-1),
            KeyCode::Left if self.focus.is_picker() => self.cycle_picker(-1),
            KeyCode::Right if self.focus.is_picker() => self.cycle_picker(1),
            KeyCode::Char(' ') if self.focus.is_picker() => self.cycle_picker(1),
            _ => {
                let focus = self.focus;
                let Some(input) = self.input_mut(focus) else {
                    return FormEvent::Consumed;
                };
                match code {
                    KeyCode::Char(c) => input.insert(c),
                    KeyCode::Backspace => input.backspace(),
                    KeyCode::Delete => input.delete(),
                    KeyCode::Left => input.left(),
                    KeyCode::Right => input.right(),
                    KeyCode::Home => input.home(),
                    KeyCode::End => input.end(),
                    _ => return FormEvent::Consumed,
                }
                // Any edit invalidates the message that described the
                // previous contents.
                self.error = None;
            }
        }
        FormEvent::Consumed
    }

    /// Turn the fields into a spec, or say in one line what is wrong.
    /// Text is trimmed here and nowhere else, so a stray space in the
    /// command box never reaches a crontab.
    pub fn to_spec(&self) -> Result<JobSpec, String> {
        let visible = self.visible_fields();
        let command = self.command.value.trim();
        if command.is_empty() {
            return Err("command must not be empty".to_string());
        }
        let schedule_text = self.schedule.value.trim();
        if schedule_text.is_empty() {
            return Err("schedule must not be empty".to_string());
        }
        let schedule = parse_schedule(self.source, schedule_text).map_err(|e| e.to_string())?;

        let mut spec = JobSpec::new(self.source, schedule, command.to_string());
        spec.scope = self.scope;
        spec.enabled = self.enabled;
        if visible.contains(&Field::Name) {
            let name = self.name.value.trim();
            spec.name = (!name.is_empty()).then(|| name.to_string());
        }
        if visible.contains(&Field::User) {
            let user = self.user.value.trim();
            spec.user = (!user.is_empty()).then(|| user.to_string());
        }
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }
}

pub fn schedule_hint(source: TaskSourceKind) -> &'static str {
    match source {
        TaskSourceKind::Cron => "*/5 * * * *",
        TaskSourceKind::Systemd => "*-*-* 03:00:00",
        TaskSourceKind::At => "now + 1 hour",
        TaskSourceKind::Anacron => "7",
        TaskSourceKind::Launchd => "3600",
    }
}

/// The schedule as the operator would type it back in: the same text the
/// export format carries, so an edit round-trips through `parse_schedule`.
fn schedule_text(schedule: &ScheduleType) -> String {
    match schedule {
        ScheduleType::Cron(expr) | ScheduleType::Calendar(expr) => expr.clone(),
        ScheduleType::Interval(d) => format!("{}s", d.as_secs()),
        ScheduleType::OneShot(dt) => dt.to_rfc3339(),
    }
}

/// Where the job lives, read back out of its id or its backing file.
fn scope_of(task: &ScheduledTask) -> JobScope {
    match task.source {
        // `user/foo.timer` is the id the systemd adapter mints for the
        // user manager.
        TaskSourceKind::Systemd if task.id.starts_with("user/") => JobScope::User,
        TaskSourceKind::Cron if task.id.starts_with("user:") => JobScope::User,
        TaskSourceKind::Launchd => match task.location.as_deref() {
            // `/Library/LaunchAgents` is the machine-wide agent
            // directory; only a copy under the home directory is
            // this user's own.
            Some(path)
                if path.contains("/Library/LaunchAgents")
                    && !path.starts_with("/Library/")
                    && !path.starts_with("/System/") =>
            {
                JobScope::User
            }
            _ => JobScope::System,
        },
        _ => JobScope::System,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn task(source: TaskSourceKind, id: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            name: "backup".to_string(),
            source,
            schedule: ScheduleType::Cron("0 9 * * *".to_string()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: "echo hi".to_string(),
            location: None,
            enabled: None,
        }
    }

    #[test]
    fn cursor_math_counts_characters_not_bytes() {
        let mut input = TextInput::new("héllo");
        assert_eq!(input.cursor, 5);
        input.home();
        input.right();
        // Sitting between `h` and `é`; inserting must not split the
        // two-byte code point.
        input.insert('X');
        assert_eq!(input.value, "hXéllo");
        assert_eq!(input.cursor, 2);
        input.delete();
        assert_eq!(input.value, "hXllo");
        input.backspace();
        assert_eq!(input.value, "hllo");
        assert_eq!(input.cursor, 1);
        input.end();
        assert_eq!(input.cursor, 4);
        input.right();
        assert_eq!(input.cursor, 4, "right stops at the end");
        input.home();
        input.left();
        assert_eq!(input.cursor, 0, "left stops at the start");
        input.backspace();
        assert_eq!(input.value, "hllo", "backspace at the start is a no-op");
    }

    #[test]
    fn pickers_wrap_in_both_directions() {
        let mut form = Form::new_add(TaskSourceKind::Systemd);
        assert_eq!(form.focus, Field::Source);
        form.handle_key(KeyCode::Left);
        assert_eq!(form.source, TaskSourceKind::Launchd, "wrapped backwards");
        form.handle_key(KeyCode::Right);
        assert_eq!(form.source, TaskSourceKind::Systemd, "wrapped forwards");

        form.handle_key(KeyCode::Tab);
        assert_eq!(form.focus, Field::Scope);
        assert_eq!(form.picker_value(Field::Scope), "user");
        form.handle_key(KeyCode::Right);
        assert_eq!(form.scope, JobScope::System);
        form.handle_key(KeyCode::Right);
        assert_eq!(form.scope, JobScope::User, "two values, so it wraps");
    }

    #[test]
    fn focus_cycles_only_the_visible_fields() {
        let mut form = Form::new_add(TaskSourceKind::At);
        assert_eq!(
            form.visible_fields(),
            vec![Field::Source, Field::Schedule, Field::Command]
        );
        form.handle_key(KeyCode::BackTab);
        assert_eq!(form.focus, Field::Command, "backtab wraps to the last row");
        form.handle_key(KeyCode::Tab);
        assert_eq!(form.focus, Field::Source);
    }

    #[test]
    fn visible_fields_follow_the_source() {
        let mut form = Form::new_add(TaskSourceKind::Systemd);
        assert_eq!(
            form.visible_fields(),
            vec![
                Field::Source,
                Field::Scope,
                Field::Name,
                Field::Schedule,
                Field::Command,
                Field::Enabled
            ]
        );

        form.source = TaskSourceKind::Cron;
        assert_eq!(
            form.visible_fields(),
            vec![
                Field::Source,
                Field::Scope,
                Field::Schedule,
                Field::Command,
                Field::Enabled
            ],
            "user scope runs as the invoking user, so there is no User row"
        );
        form.scope = JobScope::System;
        assert!(form.visible_fields().contains(&Field::User));

        form.source = TaskSourceKind::Anacron;
        assert_eq!(
            form.visible_fields(),
            vec![Field::Source, Field::Name, Field::Schedule, Field::Command]
        );

        form.source = TaskSourceKind::Launchd;
        assert_eq!(
            form.visible_fields(),
            vec![
                Field::Source,
                Field::Scope,
                Field::Name,
                Field::Schedule,
                Field::Command,
                Field::Enabled
            ]
        );
    }

    #[test]
    fn edit_hides_the_source_row_and_prefills() {
        let mut base = task(TaskSourceKind::Systemd, "user/backup.timer");
        base.schedule = ScheduleType::Interval(Duration::from_secs(3600));
        base.enabled = Some(false);
        let form = Form::from_task(&base);
        assert_eq!(
            form.intent,
            Intent::Edit {
                id: "user/backup.timer".into()
            }
        );
        assert!(!form.visible_fields().contains(&Field::Source));
        assert_eq!(form.focus, Field::Scope, "focus starts on the first row");
        assert_eq!(
            form.scope,
            JobScope::User,
            "the `user/` prefix is the user manager"
        );
        assert_eq!(form.schedule.value, "3600s");
        assert_eq!(form.command.value, "echo hi");
        assert!(!form.enabled);
    }

    #[test]
    fn scope_comes_from_the_id_or_the_backing_file() {
        assert_eq!(
            Form::from_task(&task(TaskSourceKind::Systemd, "logrotate.timer")).scope,
            JobScope::System
        );
        assert_eq!(
            Form::from_task(&task(TaskSourceKind::Cron, "user:alice:4")).scope,
            JobScope::User
        );
        assert_eq!(
            Form::from_task(&task(TaskSourceKind::Cron, "/etc/cron.d/x:2")).scope,
            JobScope::System
        );

        let mut agent = task(TaskSourceKind::Launchd, "com.example.job");
        agent.location = Some("/Users/alice/Library/LaunchAgents/com.example.job.plist".into());
        assert_eq!(Form::from_task(&agent).scope, JobScope::User);
        agent.location = Some("/Library/LaunchAgents/com.example.job.plist".into());
        assert_eq!(
            Form::from_task(&agent).scope,
            JobScope::System,
            "the machine-wide agent directory is not the user's"
        );
    }

    #[test]
    fn to_spec_trims_defaults_and_validates() {
        let mut form = Form::new_add(TaskSourceKind::Cron);
        assert_eq!(
            form.to_spec().unwrap_err(),
            "command must not be empty",
            "an empty form names the missing field"
        );

        form.command = TextInput::new("  echo hi  ");
        assert_eq!(form.to_spec().unwrap_err(), "schedule must not be empty");

        form.schedule = TextInput::new("not a schedule");
        assert!(form.to_spec().unwrap_err().contains("cannot understand"));

        form.schedule = TextInput::new(" */5 * * * * ");
        let spec = form.to_spec().expect("valid");
        assert_eq!(spec.command, "echo hi", "trimmed");
        assert_eq!(spec.schedule, ScheduleType::Cron("*/5 * * * *".into()));
        assert_eq!(spec.scope, JobScope::User);
        assert!(spec.enabled);
        assert_eq!(spec.name, None, "cron has no name row, so no name");
        assert_eq!(spec.user, None);

        // The User row only exists in system scope, and only then is it read.
        form.user = TextInput::new("alice");
        assert_eq!(form.to_spec().unwrap().user, None);
        form.scope = JobScope::System;
        assert_eq!(form.to_spec().unwrap().user.as_deref(), Some("alice"));

        form.user = TextInput::new("alice; rm -rf /");
        assert!(form.to_spec().unwrap_err().contains("invalid user name"));
    }

    #[test]
    fn a_locked_form_ignores_every_key() {
        let mut form = Form::new_add(TaskSourceKind::Cron);
        form.locked = true;
        let before = form.clone();
        for code in [
            KeyCode::Char('x'),
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Enter,
        ] {
            assert!(matches!(form.handle_key(code), FormEvent::Consumed));
        }
        assert_eq!(form, before);
    }

    #[test]
    fn schedule_hints_are_per_source() {
        assert_eq!(schedule_hint(TaskSourceKind::Cron), "*/5 * * * *");
        assert_eq!(schedule_hint(TaskSourceKind::Systemd), "*-*-* 03:00:00");
        assert_eq!(schedule_hint(TaskSourceKind::At), "now + 1 hour");
        assert_eq!(schedule_hint(TaskSourceKind::Anacron), "7");
        assert_eq!(schedule_hint(TaskSourceKind::Launchd), "3600");
    }
}
