//! ratatui frontend for shuvjobs. Depends only on `shuvjobs-core` — filter/sort/search
//! logic lives there in `shuvjobs_core::view`; this crate is just the keyboard
//! and render layer.

use std::collections::HashSet;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use shuvjobs_core::{
    view::{apply, Filter, SortMode},
    ScheduleType, ScheduledTask, TaskSourceKind, TaskStatus,
};

/// Detail-pane absolute stamp: local wall clock plus the zone offset, so a
/// timestamp is never ambiguous when read on another machine.
const ABSOLUTE_FMT: &str = "%Y-%m-%d %H:%M:%S %Z";
/// One-shot schedules are shown in table cells too, so they stay compact.
const ONE_SHOT_FMT: &str = "%Y-%m-%d %H:%M %Z";
const DATE_FMT: &str = "%Y-%m-%d";

/// `initial` is the first paint. `refresh` is moved onto a background worker
/// thread, so it must be `Send`; the event loop never calls it inline. If
/// `refresh_secs` is set the TUI re-collects on that interval, measured from
/// the last *completed* refresh; `r` re-collects on demand either way.
pub struct RunOptions {
    pub initial: Vec<ScheduledTask>,
    pub refresh: Option<RefreshFn>,
    pub refresh_secs: Option<u64>,
}

/// Collection callback. `Send` because it lives on the refresh worker thread.
pub type RefreshFn = Box<dyn FnMut() -> Result<Vec<ScheduledTask>> + Send>;

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
}

/// Owns the collection closure on a background thread so an SSH round-trip
/// never blocks the event loop. The loop sends a unit request and picks the
/// reply up later with `try_recv`.
struct RefreshWorker {
    requests: Sender<()>,
    replies: Receiver<Result<Vec<ScheduledTask>>>,
}

impl RefreshWorker {
    fn spawn(mut refresh: RefreshFn) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (rep_tx, rep_rx) = mpsc::channel::<Result<Vec<ScheduledTask>>>();
        // Deliberately detached: the handle is dropped, never joined. When the
        // App goes away `req_tx` drops, `recv` fails, and the thread ends after
        // whatever call it is inside returns. A worker stuck in a slow SSH call
        // is left to finish on its own after the terminal has been restored, so
        // quitting is always immediate instead of waiting on the network.
        thread::spawn(move || {
            while req_rx.recv().is_ok() {
                let result = refresh();
                if rep_tx.send(result).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: req_tx,
            replies: rep_rx,
        }
    }
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
    worker: Option<RefreshWorker>,
    refresh_secs: Option<u64>,
    /// Start of the interval countdown: set when a refresh *completes*, so a
    /// slow collection never queues up back-to-back refreshes.
    last_refresh: Instant,
    refreshing: bool,
    /// First line of the last failed collection, kept until one succeeds.
    refresh_error: Option<String>,
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
            worker: opts.refresh.map(RefreshWorker::spawn),
            refresh_secs: opts.refresh_secs,
            last_refresh: Instant::now(),
            refreshing: false,
            refresh_error: None,
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
        }
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

    /// Ask the worker to collect. Ignored when a refresh is already in
    /// flight — one at a time keeps the reply channel unambiguous and
    /// stops `r` mashing from queueing up SSH sessions.
    fn request_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if worker.requests.send(()).is_ok() {
            self.refreshing = true;
        }
    }

    /// Auto-refresh trigger: fires once `refresh_secs` have passed since the
    /// last completed refresh.
    fn maybe_auto_refresh(&mut self) {
        let Some(secs) = self.refresh_secs else {
            return;
        };
        if self.refreshing || self.last_refresh.elapsed() < StdDuration::from_secs(secs) {
            return;
        }
        self.request_refresh();
    }

    /// Non-blocking check for a finished collection.
    fn poll_refresh(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        match worker.replies.try_recv() {
            Ok(result) => self.apply_refresh(result),
            Err(TryRecvError::Empty) => {}
            // The worker died (panicked). Stop expecting replies.
            Err(TryRecvError::Disconnected) => {
                self.worker = None;
                self.refreshing = false;
            }
        }
    }

    /// Fold a collection result into the view. A failure keeps the last good
    /// data and only records a message, so a flaky link never blanks the table
    /// or drops the user out of the TUI.
    fn apply_refresh(&mut self, result: Result<Vec<ScheduledTask>>) {
        self.refreshing = false;
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
            Err(e) => {
                let msg = e.to_string();
                let first = msg.lines().next().unwrap_or("unknown error").to_string();
                self.refresh_error = Some(first);
            }
        }
    }
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
        app.poll_refresh();
        app.maybe_auto_refresh();

        let now = Utc::now();
        terminal.draw(|frame| draw_app(frame, &mut app, now))?;

        // Poll briefly whenever a reply or an interval tick may be due, so the
        // header indicator and fresh data land without waiting on a keypress.
        let poll_for = if app.refreshing || app.refresh_secs.is_some() {
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
        app.refreshing,
        app.refresh_error.as_deref(),
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
            Mode::Normal => {}
        }
    }
    draw_footer(
        frame,
        footer_area,
        &app.filter,
        app.sort,
        &app.available_sources,
    );
}

