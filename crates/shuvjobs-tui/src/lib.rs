//! ratatui frontend for shuvjobs. Depends only on `shuvjobs-core` — filter/sort/search
//! logic lives there in `shuvjobs_core::view`; this crate is just the keyboard
//! and render layer.

use std::collections::HashSet;
use std::io;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
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

/// `initial` is the first paint. If `refresh_secs` is set the TUI calls
/// `refresh` on that interval to re-collect tasks.
pub struct RunOptions {
    pub initial: Vec<ScheduledTask>,
    pub refresh: Option<Box<dyn FnMut() -> Result<Vec<ScheduledTask>>>>,
    pub refresh_secs: Option<u64>,
}

pub fn run(opts: RunOptions) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    let result = event_loop(&mut terminal, opts);

    // Restore the terminal even when the loop bailed.
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    Filter,
    Search,
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
    refresh: Option<Box<dyn FnMut() -> Result<Vec<ScheduledTask>>>>,
    refresh_secs: Option<u64>,
    last_refresh: Instant,
    refreshing: bool,
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
            refresh: opts.refresh,
            refresh_secs: opts.refresh_secs,
            last_refresh: Instant::now(),
            refreshing: false,
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

    /// Re-collect when the auto-refresh interval has elapsed.
    fn maybe_refresh<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<bool>
    where
        B::Error: Send + Sync + 'static,
    {
        let Some(secs) = self.refresh_secs else {
            return Ok(false);
        };
        let Some(refresh) = self.refresh.as_mut() else {
            return Ok(false);
        };
        if self.last_refresh.elapsed() < StdDuration::from_secs(secs) {
            return Ok(false);
        }

        // Draw the "refreshing…" indicator before the synchronous call
        // so the user sees something during the SSH round-trip.
        self.refreshing = true;
        let visible = std::mem::take(&mut self.visible);
        let all = std::mem::take(&mut self.all);
        let sort = self.sort;
        let filter = self.filter.clone();
        let mode = self.mode;
        let filter_cursor = self.filter_cursor;
        let detail_open = self.detail_open;
        let refreshing = self.refreshing;
        let available_sources = self.available_sources.clone();
        let table_state = self.table_state;
        let now = Utc::now();
        terminal.draw(|frame| {
            draw_with(
                frame,
                &all,
                &visible,
                &available_sources,
                &filter,
                sort,
                mode,
                filter_cursor,
                detail_open,
                refreshing,
                &mut table_state.clone(),
                now,
            )
        })?;
        // Put the moved-out fields back before the closure runs so a
        // panic inside it doesn't leave us in a half-state.
        self.visible = visible;
        self.all = all;

        let new_tasks = refresh()?;
        self.all = new_tasks;
        self.available_sources = available_sources_of(&self.all);
        // A newly-discovered source would otherwise default to hidden
        // because the existing toggle set doesn't list it.
        if let Some(allowed) = &mut self.filter.allowed_sources {
            for kind in &self.available_sources {
                allowed.insert(*kind);
            }
        }
        self.refilter();
        self.refreshing = false;
        self.last_refresh = Instant::now();
        Ok(true)
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
        let poll_for = if app.refresh_secs.is_some() {
            StdDuration::from_millis(250)
        } else {
            StdDuration::from_millis(500)
        };

        app.maybe_refresh(terminal)?;

        let now = Utc::now();
        terminal.draw(|frame| draw_app(frame, &mut app, now))?;

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
    // Pull table_state out by clone — render_stateful_widget needs it
    // mutably while the row builders below borrow other app fields.
    let mut table_state = app.table_state;
    draw_with(
        frame,
        &app.all,
        &app.visible,
        &app.available_sources,
        &app.filter,
        app.sort,
        app.mode,
        app.filter_cursor,
        app.detail_open,
        app.refreshing,
        &mut table_state,
        now,
    );
    app.table_state = table_state;
}

#[allow(clippy::too_many_arguments)]
fn draw_with(
    frame: &mut Frame,
    all: &[ScheduledTask],
    visible: &[ScheduledTask],
    available_sources: &[TaskSourceKind],
    filter: &Filter,
    sort: SortMode,
    mode: Mode,
    filter_cursor: usize,
    detail_open: bool,
    refreshing: bool,
    table_state: &mut TableState,
    now: DateTime<Utc>,
) {
    let area = frame.area();

    // header / body / [optional bar] / footer
    let bar_present = matches!(mode, Mode::Filter | Mode::Search);
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

    draw_header(frame, header_area, all.len(), visible.len(), refreshing);
    draw_body(frame, body_area, visible, detail_open, table_state, now);
    if let Some(area) = bar_area {
        match mode {
            Mode::Filter => draw_filter_bar(frame, area, available_sources, filter, filter_cursor),
            Mode::Search => draw_search_bar(frame, area, &filter.search),
            Mode::Normal => {}
        }
    }
    draw_footer(frame, footer_area, filter, sort, available_sources);
}

fn draw_header(frame: &mut Frame, area: Rect, total: usize, shown: usize, refreshing: bool) {
    let mut text = format!(" ShuvJobs — {shown}/{total} task(s) ");
    if refreshing {
        text.push_str(" · refreshing… ");
    }
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

fn format_dt_with_relative(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match dt {
        None => "-".into(),
        Some(dt) => format!(
            "{} ({})",
            format_relative(dt, now),
            dt.format("%Y-%m-%d %H:%M:%S UTC")
        ),
    }
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

    let full = "/ search · f filter · s sort · Enter detail · q quit ";
    let narrow = "/ · f · s · q ";
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

fn format_schedule(s: &ScheduleType) -> String {
    match s {
        ScheduleType::Cron(expr) => expr.clone(),
        ScheduleType::Calendar(expr) if expr.is_empty() => "(unknown)".into(),
        ScheduleType::Calendar(expr) => expr.clone(),
        ScheduleType::Interval(d) => format!("every {}", human_duration(*d)),
        ScheduleType::OneShot(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
    }
}

fn format_status(s: Option<&TaskStatus>) -> &'static str {
    match s {
        Some(TaskStatus::Success) => "✅",
        Some(TaskStatus::Failed(_)) => "❌",
        Some(TaskStatus::Running) => "⏳",
        None => "-",
    }
}

fn format_optional_dt(dt: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match dt {
        Some(dt) => format_relative(dt, now),
        None => "-".to_string(),
    }
}

fn format_relative(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = dt.signed_duration_since(now);
    let abs = delta.num_seconds().unsigned_abs();
    let in_future = delta.num_seconds() >= 0;

    if abs >= 30 * 86_400 {
        return dt.format("%Y-%m-%d").to_string();
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
