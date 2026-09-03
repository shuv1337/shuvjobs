//! ratatui frontend for shuvjobs. Depends only on `shuvjobs-core` — filter/sort/search
//! logic lives there in `shuvjobs_core::view`; this crate is just the keyboard
//! and render layer.

mod form;

use std::collections::HashSet;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use shuvjobs_core::manage::{Change, MutationOutcome};
use shuvjobs_core::{
    view::{apply, Filter, SortMode},
    Op, ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus,
};

use crate::form::{Field, Form, FormEvent, Intent};

/// Detail-pane absolute stamp: local wall clock plus the zone offset, so a
/// timestamp is never ambiguous when read on another machine.
const ABSOLUTE_FMT: &str = "%Y-%m-%d %H:%M:%S %Z";
/// One-shot schedules are shown in table cells too, so they stay compact.
const ONE_SHOT_FMT: &str = "%Y-%m-%d %H:%M %Z";
const DATE_FMT: &str = "%Y-%m-%d";

/// `initial` is the first paint. `refresh` and `mutate` are moved onto one
/// background worker thread, so they must be `Send`; the event loop never
/// calls either inline. If `refresh_secs` is set the TUI re-collects on that
/// interval, measured from the last *completed* refresh; `r` re-collects on
/// demand either way. With `mutate` absent the TUI is exactly the read-only
/// viewer it was, down to the footer hints.
pub struct RunOptions {
    pub initial: Vec<ScheduledTask>,
    pub refresh: Option<RefreshFn>,
    pub mutate: Option<MutateFn>,
    pub refresh_secs: Option<u64>,
    /// `--dry-run`: the confirm popup still renders the plan, but `y`
    /// reports "dry run: not applied" instead of writing anything.
    pub dry_run: bool,
}

/// Collection callback. `Send` because it lives on the worker thread.
pub type RefreshFn = Box<dyn FnMut() -> Result<Vec<ScheduledTask>> + Send>;

/// Mutation callback, called twice per confirmed action: once to plan and
/// once to apply. `Send` for the same reason as [`RefreshFn`].
pub type MutateFn = Box<dyn FnMut(Op, Stage) -> Result<MutationOutcome> + Send>;

/// Which half of a mutation the worker is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Read-only: what would change. Feeds the confirm popup.
    Plan,
    /// Carry it out.
    Apply,
}

pub fn run(opts: RunOptions) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    // No mouse capture: the TUI is keyboard-driven, and leaving the
    // mouse alone keeps terminal-native text selection and copy working.
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    let result = event_loop(&mut terminal, opts);

    // Restore the terminal even when the loop bailed.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    Filter,
    Search,
    /// The add/edit popup owns the keyboard.
    Form,
    /// The rendered plan is up, waiting for `y` or `n`.
    Confirm,
}

/// What the single in-flight worker request is for. One at a time keeps the
/// reply channel unambiguous: every reply belongs to whatever `busy` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Refresh,
    Plan,
    Apply,
}

enum Request {
    Refresh,
    /// Boxed: a whole `JobSpec` rides along, and a plain refresh should
    /// not have to pay for the space.
    Mutate {
        op: Box<Op>,
        stage: Stage,
    },
}

/// One finished mutation, boxed into [`Reply::Mutated`] for the same
/// reason [`Request::Mutate`] boxes its op.
struct Mutated {
    op: Op,
    stage: Stage,
    result: Result<MutationOutcome>,
}

enum Reply {
    Refreshed(Result<Vec<ScheduledTask>>),
    Mutated(Box<Mutated>),
}

/// Owns both host-facing closures on a background thread so an SSH round
/// trip never blocks the event loop. The loop sends a request and picks the
/// reply up later with `try_recv`.
struct Worker {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    /// Whether `a`/`e`/`d`/`t` do anything at all.
    can_mutate: bool,
    can_refresh: bool,
}

impl Worker {
    /// `None` when there is nothing to run: a viewer opened on a static
    /// snapshot has no thread and no channels.
    fn spawn(refresh: Option<RefreshFn>, mutate: Option<MutateFn>) -> Option<Self> {
        let (can_refresh, can_mutate) = (refresh.is_some(), mutate.is_some());
        if !can_refresh && !can_mutate {
            return None;
        }
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (rep_tx, rep_rx) = mpsc::channel::<Reply>();
        // Deliberately detached: the handle is dropped, never joined. When the
        // App goes away `req_tx` drops, `recv` fails, and the thread ends after
        // whatever call it is inside returns. A worker stuck in a slow SSH call
        // is left to finish on its own after the terminal has been restored, so
        // quitting is always immediate instead of waiting on the network.
        thread::spawn(move || {
            let mut refresh = refresh;
            let mut mutate = mutate;
            while let Ok(request) = req_rx.recv() {
                let reply = match request {
                    Request::Refresh => Reply::Refreshed(match refresh.as_mut() {
                        Some(refresh) => refresh(),
                        None => Err(anyhow!("collection is not available")),
                    }),
                    Request::Mutate { op, stage } => {
                        let op = *op;
                        let result = match mutate.as_mut() {
                            Some(mutate) => mutate(op.clone(), stage),
                            None => Err(anyhow!("this session cannot change jobs")),
                        };
                        Reply::Mutated(Box::new(Mutated { op, stage, result }))
                    }
                };
                if rep_tx.send(reply).is_err() {
                    break;
                }
            }
        });
        Some(Self {
            requests: req_tx,
            replies: rep_rx,
            can_mutate,
            can_refresh,
        })
    }
}

/// A mutation the operator has asked for and not yet confirmed.
struct Pending {
    op: Op,
    /// `None` only while the plan request is still in flight.
    plan: Option<MutationOutcome>,
    scroll: u16,
}

/// The one-line result of the last action, shown in the header until the
/// next one replaces it.
struct Notice {
    text: String,
    is_error: bool,
}

struct App {
    all: Vec<ScheduledTask>,
    available_sources: Vec<TaskSourceKind>,
    filter: Filter,
    sort: SortMode,
    visible: Vec<ScheduledTask>,
    table_state: TableState,
    mode: Mode,
    filter_cursor: usize,
    detail_open: bool,
    worker: Option<Worker>,
    refresh_secs: Option<u64>,
    /// Start of the interval countdown: set when a refresh *completes*, so a
    /// slow collection never queues up back-to-back refreshes.
    last_refresh: Instant,
    /// The single in-flight request, if any.
    busy: Option<Busy>,
    /// First line of the last failed collection, kept until one succeeds.
    refresh_error: Option<String>,
    /// Open add/edit popup.
    form: Option<Form>,
    /// The mutation being planned or confirmed.
    pending: Option<Pending>,
    notice: Option<Notice>,
    dry_run: bool,
    quit: bool,
}

impl App {
    fn new(opts: RunOptions) -> Self {
        let available_sources = available_sources_of(&opts.initial);
        let filter = Filter {
            allowed_sources: Some(available_sources.iter().copied().collect()),
            search: String::new(),
        };
        let mut app = Self {
            all: opts.initial,
            available_sources,
            filter,
            sort: SortMode::Default,
            visible: Vec::new(),
            table_state: TableState::default(),
            mode: Mode::Normal,
            filter_cursor: 0,
            detail_open: false,
            worker: Worker::spawn(opts.refresh, opts.mutate),
            refresh_secs: opts.refresh_secs,
            last_refresh: Instant::now(),
            busy: None,
            refresh_error: None,
            form: None,
            pending: None,
            notice: None,
            dry_run: opts.dry_run,
            quit: false,
        };
        app.refilter();
        app
    }

    fn refilter(&mut self) {
        self.visible = apply(&self.all, &self.filter, self.sort);
        if self.visible.is_empty() {
            self.table_state.select(None);
        } else {
            let cur = self.table_state.selected().unwrap_or(0);
            let next = cur.min(self.visible.len() - 1);
            self.table_state.select(Some(next));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        let cur = self.table_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, max);
        self.table_state.select(Some(next as usize));
    }

    fn toggle_source_at_cursor(&mut self) {
        let Some(kind) = self.available_sources.get(self.filter_cursor).copied() else {
            return;
        };
        let allowed = self.filter.allowed_sources.get_or_insert_with(HashSet::new);
        if allowed.contains(&kind) {
            allowed.remove(&kind);
        } else {
            allowed.insert(kind);
        }
        self.refilter();
    }

    fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.refilter();
    }

    fn handle_key(&mut self, code: KeyCode) {
        match self.mode {
            Mode::Normal => self.handle_normal(code),
            Mode::Filter => self.handle_filter(code),
            Mode::Search => self.handle_search(code),
            Mode::Form => self.handle_form(code),
            Mode::Confirm => self.handle_confirm(code),
        }
    }

    fn selected(&self) -> Option<&ScheduledTask> {
        self.table_state
            .selected()
            .and_then(|i| self.visible.get(i))
    }

    /// Whether a mutation key should do anything: there has to be a
    /// handle, and nothing may already be in flight.
    fn can_mutate(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| w.can_mutate) && self.busy.is_none()
    }

    fn handle_normal(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => {
                if !self.visible.is_empty() {
                    self.table_state.select(Some(0));
                }
            }
            KeyCode::End => {
                if !self.visible.is_empty() {
                    self.table_state.select(Some(self.visible.len() - 1));
                }
            }
            KeyCode::Char('f') => {
                self.mode = Mode::Filter;
                self.filter_cursor = 0;
            }
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('r') => self.request_refresh(),
            KeyCode::Char('a') if self.can_mutate() => self.open_add(),
            KeyCode::Char('e') if self.can_mutate() => self.open_edit(),
            KeyCode::Char('d') if self.can_mutate() => self.start_delete(),
            KeyCode::Char('t') if self.can_mutate() => self.start_toggle(),
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
            }
            KeyCode::Char('l') | KeyCode::Enter | KeyCode::Right => {
                if !self.visible.is_empty() {
                    self.detail_open = true;
                }
            }
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Left => {
                self.detail_open = false;
            }
            _ => {}
        }
    }

    fn handle_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Left | KeyCode::Char('h') => {
                if self.filter_cursor > 0 {
                    self.filter_cursor -= 1;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.filter_cursor + 1 < self.available_sources.len() {
                    self.filter_cursor += 1;
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_source_at_cursor(),
            _ => {}
        }
    }

    fn handle_search(&mut self, code: KeyCode) {
        match code {
            // Esc clears the query; Enter keeps it but exits search mode.
            KeyCode::Esc => {
                self.filter.search.clear();
                self.mode = Mode::Normal;
                self.refilter();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.search.pop();
                self.refilter();
            }
            KeyCode::Char(c) => {
                self.filter.search.push(c);
                self.refilter();
            }
            _ => {}
        }
    }

    /// `a`: a blank form for whichever source this machine actually has,
    /// falling back to cron on an empty table.
    fn open_add(&mut self) {
        let source = self
            .available_sources
            .first()
            .copied()
            .unwrap_or(TaskSourceKind::Cron);
        self.notice = None;
        self.form = Some(Form::new_add(source));
        self.mode = Mode::Form;
    }

    /// `e`: the same form, prefilled from the selected row.
    fn open_edit(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let form = Form::from_task(task);
        self.notice = None;
        self.form = Some(form);
        self.mode = Mode::Form;
    }

    fn start_delete(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let op = Op::Delete {
            id: task.id.clone(),
            source: task.source,
        };
        self.send_mutation(op, Stage::Plan);
    }

    /// `t`: a job whose state we could not read is assumed to be on, so
    /// the obvious next step is turning it off.
    fn start_toggle(&mut self) {
        let Some(task) = self.selected() else {
            return;
        };
        let op = Op::SetEnabled {
            id: task.id.clone(),
            source: task.source,
            enabled: !task.enabled.unwrap_or(true),
        };
        self.send_mutation(op, Stage::Plan);
    }

    fn handle_form(&mut self, code: KeyCode) {
        let Some(form) = self.form.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let op = match form.handle_key(code) {
            FormEvent::Consumed => return,
            FormEvent::Cancel => {
                self.form = None;
                self.mode = Mode::Normal;
                return;
            }
            FormEvent::Submit(spec) => {
                let op = match &form.intent {
                    Intent::Add => Op::Create(spec),
                    Intent::Edit { id } => Op::Update {
                        id: id.clone(),
                        source: form.source,
                        spec,
                    },
                };
                // Locked until the plan comes back: the worker is
                // planning this exact spec, and an edit under it would
                // make the confirm popup a lie.
                form.locked = true;
                op
            }
        };
        if !self.send_mutation(op, Stage::Plan) {
            if let Some(form) = self.form.as_mut() {
                form.locked = false;
            }
        }
    }

    fn handle_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => self.confirm_apply(),
            KeyCode::Char('n') | KeyCode::Esc => {
                self.pending = None;
                self.form = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_confirm(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_confirm(-1),
            KeyCode::PageDown => self.scroll_confirm(10),
            KeyCode::PageUp => self.scroll_confirm(-10),
            _ => {}
        }
    }

    fn scroll_confirm(&mut self, delta: i32) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        let next = i64::from(pending.scroll) + i64::from(delta);
        pending.scroll = next.clamp(0, i64::from(u16::MAX)) as u16;
    }

    fn confirm_apply(&mut self) {
        if self.dry_run {
            // The plan was rendered from a read-only pass; there is
            // nothing to undo and nothing to report but the refusal.
            self.notice = Some(Notice {
                text: "dry run: not applied".to_string(),
                is_error: false,
            });
            self.pending = None;
            self.form = None;
            self.mode = Mode::Normal;
            return;
        }
        let Some(op) = self.pending.as_ref().map(|p| p.op.clone()) else {
            self.mode = Mode::Normal;
            return;
        };
        self.send_mutation(op, Stage::Apply);
    }

    /// Hand one half of a mutation to the worker. `false` means nothing
    /// was sent: no handle, something already in flight, or a dead
    /// worker — in every case the caller must not pretend it started.
    fn send_mutation(&mut self, op: Op, stage: Stage) -> bool {
        if self.busy.is_some() {
            return false;
        }
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        if !worker.can_mutate {
            return false;
        }
        let request = Request::Mutate {
            op: Box::new(op),
            stage,
        };
        if worker.requests.send(request).is_err() {
            return false;
        }
        self.busy = Some(match stage {
            Stage::Plan => Busy::Plan,
            Stage::Apply => Busy::Apply,
        });
        true
    }

    /// Ask the worker to collect. Ignored when a request is already in
    /// flight — one at a time keeps the reply channel unambiguous and
    /// stops `r` mashing from queueing up SSH sessions.
    fn request_refresh(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if !worker.can_refresh {
            return;
        }
        if worker.requests.send(Request::Refresh).is_ok() {
            self.busy = Some(Busy::Refresh);
        }
    }

    /// Auto-refresh trigger: fires once `refresh_secs` have passed since the
    /// last completed refresh.
    fn maybe_auto_refresh(&mut self) {
        let Some(secs) = self.refresh_secs else {
            return;
        };
        if self.busy.is_some() || self.last_refresh.elapsed() < StdDuration::from_secs(secs) {
            return;
        }
        // Never pull the rows out from under an open popup: the form and
        // the confirmed plan both describe a row that is on screen now.
        if matches!(self.mode, Mode::Form | Mode::Confirm) {
            return;
        }
        self.request_refresh();
    }

    /// Non-blocking check for a finished request. `true` when a reply was
    /// folded in, which is what the tests wait on.
    fn poll_worker(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        match worker.replies.try_recv() {
            Ok(Reply::Refreshed(result)) => {
                self.apply_refresh(result);
                true
            }
            Ok(Reply::Mutated(mutated)) => {
                let Mutated { op, stage, result } = *mutated;
                self.apply_mutation_reply(op, stage, result);
                true
            }
            Err(TryRecvError::Empty) => false,
            // The worker died (panicked). Stop expecting replies.
            Err(TryRecvError::Disconnected) => {
                self.worker = None;
                self.busy = None;
                false
            }
        }
    }

    /// Fold one half of a mutation back into the UI.
    ///
    /// A plan failure with the form still open belongs *in* the form —
    /// the operator is one keystroke from fixing the field that caused
    /// it — while everything else becomes a header notice.
    fn apply_mutation_reply(&mut self, op: Op, stage: Stage, result: Result<MutationOutcome>) {
        self.busy = None;
        match (stage, result) {
            (Stage::Plan, Ok(outcome)) => {
                self.pending = Some(Pending {
                    op,
                    plan: Some(outcome),
                    scroll: 0,
                });
                self.form = None;
                self.mode = Mode::Confirm;
            }
            (Stage::Plan, Err(err)) => {
                let message = first_line(&err);
                match self.form.as_mut() {
                    Some(form) => {
                        form.locked = false;
                        form.error = Some(message);
                        self.mode = Mode::Form;
                    }
                    None => {
                        self.notice = Some(Notice {
                            text: message,
                            is_error: true,
                        });
                        self.pending = None;
                        self.mode = Mode::Normal;
                    }
                }
            }
            (Stage::Apply, Ok(outcome)) => {
                let id = outcome
                    .id
                    .clone()
                    .or_else(|| op.id().map(str::to_string))
                    .unwrap_or_default();
                let text = format!("{} {id}", past_tense(&op));
                self.notice = Some(Notice {
                    text: text.trim_end().to_string(),
                    is_error: false,
                });
                self.pending = None;
                self.form = None;
                self.mode = Mode::Normal;
                // The rows on screen describe the machine before the
                // write; ask for the ones that describe it after.
                self.request_refresh();
            }
            (Stage::Apply, Err(err)) => {
                self.notice = Some(Notice {
                    text: first_line(&err),
                    is_error: true,
                });
                self.pending = None;
                self.form = None;
                self.mode = Mode::Normal;
            }
        }
    }

    /// Fold a collection result into the view. A failure keeps the last good
    /// data and only records a message, so a flaky link never blanks the table
    /// or drops the user out of the TUI.
    fn apply_refresh(&mut self, result: Result<Vec<ScheduledTask>>) {
        self.busy = None;
        self.last_refresh = Instant::now();
        match result {
            Ok(tasks) => {
                self.refresh_error = None;
                self.all = tasks;
                self.available_sources = available_sources_of(&self.all);
                // A newly-discovered source would otherwise default to hidden
                // because the existing toggle set doesn't list it.
                if let Some(allowed) = &mut self.filter.allowed_sources {
                    for kind in &self.available_sources {
                        allowed.insert(*kind);
                    }
                }
                self.refilter();
            }
            Err(e) => self.refresh_error = Some(first_line(&e)),
        }
    }
}