/// Header body, without the surrounding padding. Split out so the
/// in-flight and failure indicators are testable without a terminal.
fn header_text(
    total: usize,
    shown: usize,
    refreshing: bool,
    refresh_error: Option<&str>,
) -> String {
    let mut text = format!("ShuvJobs — {shown}/{total} task(s)");
    if refreshing {
        text.push_str(" · refreshing…");
    }
    if let Some(err) = refresh_error {
        text.push_str(&format!(" · refresh failed: {err}"));
    }
    text
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    shown: usize,
    refreshing: bool,
    refresh_error: Option<&str>,
) {
    let mut text = header_text(total, shown, refreshing, refresh_error);
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
                Cell::from(format_status(t.last_status.as_ref())),
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
        kv("Source", format_source(t.source)),
        kv("Command", &t.command),
        kv("Schedule", &format_schedule(&t.schedule)),
        kv("Last run", &format_dt_with_relative(t.last_run, now)),
        kv("Next run", &format_dt_with_relative(t.next_run, now)),
        kv("Status", &format_status_long(t.last_status.as_ref())),
    ];
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
        Some(TaskStatus::Success) => "✅ Success".into(),
        Some(TaskStatus::Failed(msg)) if !msg.is_empty() => format!("❌ Failed ({msg})"),
        Some(TaskStatus::Failed(_)) => "❌ Failed".into(),
        Some(TaskStatus::Running) => "⏳ Running".into(),
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

    let full = "/ search · f filter · s sort · r refresh · Enter detail · q quit ";
    let narrow = "/ · f · s · r · q ";
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
        Some(TaskStatus::Success) => "✅",
        Some(TaskStatus::Failed(_)) => "❌",
        Some(TaskStatus::Running) => "⏳",
        None => "-",
    }
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
        assert_eq!(format_status(Some(&TaskStatus::Running)), "⏳");
        assert_eq!(
            format_status_long(Some(&TaskStatus::Failed("exit-code".into()))),
            "❌ Failed (exit-code)"
        );
        assert_eq!(
            format_status_long(Some(&TaskStatus::Failed(String::new()))),
            "❌ Failed"
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
            refresh_secs: None,
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
        assert!(!app.refreshing, "no worker, nothing to wait for");
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
            refresh_secs: None,
        });

        app.handle_key(KeyCode::Char('r'));
        assert!(app.refreshing);
        started_rx
            .recv_timeout(StdDuration::from_secs(5))
            .expect("worker picked the request up");

        app.handle_key(KeyCode::Char('r'));
        assert!(app.refreshing);
        app.poll_refresh();
        assert!(app.refreshing, "no reply yet, still in flight");
        assert_eq!(
            started_rx.try_recv(),
            Err(TryRecvError::Empty),
            "the second request was dropped, not queued"
        );

        release_tx.send(()).unwrap();
        // The reply may not have landed on the very first poll.
        for _ in 0..100 {
            app.poll_refresh();
            if !app.refreshing {
                break;
            }
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(!app.refreshing, "reply applied");
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
        app.refreshing = true;

        app.apply_refresh(Ok(vec![
            task("delta", TaskSourceKind::Cron),
            task("echo", TaskSourceKind::Systemd),
        ]));

        assert!(!app.refreshing);
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
        app.refreshing = true;

        app.apply_refresh(Err(anyhow::anyhow!("ssh: connect failed\nsecond line")));

        assert!(!app.refreshing);
        assert_eq!(app.refresh_error.as_deref(), Some("ssh: connect failed"));
        assert_eq!(app.visible.len(), 1, "old rows survive a failure");
        assert_eq!(app.visible[0].name, "alpha");
        assert!(!app.quit, "a refresh failure never leaves the TUI");

        app.apply_refresh(Ok(vec![task("bravo", TaskSourceKind::Cron)]));
        assert!(app.refresh_error.is_none());
    }

    #[test]
    fn header_shows_in_flight_and_failure_state() {
        assert_eq!(header_text(3, 2, false, None), "ShuvJobs — 2/3 task(s)");
        assert_eq!(
            header_text(3, 3, true, None),
            "ShuvJobs — 3/3 task(s) · refreshing…"
        );
        assert_eq!(
            header_text(3, 3, true, Some("ssh: connect failed")),
            "ShuvJobs — 3/3 task(s) · refreshing… · refresh failed: ssh: connect failed"
        );
    }
}