/// Errors are shown in one line of header or one line of form, so only
/// the first line of a chain ever fits.
fn first_line(err: &anyhow::Error) -> String {
    let text = err.to_string();
    text.lines().next().unwrap_or("unknown error").to_string()
}

/// What the success notice calls the thing that just happened.
fn past_tense(op: &Op) -> &'static str {
    match op {
        Op::Create(_) => "added",
        Op::Update { .. } => "edited",
        Op::Delete { .. } => "removed",
        Op::SetEnabled { enabled: true, .. } => "enabled",
        Op::SetEnabled { enabled: false, .. } => "disabled",
    }
}

/// The confirm popup's body: what this plan writes, removes, and runs,
/// then whatever the writer wanted to say about it.
///
/// The diff is deliberately minimal — changed lines only, no context and
/// no hunk headers. A full unified diff is the CLI's job, where there is
/// a pager; here the popup has to fit over the table.
pub fn render_outcome(outcome: &MutationOutcome) -> Vec<String> {
    let mut lines = Vec::new();
    for change in &outcome.changes {
        match change {
            Change::WriteFile {
                path,
                before,
                after,
                ..
            } => {
                lines.push(format!("write {path}"));
                lines.extend(line_diff(before.as_deref().unwrap_or(""), after));
            }
            Change::RemoveFile { path, before, .. } => {
                lines.push(format!("remove {path}"));
                lines.extend(line_diff(before.as_deref().unwrap_or(""), ""));
            }
            Change::Command { cmd, .. } => lines.push(format!("run {cmd}")),
        }
    }
    if !outcome.notes.is_empty() {
        lines.push("notes:".to_string());
        for note in &outcome.notes {
            lines.push(format!("  {note}"));
        }
    }
    lines
}

/// Longest common subsequence over lines, capped so a pathological pair
/// of large files degrades to a set difference instead of allocating a
/// table nobody is going to read anyway.
const LCS_CELL_LIMIT: usize = 250_000;

fn line_diff(before: &str, after: &str) -> Vec<String> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len().saturating_mul(b.len()) > LCS_CELL_LIMIT {
        let mut out: Vec<String> = a
            .iter()
            .filter(|line| !b.contains(line))
            .map(|line| format!("-{line}"))
            .collect();
        out.extend(
            b.iter()
                .filter(|line| !a.contains(line))
                .map(|line| format!("+{line}")),
        );
        return out;
    }

    // lcs[i][j] = length of the longest common subsequence of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(format!("-{}", a[i]));
            i += 1;
        } else {
            out.push(format!("+{}", b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|line| format!("-{line}")));
    out.extend(b[j..].iter().map(|line| format!("+{line}")));
    out
}

fn available_sources_of(tasks: &[ScheduledTask]) -> Vec<TaskSourceKind> {
    let mut seen: Vec<TaskSourceKind> = Vec::new();
    for t in tasks {
        if !seen.contains(&t.source) {
            seen.push(t.source);
        }
    }
    // Always present sources in the same order, regardless of which
    // adapter happened to populate them first.
    let order = [
        TaskSourceKind::Systemd,
        TaskSourceKind::Cron,
        TaskSourceKind::At,
        TaskSourceKind::Anacron,
        TaskSourceKind::Launchd,
    ];
    order.iter().filter(|k| seen.contains(k)).copied().collect()
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, opts: RunOptions) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut app = App::new(opts);

    while !app.quit {
        app.poll_worker();
        app.maybe_auto_refresh();

        let now = Utc::now();
        terminal.draw(|frame| draw_app(frame, &mut app, now))?;

        // Poll briefly whenever a reply or an interval tick may be due, so the
        // header indicator and fresh data land without waiting on a keypress.
        let poll_for = if app.busy.is_some() || app.refresh_secs.is_some() {
            StdDuration::from_millis(100)
        } else {
            StdDuration::from_millis(500)
        };
        if !event::poll(poll_for)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        app.handle_key(key.code);
    }
    Ok(())
}

fn draw_app(frame: &mut Frame, app: &mut App, now: DateTime<Utc>) {
    let area = frame.area();

    // header / body / [optional bar] / footer
    let bar_present = matches!(app.mode, Mode::Filter | Mode::Search);
    let constraints: Vec<Constraint> = if bar_present {
        vec![
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ]
    };
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let header_area = v[0];
    let body_area = v[1];
    let (bar_area, footer_area) = if bar_present {
        (Some(v[2]), v[3])
    } else {
        (None, v[2])
    };

    draw_header(
        frame,
        header_area,
        app.all.len(),
        app.visible.len(),
        app.busy,
        app.refresh_error.as_deref(),
        app.notice.as_ref(),
    );
    // Pull table_state out by copy — render_stateful_widget needs it mutably
    // while the row builders below borrow other app fields.
    let mut table_state = app.table_state;
    draw_body(
        frame,
        body_area,
        &app.visible,
        app.detail_open,
        &mut table_state,
        now,
    );
    app.table_state = table_state;
    if let Some(area) = bar_area {
        match app.mode {
            Mode::Filter => draw_filter_bar(
                frame,
                area,
                &app.available_sources,
                &app.filter,
                app.filter_cursor,
            ),
            Mode::Search => draw_search_bar(frame, area, &app.filter.search),
            Mode::Normal | Mode::Form | Mode::Confirm => {}
        }
    }
    // Popups sit over the table, not beside it: the row being changed
    // stays visible around the edges.
    match app.mode {
        Mode::Form => {
            if let Some(form) = &app.form {
                draw_form(frame, body_area, form);
            }
        }
        Mode::Confirm => {
            if let Some(pending) = &app.pending {
                draw_confirm(frame, body_area, pending, app.dry_run);
            }
        }
        _ => {}
    }
    draw_footer(
        frame,
        footer_area,
        &app.filter,
        app.sort,
        &app.available_sources,
        app.worker.as_ref().is_some_and(|w| w.can_mutate),
    );
}

/// A centred box `width` by `height`, clamped to `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Popups are wide enough for a cron line plus a label, and never wider
/// than the body they cover.
fn popup_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).min(72)
}

/// Label column, wide enough for the longest field name plus a space.
const FORM_LABEL_WIDTH: u16 = 10;

fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
    let fields = form.visible_fields();
    // fields + hint + error, inside a border.
    let height = fields.len() as u16 + 4;
    let popup = centered(area, popup_width(area), height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", form.title()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor: Option<Position> = None;
    for (row, field) in fields.iter().enumerate() {
        let label = format!(
            "{:<width$}",
            field.label(),
            width = FORM_LABEL_WIDTH as usize
        );
        let focused = *field == form.focus;
        let label_style = if focused {
            Style::default().bold()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let mut spans = vec![Span::styled(label, label_style)];
        if field.is_picker() {
            spans.push(Span::raw(format!("◂ {} ▸", form.picker_value(*field))));
        } else if let Some(input) = form.input(*field) {
            if input.value.is_empty() && *field == Field::Schedule {
                spans.push(Span::styled(
                    form.schedule_hint().to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            } else {
                spans.push(Span::raw(input.value.clone()));
            }
            if focused {
                cursor = Some(Position::new(
                    inner.x + FORM_LABEL_WIDTH + input.cursor as u16,
                    inner.y + row as u16,
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    match &form.error {
        Some(error) => lines.push(Line::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )),
        None => lines.push(Line::styled(
            "Tab field · ◂ ▸ change · Enter plan · Esc cancel",
            Style::default().add_modifier(Modifier::DIM),
        )),
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    if let Some(position) = cursor {
        if !form.locked {
            frame.set_cursor_position(position);
        }
    }
}

/// The title of the confirm popup: exactly which job is about to change.
fn confirm_title(op: &Op) -> String {
    format!(
        "{} {} {}",
        op.verb(),
        op.source(),
        op.id().unwrap_or("(new)")
    )
}

fn draw_confirm(frame: &mut Frame, area: Rect, pending: &Pending, dry_run: bool) {
    let body: Vec<String> = pending
        .plan
        .as_ref()
        .map(render_outcome)
        .unwrap_or_else(|| vec!["planning…".to_string()]);
    let height = (body.len() as u16 + 4).min(area.height);
    let popup = centered(area, popup_width(area), height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", confirm_title(&pending.op)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = body
        .into_iter()
        .map(|text| {
            let style = match text.as_bytes().first() {
                Some(b'+') => Style::default().fg(Color::Green),
                Some(b'-') => Style::default().fg(Color::Red),
                _ => Style::default(),
            };
            Line::styled(text, style)
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if dry_run {
            "dry run · Esc close"
        } else {
            "y apply · n cancel"
        },
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((pending.scroll, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// Header body, without the surrounding padding. Split out so the
/// in-flight and failure indicators are testable without a terminal.
fn header_text(
    total: usize,
    shown: usize,
    busy: Option<Busy>,
    refresh_error: Option<&str>,
    notice: Option<&Notice>,
) -> String {
    let mut text = format!("ShuvJobs — {shown}/{total} task(s)");
    match busy {
        Some(Busy::Refresh) => text.push_str(" · refreshing…"),
        Some(Busy::Plan) => text.push_str(" · planning…"),
        Some(Busy::Apply) => text.push_str(" · applying…"),
        None => {}
    }
    if let Some(err) = refresh_error {
        text.push_str(&format!(" · refresh failed: {err}"));
    }
    if let Some(notice) = notice {
        if notice.is_error {
            text.push_str(&format!(" · error: {}", notice.text));
        } else {
            text.push_str(&format!(" · {}", notice.text));
        }
    }
    text
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    shown: usize,
    busy: Option<Busy>,
    refresh_error: Option<&str>,
    notice: Option<&Notice>,
) {
    let mut text = header_text(total, shown, busy, refresh_error, notice);
    text.insert(0, ' ');
    text.push(' ');
    let para = Paragraph::new(Line::from(text)).style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
    );
    frame.render_widget(para, area);
}

fn draw_body(
    frame: &mut Frame,
    area: Rect,
    visible: &[ScheduledTask],
    detail_open: bool,
    table_state: &mut TableState,
    now: DateTime<Utc>,
) {
    if detail_open && !visible.is_empty() {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        draw_table(frame, chunks[0], visible, table_state, now);
        let selected = table_state.selected().and_then(|i| visible.get(i));
        draw_detail(frame, chunks[1], selected, now);
    } else {
        draw_table(frame, area, visible, table_state, now);
    }
}

fn draw_table(
    frame: &mut Frame,
    area: Rect,
    visible: &[ScheduledTask],
    table_state: &mut TableState,
    now: DateTime<Utc>,
) {
    let header_row = Row::new(vec![
        Cell::from("SOURCE"),
        Cell::from("NAME"),
        Cell::from("SCHEDULE"),
        Cell::from("LAST RUN"),
        Cell::from("STATUS"),
        Cell::from("NEXT RUN"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD).underlined())
    .height(1);

    let rows: Vec<Row> = visible
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(format_source(t.source)),
                Cell::from(t.name.clone()),
                Cell::from(format_schedule(&t.schedule)),
                Cell::from(format_optional_dt(t.last_run, now)),
                Cell::from(status_span(t.last_status.as_ref())),
                Cell::from(format_optional_dt(t.next_run, now)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Percentage(25),
        Constraint::Percentage(30),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" tasks "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(table, area, table_state);
}

fn draw_detail(frame: &mut Frame, area: Rect, task: Option<&ScheduledTask>, now: DateTime<Utc>) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let inner_text: Vec<Line> = match task {
        None => vec![Line::from("(no selection)")],
        Some(t) => format_detail(t, now),
    };
    let para = Paragraph::new(inner_text)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn format_detail(t: &ScheduledTask, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv("Name", &t.name),
        // The id is what `e`, `d`, and every CLI subcommand address the
        // job by, so it belongs where the operator can read it off.
        kv("Id", &t.id),
        kv("Source", format_source(t.source)),
        kv("Command", &t.command),
        kv("Schedule", &format_schedule(&t.schedule)),
        kv("Last run", &format_dt_with_relative(t.last_run, now)),
        kv("Next run", &format_dt_with_relative(t.next_run, now)),
        kv("Status", &format_status_long(t.last_status.as_ref())),
        kv(
            "Enabled",
            match t.enabled {
                Some(true) => "yes",
                Some(false) => "no",
                None => "-",
            },
        ),
    ];
    if let Some(location) = &t.location {
        lines.push(kv("Location", location));
    }
    if let Some(d) = t.last_duration {
        lines.push(kv("Duration", &format!("{:.2}s", d.as_secs_f64())));
    }
    lines
}

fn kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().bold()),
        Span::raw(value.to_string()),
    ])
}

/// Absolute timestamps are rendered in `tz` so the pane matches the wall clock
/// the user schedules against; the relative half stays zone-independent.
fn format_dt_with_relative_in<Tz>(dt: Option<DateTime<Utc>>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    match dt {
        None => "-".into(),
        Some(dt) => format!(
            "{} ({})",
            format_relative_in(dt, now, tz),
            dt.with_timezone(tz).format(ABSOLUTE_FMT)
        ),
    }
}

fn format_dt_with_relative(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    format_dt_with_relative_in(dt, now, &Local)
}

fn format_status_long(s: Option<&TaskStatus>) -> String {
    match s {
        Some(TaskStatus::Success) => "ok".into(),
        Some(TaskStatus::Failed(msg)) if !msg.is_empty() => format!("failed ({msg})"),
        Some(TaskStatus::Failed(_)) => "failed".into(),
        Some(TaskStatus::Running) => "running".into(),
        None => "-".into(),
    }
}

fn draw_filter_bar(
    frame: &mut Frame,
    area: Rect,
    available: &[TaskSourceKind],
    filter: &Filter,
    cursor: usize,
) {
    let allowed = filter.allowed_sources.as_ref();
    let mut spans: Vec<Span> = vec![Span::raw(" filter ▸ ")];
    for (i, kind) in available.iter().enumerate() {
        let is_on = allowed.map(|a| a.contains(kind)).unwrap_or(true);
        let mark = if is_on { "[x]" } else { "[ ]" };
        let label = format!("{mark} {} ", kind.as_str());
        let style = if i == cursor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    spans.push(Span::raw(" · space toggle · esc close "));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_search_bar(frame: &mut Frame, area: Rect, query: &str) {
    let line = Line::from(vec![
        Span::styled(" search: ", Style::default().bold()),
        Span::raw(query.to_string()),
        Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        Span::raw("   esc clear · enter commit "),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    filter: &Filter,
    sort: SortMode,
    available: &[TaskSourceKind],
    can_mutate: bool,
) {
    let mut state_parts: Vec<String> = Vec::new();
    if let Some(allowed) = &filter.allowed_sources {
        let active: Vec<&str> = available
            .iter()
            .filter(|k| allowed.contains(k))
            .map(|k| k.as_str())
            .collect();
        if active.len() != available.len() {
            state_parts.push(format!("filter: {}", active.join(", ")));
        }
    }
    if !filter.search.is_empty() {
        state_parts.push(format!("search: {}", filter.search));
    }
    if sort != SortMode::Default {
        state_parts.push(format!("sort: {}", sort.label()));
    }
    let left = if state_parts.is_empty() {
        String::new()
    } else {
        format!(" {} ", state_parts.join(" · "))
    };

    // Without a mutate handle the action keys do nothing, so promising
    // them in the footer would be a lie.
    let (full, narrow) = if can_mutate {
        (
            "a add · e edit · d delete · t toggle · / search · f filter · s sort · r refresh · Enter detail · q quit ",
            "a e d t / f s r q ",
        )
    } else {
        (
            "/ search · f filter · s sort · r refresh · Enter detail · q quit ",
            "/ · f · s · r · q ",
        )
    };
    let width = area.width as usize;
    let right = if width >= left.chars().count() + full.chars().count() + 3 {
        full
    } else {
        narrow
    };

    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = Line::from(vec![
        Span::styled(left, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(
            right.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn format_source(kind: TaskSourceKind) -> &'static str {
    kind.as_str()
}

fn format_schedule_in<Tz>(s: &ScheduleType, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    match s {
        ScheduleType::Cron(expr) => expr.clone(),
        ScheduleType::Calendar(expr) if expr.is_empty() => "(unknown)".into(),
        ScheduleType::Calendar(expr) => expr.clone(),
        ScheduleType::Interval(d) => format!("every {}", human_duration(*d)),
        ScheduleType::OneShot(dt) => dt.with_timezone(tz).format(ONE_SHOT_FMT).to_string(),
    }
}

fn format_schedule(s: &ScheduleType) -> String {
    format_schedule_in(s, &Local)
}

fn format_status(s: Option<&TaskStatus>) -> &'static str {
    match s {
        Some(TaskStatus::Success) => "ok",
        Some(TaskStatus::Failed(_)) => "fail",
        Some(TaskStatus::Running) => "run",
        None => "-",
    }
}

fn status_span(s: Option<&TaskStatus>) -> Span<'static> {
    let style = match s {
        Some(TaskStatus::Success) => Style::default().fg(Color::Green),
        Some(TaskStatus::Failed(_)) => Style::default().fg(Color::Red),
        Some(TaskStatus::Running) => Style::default().fg(Color::Yellow),
        None => Style::default(),
    };
    Span::styled(format_status(s), style)
}

fn format_optional_dt_in<Tz>(dt: Option<DateTime<Utc>>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    match dt {
        Some(dt) => format_relative_in(dt, now, tz),
        None => "-".to_string(),
    }
}

fn format_optional_dt(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    format_optional_dt_in(dt, now, &Local)
}

/// Only the far-future/far-past fallback is zone-sensitive: it degrades to a
/// calendar date, which has to be the date in `tz`.
fn format_relative_in<Tz>(dt: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let delta = dt.signed_duration_since(now);
    let abs = delta.num_seconds().unsigned_abs();
    let in_future = delta.num_seconds() >= 0;

    if abs >= 30 * 86_400 {
        return dt.with_timezone(tz).format(DATE_FMT).to_string();
    }

    let label = if abs < 60 {
        format!("{abs}s")
    } else if abs < 3_600 {
        format!("{}m", abs / 60)
    } else if abs < 86_400 {
        format!("{}h", abs / 3_600)
    } else {
        format!("{}d", abs / 86_400)
    };

    if in_future {
        format!("in {label}")
    } else {
        format!("{label} ago")
    }
}

fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs.is_multiple_of(86_400) && secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) && secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) && secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    /// UTC+02:00 — deterministic stand-in for `Local` in formatting tests.
    fn plus_two() -> FixedOffset {
        FixedOffset::east_opt(2 * 3_600).unwrap()
    }

    /// UTC-08:00, far enough west to push a late-evening UTC stamp onto the
    /// previous calendar day.
    fn minus_eight() -> FixedOffset {
        FixedOffset::west_opt(8 * 3_600).unwrap()
    }

    fn task(name: &str, source: TaskSourceKind) -> ScheduledTask {
        ScheduledTask {
            id: name.into(),
            name: name.into(),
            source,
            schedule: ScheduleType::Cron("* * * * *".into()),
            last_run: None,
            last_status: None,
            last_duration: None,
            next_run: None,
            command: String::new(),
            location: None,
            enabled: None,
        }
    }

    #[test]
    fn relative_times_scale_by_magnitude() {
        let now = at(1_000_000);
        let tz = plus_two();
        assert_eq!(format_relative_in(at(1_000_030), now, &tz), "in 30s");
        assert_eq!(format_relative_in(at(1_000_000 - 90), now, &tz), "1m ago");
        assert_eq!(format_relative_in(at(1_000_000 + 7_200), now, &tz), "in 2h");
        assert_eq!(
            format_relative_in(at(1_000_000 - 3 * 86_400), now, &tz),
            "3d ago"
        );
        assert_eq!(format_relative_in(now, now, &tz), "in 0s");
        assert_eq!(
            format_relative_in(at(1_000_030), now, &minus_eight()),
            "in 30s",
            "relative strings do not depend on the zone"
        );
    }

    #[test]
    fn relative_times_beyond_a_month_show_the_date() {
        let now = at(1_000_000);
        let far = now + chrono::Duration::days(45);
        assert_eq!(
            format_relative_in(far, now, &Utc),
            far.format("%Y-%m-%d").to_string()
        );
    }

    #[test]
    fn far_future_date_fallback_uses_the_target_zone() {
        // 2026-04-14 04:00 UTC is still 2026-04-13 in UTC-08:00.
        let far = at(1_776_139_200);
        let now = far - chrono::Duration::days(45);
        assert_eq!(format_relative_in(far, now, &Utc), "2026-04-14");
        assert_eq!(format_relative_in(far, now, &minus_eight()), "2026-04-13");
    }

    #[test]
    fn absolute_timestamps_render_in_the_target_zone() {
        let now = at(1_776_175_200);
        let last = Some(at(1_776_175_200 - 3_600));
        assert_eq!(
            format_dt_with_relative_in(last, now, &Utc),
            "1h ago (2026-04-14 13:00:00 UTC)"
        );
        assert_eq!(
            format_dt_with_relative_in(last, now, &plus_two()),
            "1h ago (2026-04-14 15:00:00 +02:00)"
        );
        assert_eq!(
            format_dt_with_relative_in(last, now, &minus_eight()),
            "1h ago (2026-04-14 05:00:00 -08:00)"
        );
        assert_eq!(format_dt_with_relative_in(None, now, &plus_two()), "-");
    }

    #[test]
    fn one_shot_schedule_renders_in_the_target_zone() {
        let one_shot = ScheduleType::OneShot(at(1_776_175_200));
        assert_eq!(format_schedule_in(&one_shot, &Utc), "2026-04-14 14:00 UTC");
        assert_eq!(
            format_schedule_in(&one_shot, &minus_eight()),
            "2026-04-14 06:00 -08:00"
        );
    }

    #[test]
    fn optional_datetime_renders_dash_for_none() {
        assert_eq!(format_optional_dt_in(None, at(0), &plus_two()), "-");
    }

    #[test]
    fn human_duration_picks_largest_exact_unit() {
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(900)), "15m");
        assert_eq!(human_duration(Duration::from_secs(5_400)), "90m");
        assert_eq!(human_duration(Duration::from_secs(7_200)), "2h");
        assert_eq!(human_duration(Duration::from_secs(172_800)), "2d");
    }

    #[test]
    fn schedule_formatting_covers_every_variant() {
        assert_eq!(
            format_schedule_in(&ScheduleType::Cron("0 2 * * *".into()), &Utc),
            "0 2 * * *"
        );
        assert_eq!(
            format_schedule_in(&ScheduleType::Calendar(String::new()), &Utc),
            "(unknown)"
        );
        assert_eq!(
            format_schedule_in(&ScheduleType::Calendar("daily".into()), &Utc),
            "daily"
        );
        assert_eq!(
            format_schedule_in(&ScheduleType::Interval(Duration::from_secs(3_600)), &Utc),
            "every 1h"
        );
        assert_eq!(
            format_schedule_in(&ScheduleType::OneShot(at(1_776_175_200)), &Utc),
            "2026-04-14 14:00 UTC"
        );
    }

    #[test]
    fn status_glyphs_and_long_form() {
        assert_eq!(format_status(None), "-");
        assert_eq!(format_status(Some(&TaskStatus::Success)), "ok");
        assert_eq!(format_status(Some(&TaskStatus::Running)), "run");
        assert_eq!(
            format_status_long(Some(&TaskStatus::Failed("exit-code".into()))),
            "failed (exit-code)"
        );
        assert_eq!(
            format_status_long(Some(&TaskStatus::Failed(String::new()))),
            "failed"
        );
    }

    #[test]
    fn available_sources_are_deduped_in_canonical_order() {
        let tasks = vec![
            task("a", TaskSourceKind::Launchd),
            task("b", TaskSourceKind::Cron),
            task("c", TaskSourceKind::Cron),
            task("d", TaskSourceKind::Systemd),
        ];
        assert_eq!(
            available_sources_of(&tasks),
            vec![
                TaskSourceKind::Systemd,
                TaskSourceKind::Cron,
                TaskSourceKind::Launchd
            ]
        );
        assert!(available_sources_of(&[]).is_empty());
    }

    fn app_with(tasks: Vec<ScheduledTask>) -> App {
        App::new(RunOptions {
            initial: tasks,
            refresh: None,
            mutate: None,
            refresh_secs: None,
            dry_run: false,
        })
    }

    #[test]
    fn selection_clamps_and_search_filters() {
        let mut app = app_with(vec![
            task("alpha", TaskSourceKind::Cron),
            task("bravo", TaskSourceKind::Cron),
            task("charlie", TaskSourceKind::Systemd),
        ]);
        assert_eq!(app.visible.len(), 3);
        assert_eq!(app.table_state.selected(), Some(0));

        app.handle_key(KeyCode::End);
        assert_eq!(app.table_state.selected(), Some(2));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(
            app.table_state.selected(),
            Some(2),
            "clamped at the last row"
        );
        app.handle_key(KeyCode::PageUp);
        assert_eq!(app.table_state.selected(), Some(0));

        app.handle_key(KeyCode::Char('/'));
        for c in "brav".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.visible[0].name, "bravo");
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter.search, "brav", "enter keeps the query");

        app.handle_key(KeyCode::Char('/'));
        app.handle_key(KeyCode::Esc);
        assert!(app.filter.search.is_empty(), "esc clears the query");
        assert_eq!(app.visible.len(), 3);
    }

    #[test]
    fn filter_bar_toggles_sources_and_detail_needs_a_row() {
        let mut app = app_with(vec![
            task("alpha", TaskSourceKind::Systemd),
            task("bravo", TaskSourceKind::Cron),
        ]);
        app.handle_key(KeyCode::Char('f'));
        assert_eq!(app.mode, Mode::Filter);
        // cursor 0 = systemd (canonical order); toggle it off.
        app.handle_key(KeyCode::Char(' '));
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.visible[0].source, TaskSourceKind::Cron);
        app.handle_key(KeyCode::Right);
        app.handle_key(KeyCode::Char(' '));
        assert!(app.visible.is_empty());
        app.handle_key(KeyCode::Esc);

        app.handle_key(KeyCode::Enter);
        assert!(!app.detail_open, "no rows visible, so no detail pane");
        app.handle_key(KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn sort_key_cycles_and_footer_reflects_it() {
        let mut app = app_with(vec![task("a", TaskSourceKind::Cron)]);
        app.handle_key(KeyCode::Char('s'));
        assert_eq!(app.sort, SortMode::NextRun);
    }

    #[test]
    fn refresh_key_without_a_collector_is_inert() {
        let mut app = app_with(vec![task("a", TaskSourceKind::Cron)]);
        app.handle_key(KeyCode::Char('r'));
        assert!(app.busy.is_none(), "no worker, nothing to wait for");
    }

    #[test]
    fn refresh_key_starts_one_collection_and_ignores_the_second() {
        // The fake collector announces each call and then parks, so the
        // "one in flight at a time" rule is observable without timing luck.
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mut app = App::new(RunOptions {
            initial: vec![task("alpha", TaskSourceKind::Cron)],
            refresh: Some(Box::new(move || {
                started_tx.send(()).ok();
                release_rx.recv().ok();
                Ok(vec![task("alpha", TaskSourceKind::Cron)])
            })),
            mutate: None,
            refresh_secs: None,
            dry_run: false,
        });

        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.busy, Some(Busy::Refresh));
        started_rx
            .recv_timeout(StdDuration::from_secs(5))
            .expect("worker picked the request up");

        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.busy, Some(Busy::Refresh));
        app.poll_worker();
        assert_eq!(
            app.busy,
            Some(Busy::Refresh),
            "no reply yet, still in flight"
        );
        assert_eq!(
            started_rx.try_recv(),
            Err(TryRecvError::Empty),
            "the second request was dropped, not queued"
        );

        release_tx.send(()).unwrap();
        // The reply may not have landed on the very first poll.
        for _ in 0..100 {
            app.poll_worker();
            if app.busy.is_none() {
                break;
            }
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(app.busy.is_none(), "reply applied");
        assert!(app.refresh_error.is_none());
    }

    #[test]
    fn successful_refresh_replaces_tasks_and_clamps_the_selection() {
        let mut app = app_with(vec![
            task("alpha", TaskSourceKind::Cron),
            task("bravo", TaskSourceKind::Cron),
            task("charlie", TaskSourceKind::Cron),
        ]);
        app.handle_key(KeyCode::End);
        assert_eq!(app.table_state.selected(), Some(2));
        app.refresh_error = Some("stale".into());
        app.busy = Some(Busy::Refresh);

        app.apply_refresh(Ok(vec![
            task("delta", TaskSourceKind::Cron),
            task("echo", TaskSourceKind::Systemd),
        ]));

        assert!(app.busy.is_none());
        assert!(app.refresh_error.is_none(), "a success clears the error");
        assert_eq!(app.visible.len(), 2);
        assert_eq!(
            app.table_state.selected(),
            Some(1),
            "clamped to the new len"
        );
        assert_eq!(
            app.available_sources,
            vec![TaskSourceKind::Systemd, TaskSourceKind::Cron]
        );
        assert!(
            app.visible
                .iter()
                .any(|t| t.source == TaskSourceKind::Systemd),
            "a source discovered by the refresh is visible, not hidden"
        );
    }

    #[test]
    fn failed_refresh_keeps_the_last_good_data() {
        let mut app = app_with(vec![task("alpha", TaskSourceKind::Cron)]);
        app.busy = Some(Busy::Refresh);

        app.apply_refresh(Err(anyhow::anyhow!("ssh: connect failed\nsecond line")));

        assert!(app.busy.is_none());
        assert_eq!(app.refresh_error.as_deref(), Some("ssh: connect failed"));
        assert_eq!(app.visible.len(), 1, "old rows survive a failure");
        assert_eq!(app.visible[0].name, "alpha");
        assert!(!app.quit, "a refresh failure never leaves the TUI");

        app.apply_refresh(Ok(vec![task("bravo", TaskSourceKind::Cron)]));
        assert!(app.refresh_error.is_none());
    }

    #[test]
    fn header_shows_in_flight_and_failure_state() {
        assert_eq!(
            header_text(3, 2, None, None, None),
            "ShuvJobs — 2/3 task(s)"
        );
        assert_eq!(
            header_text(3, 3, Some(Busy::Refresh), None, None),
            "ShuvJobs — 3/3 task(s) · refreshing…"
        );
        assert_eq!(
            header_text(3, 3, Some(Busy::Refresh), Some("ssh: connect failed"), None),
            "ShuvJobs — 3/3 task(s) · refreshing… · refresh failed: ssh: connect failed"
        );
        assert_eq!(
            header_text(3, 3, Some(Busy::Plan), None, None),
            "ShuvJobs — 3/3 task(s) · planning…"
        );
        assert_eq!(
            header_text(
                3,
                3,
                Some(Busy::Apply),
                None,
                Some(&Notice {
                    text: "added user:alice:4".into(),
                    is_error: false
                })
            ),
            "ShuvJobs — 3/3 task(s) · applying… · added user:alice:4"
        );
        assert_eq!(
            header_text(
                3,
                3,
                None,
                None,
                Some(&Notice {
                    text: "needs root".into(),
                    is_error: true
                })
            ),
            "ShuvJobs — 3/3 task(s) · error: needs root"
        );
    }
    // ---- mutation flow -------------------------------------------------

    use crate::form::Intent;
    use shuvjobs_core::host::Privilege;
    use shuvjobs_core::manage::{JobScope, MutationOutcome};

    /// A fake mutate handle: it announces every `(op, stage)` it is asked
    /// for on a channel and answers from a scripted list, so a whole
    /// plan/confirm/apply round trip is deterministic.
    fn app_with_mutator(
        tasks: Vec<ScheduledTask>,
        replies: Vec<Result<MutationOutcome>>,
    ) -> (App, Receiver<(Op, Stage)>) {
        app_with_mutator_opts(tasks, replies, false)
    }

    fn app_with_mutator_opts(
        tasks: Vec<ScheduledTask>,
        replies: Vec<Result<MutationOutcome>>,
        dry_run: bool,
    ) -> (App, Receiver<(Op, Stage)>) {
        let (calls_tx, calls_rx) = mpsc::channel::<(Op, Stage)>();
        let mut replies = replies.into_iter();
        let app = App::new(RunOptions {
            initial: tasks,
            refresh: Some(Box::new(|| Ok(Vec::new()))),
            mutate: Some(Box::new(move |op, stage| {
                calls_tx.send((op, stage)).ok();
                replies
                    .next()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("no scripted reply")))
            })),
            refresh_secs: None,
            dry_run,
        });
        (app, calls_rx)
    }

    /// Wait for exactly one worker reply to be folded in.
    fn pump(app: &mut App) {
        for _ in 0..500 {
            if app.poll_worker() {
                return;
            }
            thread::sleep(StdDuration::from_millis(10));
        }
        panic!("no reply from the worker");
    }

    fn next_call(calls: &Receiver<(Op, Stage)>) -> (Op, Stage) {
        calls
            .recv_timeout(StdDuration::from_secs(5))
            .expect("the worker was asked for something")
    }

    fn plan_outcome() -> MutationOutcome {
        MutationOutcome {
            id: Some("user:alice:4".into()),
            changes: vec![Change::WriteFile {
                path: "/etc/cron.d/x".into(),
                before: Some("old\n".into()),
                after: "new\n".into(),
                mode: 0o644,
                privilege: Privilege::Root,
            }],
            applied: false,
            outputs: Vec::new(),
            notes: vec!["needs a reload".into()],
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyCode::Char(c));
        }
    }

    /// Fill a fresh cron add form: Source, Scope, Schedule, Command, Enabled.
    fn fill_cron_form(app: &mut App) {
        app.handle_key(KeyCode::Tab);
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.form.as_ref().unwrap().focus, Field::Schedule);
        type_text(app, "*/5 * * * *");
        app.handle_key(KeyCode::Tab);
        type_text(app, "echo hi");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn mutation_keys_are_inert_without_a_handle() {
        let mut app = app_with(vec![task("alpha", TaskSourceKind::Cron)]);
        for code in ['a', 'e', 'd', 't'] {
            app.handle_key(KeyCode::Char(code));
            assert_eq!(app.mode, Mode::Normal, "`{code}` did something");
        }
        assert!(app.form.is_none());
        assert!(app.pending.is_none());
        assert!(app.busy.is_none());
    }

    #[test]
    fn add_opens_a_form_focused_on_the_source_picker() {
        let (mut app, calls) = app_with_mutator(vec![task("alpha", TaskSourceKind::Cron)], vec![]);
        app.handle_key(KeyCode::Char('a'));
        assert_eq!(app.mode, Mode::Form);
        let form = app.form.as_ref().expect("form");
        assert_eq!(form.intent, Intent::Add);
        assert_eq!(form.focus, Field::Source);
        assert_eq!(form.source, TaskSourceKind::Cron, "the only source present");
        assert!(form.command.value.is_empty());
        assert_eq!(
            calls.try_recv(),
            Err(TryRecvError::Empty),
            "opening a form asks the host nothing"
        );
    }

    #[test]
    fn edit_prefills_from_the_row_and_hides_the_source() {
        let mut row = task("backup", TaskSourceKind::Cron);
        row.id = "user:alice:4".into();
        row.command = "echo hi".into();
        row.schedule = ScheduleType::Cron("0 9 * * *".into());
        let (mut app, _calls) = app_with_mutator(vec![row], vec![]);

        app.handle_key(KeyCode::Char('e'));
        assert_eq!(app.mode, Mode::Form);
        let form = app.form.as_ref().expect("form");
        assert_eq!(
            form.intent,
            Intent::Edit {
                id: "user:alice:4".into()
            }
        );
        assert_eq!(form.schedule.value, "0 9 * * *");
        assert_eq!(form.command.value, "echo hi");
        assert_eq!(form.scope, JobScope::User, "`user:` is this user's crontab");
        assert!(!form.visible_fields().contains(&Field::Source));
        assert_eq!(form.focus, Field::Scope);
    }

    #[test]
    fn tab_backtab_and_text_keys_edit_the_focused_field() {
        let (mut app, _calls) = app_with_mutator(vec![task("a", TaskSourceKind::Cron)], vec![]);
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.form.as_ref().unwrap().focus, Field::Scope);
        app.handle_key(KeyCode::Tab);
        type_text(&mut app, "@daily");
        app.handle_key(KeyCode::Backspace);
        assert_eq!(app.form.as_ref().unwrap().schedule.value, "@dail");
        app.handle_key(KeyCode::BackTab);
        assert_eq!(app.form.as_ref().unwrap().focus, Field::Scope);
        app.handle_key(KeyCode::BackTab);
        assert_eq!(app.form.as_ref().unwrap().focus, Field::Source);
    }

    #[test]
    fn esc_closes_the_form_without_asking_the_host() {
        let (mut app, calls) = app_with_mutator(vec![task("a", TaskSourceKind::Cron)], vec![]);
        app.handle_key(KeyCode::Char('a'));
        fill_cron_form(&mut app);
        app.handle_key(KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.form.is_none());
        assert!(app.busy.is_none());
        assert_eq!(calls.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn an_empty_command_is_reported_inline() {
        let (mut app, calls) = app_with_mutator(vec![task("a", TaskSourceKind::Cron)], vec![]);
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Tab);
        app.handle_key(KeyCode::Tab);
        type_text(&mut app, "*/5 * * * *");
        app.handle_key(KeyCode::Enter);

        assert_eq!(app.mode, Mode::Form, "a bad form stays open");
        let form = app.form.as_ref().expect("form");
        assert_eq!(form.error.as_deref(), Some("command must not be empty"));
        assert!(!form.locked, "nothing is in flight, so keep typing");
        assert!(app.busy.is_none());
        assert_eq!(calls.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn submitting_the_form_plans_and_opens_the_confirm() {
        let (mut app, calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome())],
        );
        app.handle_key(KeyCode::Char('a'));
        fill_cron_form(&mut app);
        app.handle_key(KeyCode::Enter);

        assert_eq!(app.busy, Some(Busy::Plan));
        assert!(app.form.as_ref().unwrap().locked, "the spec cannot drift");
        let (op, stage) = next_call(&calls);
        assert_eq!(stage, Stage::Plan);
        match &op {
            Op::Create(spec) => {
                assert_eq!(spec.command, "echo hi");
                assert_eq!(spec.schedule, ScheduleType::Cron("*/5 * * * *".into()));
            }
            other => panic!("expected a create, got {other:?}"),
        }

        pump(&mut app);
        assert_eq!(app.mode, Mode::Confirm);
        assert!(app.busy.is_none());
        assert!(app.form.is_none(), "the form is replaced by the plan");
        assert_eq!(app.pending.as_ref().unwrap().plan, Some(plan_outcome()));
    }

    #[test]
    fn y_applies_and_then_asks_for_a_refresh() {
        let applied = MutationOutcome {
            applied: true,
            ..plan_outcome()
        };
        let (mut app, calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome()), Ok(applied)],
        );
        app.handle_key(KeyCode::Char('d'));
        pump(&mut app);
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(next_call(&calls).1, Stage::Plan);

        app.handle_key(KeyCode::Char('y'));
        assert_eq!(app.busy, Some(Busy::Apply));
        assert_eq!(next_call(&calls).1, Stage::Apply);
        pump(&mut app);

        assert_eq!(app.mode, Mode::Normal);
        let notice = app.notice.as_ref().expect("notice");
        assert_eq!(notice.text, "removed user:alice:4");
        assert!(!notice.is_error);
        assert!(app.pending.is_none());
        assert_eq!(
            app.busy,
            Some(Busy::Refresh),
            "the rows on screen are now stale"
        );
    }

    #[test]
    fn n_cancels_a_confirmed_plan() {
        let (mut app, calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome())],
        );
        app.handle_key(KeyCode::Char('d'));
        pump(&mut app);
        assert_eq!(app.mode, Mode::Confirm);
        next_call(&calls);

        app.handle_key(KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending.is_none());
        assert!(app.busy.is_none());
        assert_eq!(calls.try_recv(), Err(TryRecvError::Empty), "no apply");
    }

    #[test]
    fn a_dry_run_confirm_never_applies() {
        let (mut app, calls) = app_with_mutator_opts(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome())],
            true,
        );
        app.handle_key(KeyCode::Char('d'));
        pump(&mut app);
        next_call(&calls);

        app.handle_key(KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.notice.as_ref().unwrap().text, "dry run: not applied");
        assert!(app.busy.is_none());
        assert_eq!(calls.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn d_plans_a_delete_of_the_selected_row() {
        let mut row = task("backup", TaskSourceKind::Cron);
        row.id = "user:alice:4".into();
        let (mut app, calls) = app_with_mutator(vec![row], vec![Ok(plan_outcome())]);
        app.handle_key(KeyCode::Char('d'));
        assert_eq!(
            next_call(&calls),
            (
                Op::Delete {
                    id: "user:alice:4".into(),
                    source: TaskSourceKind::Cron
                },
                Stage::Plan
            )
        );
    }

    #[test]
    fn t_flips_the_enabled_flag_of_the_selected_row() {
        let mut off = task("backup", TaskSourceKind::Systemd);
        off.id = "user/backup.timer".into();
        off.enabled = Some(false);
        let (mut app, calls) = app_with_mutator(vec![off], vec![Ok(plan_outcome())]);
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(
            next_call(&calls),
            (
                Op::SetEnabled {
                    id: "user/backup.timer".into(),
                    source: TaskSourceKind::Systemd,
                    enabled: true
                },
                Stage::Plan
            )
        );

        // An unknown state is treated as on, so `t` turns it off.
        let (mut app, calls) = app_with_mutator(
            vec![task("alpha", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome())],
        );
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(
            next_call(&calls).0,
            Op::SetEnabled {
                id: "alpha".into(),
                source: TaskSourceKind::Cron,
                enabled: false
            }
        );
    }

    #[test]
    fn a_plan_failure_returns_to_the_open_form() {
        let (mut app, _calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Err(anyhow::anyhow!("cron: bad expression\ndetail"))],
        );
        app.handle_key(KeyCode::Char('a'));
        fill_cron_form(&mut app);
        app.handle_key(KeyCode::Enter);
        pump(&mut app);

        assert_eq!(app.mode, Mode::Form);
        let form = app.form.as_ref().expect("form still open");
        assert_eq!(form.error.as_deref(), Some("cron: bad expression"));
        assert!(!form.locked, "the operator can fix the field");
        assert_eq!(form.command.value, "echo hi", "what was typed survives");
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_plan_failure_without_a_form_becomes_a_notice() {
        let (mut app, _calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Err(anyhow::anyhow!("needs root: pass --sudo"))],
        );
        app.handle_key(KeyCode::Char('d'));
        pump(&mut app);
        assert_eq!(app.mode, Mode::Normal);
        let notice = app.notice.as_ref().expect("notice");
        assert!(notice.is_error);
        assert_eq!(notice.text, "needs root: pass --sudo");
    }

    #[test]
    fn an_apply_failure_keeps_the_rows_and_reports_it() {
        let (mut app, _calls) = app_with_mutator(
            vec![task("alpha", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome()), Err(anyhow::anyhow!("write failed"))],
        );
        app.handle_key(KeyCode::Char('d'));
        pump(&mut app);
        app.handle_key(KeyCode::Char('y'));
        pump(&mut app);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.notice.as_ref().unwrap().is_error);
        assert_eq!(app.notice.as_ref().unwrap().text, "write failed");
        assert_eq!(app.visible.len(), 1, "the table is untouched");
        assert_eq!(app.visible[0].name, "alpha");
        assert!(app.busy.is_none(), "a failure asks for no refresh");
    }

    #[test]
    fn mutation_keys_are_ignored_while_a_request_is_in_flight() {
        let (mut app, calls) = app_with_mutator(
            vec![task("a", TaskSourceKind::Cron)],
            vec![Ok(plan_outcome())],
        );
        app.handle_key(KeyCode::Char('d'));
        assert_eq!(app.busy, Some(Busy::Plan));

        for code in ['a', 'e', 'd', 't', 'r'] {
            app.handle_key(KeyCode::Char(code));
        }
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.form.is_none());
        assert_eq!(app.busy, Some(Busy::Plan), "still the same request");
        next_call(&calls);
        assert_eq!(
            calls.try_recv(),
            Err(TryRecvError::Empty),
            "nothing was queued behind it"
        );
    }

    #[test]
    fn the_detail_pane_lists_id_enabled_and_location() {
        let mut row = task("backup", TaskSourceKind::Systemd);
        row.id = "user/backup.timer".into();
        row.enabled = Some(false);
        row.location = Some("/home/alice/.config/systemd/user/backup.timer".into());
        let rendered: Vec<String> = format_detail(&row, at(0)).iter().map(line_text).collect();
        assert!(rendered.iter().any(|l| l.contains("user/backup.timer")));
        assert!(rendered
            .iter()
            .any(|l| l.starts_with("Enabled") && l.ends_with("no")));
        assert!(rendered.iter().any(|l| l.starts_with("Location")
            && l.contains("/home/alice/.config/systemd/user/backup.timer")));

        let plain = task("alpha", TaskSourceKind::Cron);
        let rendered: Vec<String> = format_detail(&plain, at(0)).iter().map(line_text).collect();
        assert!(rendered
            .iter()
            .any(|l| l.starts_with("Enabled") && l.ends_with('-')));
        assert!(
            !rendered.iter().any(|l| l.starts_with("Location")),
            "no backing file, no row"
        );
    }

    #[test]
    fn the_confirm_body_shows_the_changed_lines_and_the_notes() {
        let outcome = MutationOutcome {
            id: None,
            changes: vec![
                Change::WriteFile {
                    path: "/etc/cron.d/x".into(),
                    before: Some("keep\ndrop\n".into()),
                    after: "keep\nadd\n".into(),
                    mode: 0o644,
                    privilege: Privilege::Root,
                },
                Change::RemoveFile {
                    path: "/etc/cron.d/y".into(),
                    before: Some("gone\n".into()),
                    privilege: Privilege::Root,
                },
                Change::Command {
                    cmd: "systemctl daemon-reload".into(),
                    stdin: None,
                    privilege: Privilege::Root,
                    description: "reload".into(),
                    on_fail: shuvjobs_core::FailPolicy::Error,
                },
            ],
            applied: false,
            outputs: Vec::new(),
            notes: vec!["line moved".into()],
        };
        assert_eq!(
            render_outcome(&outcome),
            vec![
                "write /etc/cron.d/x",
                "-drop",
                "+add",
                "remove /etc/cron.d/y",
                "-gone",
                "run systemctl daemon-reload",
                "notes:",
                "  line moved",
            ]
        );
        assert!(render_outcome(&MutationOutcome::default()).is_empty());
    }

    #[test]
    fn the_confirm_title_names_the_job() {
        assert_eq!(
            confirm_title(&Op::Delete {
                id: "user:alice:4".into(),
                source: TaskSourceKind::Cron
            }),
            "rm cron user:alice:4"
        );
        assert_eq!(
            confirm_title(&Op::Create(shuvjobs_core::JobSpec::new(
                TaskSourceKind::At,
                ScheduleType::Calendar("now + 1 hour".into()),
                "echo hi".into()
            ))),
            "add at (new)"
        );
    }
}
