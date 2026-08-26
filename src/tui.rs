use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    backend::CrosstermBackend,
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use crate::control::{self, ControlAction, LoadOptions, RESULT_VISIBLE};
use crate::dispatcher::{AppState, BackendApiType, BackendStatus, LogEvent};

#[derive(PartialEq, Clone, Copy)]
enum Panel {
    Backends,
    Users,
    Blocked,
    Logs,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Panel::Backends => Panel::Users,
            Panel::Users => Panel::Blocked,
            Panel::Blocked => Panel::Logs,
            Panel::Logs => Panel::Backends,
        }
    }

    fn prev(self) -> Self {
        match self {
            Panel::Backends => Panel::Logs,
            Panel::Users => Panel::Backends,
            Panel::Blocked => Panel::Users,
            Panel::Logs => Panel::Blocked,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Panel::Backends => "BACKENDS",
            Panel::Users => "USERS",
            Panel::Blocked => "BLOCKED",
            Panel::Logs => "LOGS",
        }
    }
}

/// Model-name entry for a load/unload control operation on a backend.
#[derive(Clone)]
enum InputMode {
    Load { backend_idx: usize },
    Unload { backend_idx: usize },
}

/// Transient feedback line (shown after a control op is started/rejected).
struct FlashMsg {
    text: String,
    ok: bool,
    at: Instant,
}

/// Full-content detail overlay for one log event. The event is a clone, so
/// events arriving while the view is open don't shift what's displayed.
struct LogDetail {
    event: LogEvent,
    /// Scroll offset in wrapped (visual) lines.
    offset: usize,
}

/// Lines scrolled per PageUp/PageDown in the log detail view.
const DETAIL_PAGE: usize = 10;

struct ActiveOpView {
    backend_idx: usize,
    verb: String,
    model: String,
    identifier: Option<String>,
    elapsed_secs: u64,
}

struct RecentResultView {
    backend_idx: usize,
    ok: bool,
    verb: String,
    model: String,
    identifier: Option<String>,
    error: Option<String>,
}

struct StateSnapshot {
    queues_len: HashMap<String, usize>,
    processing_counts: HashMap<String, usize>,
    processed_counts: HashMap<String, usize>,
    dropped_counts: HashMap<String, usize>,
    /// Request-log records dropped because the log channel was full.
    reqlog_dropped: u64,
    user_ips: HashMap<String, IpAddr>,
    blocked_ips: HashSet<IpAddr>,
    blocked_users: HashSet<String>,
    vip_user: Option<String>,
    boost_user: Option<String>,
    user_ids: Vec<String>,
    backends: Vec<BackendStatus>,
    /// Recent request/control events (newest first) for the Logs panel.
    logs: Vec<LogEvent>,
    active_ops: Vec<ActiveOpView>,
    recent_results: Vec<RecentResultView>,
}

pub struct TuiDashboard {
    table_state: TableState,
    backend_table_state: TableState,
    blocked_table_state: TableState,
    log_table_state: TableState,
    active_panel: Panel,
    expanded_backends: HashSet<String>,
    show_all_backends: HashSet<String>,
    /// Per-backend (keyed by URL) cursor into the expanded model list,
    /// cycled with Tab/Shift+Tab.
    model_cursor: HashMap<String, usize>,
    show_help: bool,
    input_mode: Option<InputMode>,
    input_buf: String,
    flash: Option<FlashMsg>,
    log_detail: Option<LogDetail>,
}

impl TuiDashboard {
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            backend_table_state: TableState::default(),
            blocked_table_state: TableState::default(),
            log_table_state: TableState::default(),
            active_panel: Panel::Users,
            expanded_backends: HashSet::new(),
            show_all_backends: HashSet::new(),
            model_cursor: HashMap::new(),
            show_help: false,
            input_mode: None,
            input_buf: String::new(),
            flash: None,
            log_detail: None,
        }
    }

    fn capture_snapshot(&self, state: &Arc<AppState>) -> StateSnapshot {
        let queues_len: HashMap<String, usize> = {
            let q = state.queues.lock().unwrap();
            q.iter().map(|(k, v)| (k.clone(), v.len())).collect()
        };
        let processing_counts = state.processing_counts.lock().unwrap().clone();
        let processed_counts = state.processed_counts.lock().unwrap().clone();
        let dropped_counts = state.dropped_counts.lock().unwrap().clone();
        let reqlog_dropped = state.reqlog.dropped_count();
        let user_ips = state.user_ips.lock().unwrap().clone();
        let blocked_ips = state.blocked_ips.lock().unwrap().clone();
        let blocked_users = state.blocked_users.lock().unwrap().clone();
        let vip_user = state.vip_user.lock().unwrap().clone();
        let boost_user = state.boost_user.lock().unwrap().clone();
        let backends = state.backends.lock().unwrap().clone();
        let logs: Vec<LogEvent> = state
            .logs
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(50)
            .cloned()
            .collect();
        let active_ops: Vec<ActiveOpView> = state
            .control_ops
            .lock()
            .unwrap()
            .values()
            .map(|op| ActiveOpView {
                backend_idx: op.backend_idx,
                verb: op.action.verb().to_string(),
                model: op.canonical.clone(),
                identifier: op.identifier.clone(),
                elapsed_secs: op.started.elapsed().as_secs(),
            })
            .collect();
        let recent_results: Vec<RecentResultView> = state
            .control_history
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.finished_at.elapsed() < RESULT_VISIBLE)
            .map(|r| RecentResultView {
                backend_idx: r.backend_idx,
                ok: r.ok,
                verb: r.action.verb().to_string(),
                model: r.model.clone(),
                identifier: r.identifier.clone(),
                error: r.error.clone(),
            })
            .collect();

        let mut user_ids: Vec<String> = queues_len.keys().cloned().collect();
        user_ids.sort_by(|a, b| {
            let a_q = queues_len.get(a).unwrap_or(&0) + processing_counts.get(a).unwrap_or(&0);
            let b_q = queues_len.get(b).unwrap_or(&0) + processing_counts.get(b).unwrap_or(&0);
            let a_total =
                processed_counts.get(a).unwrap_or(&0) + dropped_counts.get(a).unwrap_or(&0);
            let b_total =
                processed_counts.get(b).unwrap_or(&0) + dropped_counts.get(b).unwrap_or(&0);

            b_q.cmp(&a_q)
                .then_with(|| b_total.cmp(&a_total))
                .then_with(|| a.cmp(b))
        });

        StateSnapshot {
            queues_len,
            processing_counts,
            processed_counts,
            dropped_counts,
            reqlog_dropped,
            user_ips,
            blocked_ips,
            blocked_users,
            vip_user,
            boost_user,
            user_ids,
            backends,
            logs,
            active_ops,
            recent_results,
        }
    }

    pub fn run(&mut self, state: &Arc<AppState>) -> io::Result<bool> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;

        loop {
            let snapshot = self.capture_snapshot(state);
            terminal.draw(|f| self.render(f, &snapshot))?;

            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    // Log detail view (opened with Enter from the Logs panel):
                    // its keys take priority over all other handling, including
                    // input mode.
                    if self.log_detail.is_some() {
                        match key.code {
                            KeyCode::Char('j') | KeyCode::Down => {
                                if let Some(d) = self.log_detail.as_mut() {
                                    d.offset += 1;
                                }
                            }
                            KeyCode::PageDown => {
                                if let Some(d) = self.log_detail.as_mut() {
                                    d.offset += DETAIL_PAGE;
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                if let Some(d) = self.log_detail.as_mut() {
                                    d.offset = d.offset.saturating_sub(1);
                                }
                            }
                            KeyCode::PageUp => {
                                if let Some(d) = self.log_detail.as_mut() {
                                    d.offset = d.offset.saturating_sub(DETAIL_PAGE);
                                }
                            }
                            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                                self.log_detail = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Input mode (L/U model name): consume the keystroke and
                    // skip the normal key handling.
                    if let Some(mode) = self.input_mode.clone() {
                        match key.code {
                            KeyCode::Esc => {
                                self.input_mode = None;
                                self.input_buf.clear();
                            }
                            KeyCode::Enter => {
                                self.input_mode = None;
                                let model = self.input_buf.trim().to_string();
                                self.input_buf.clear();
                                if !model.is_empty() {
                                    let (backend_idx, action) = match mode {
                                        InputMode::Load { backend_idx } => {
                                            (backend_idx, ControlAction::Load)
                                        }
                                        InputMode::Unload { backend_idx } => {
                                            (backend_idx, ControlAction::Unload)
                                        }
                                    };
                                    self.submit_control(
                                        state,
                                        backend_idx,
                                        action,
                                        model,
                                        LoadOptions::default(),
                                    );
                                }
                            }
                            KeyCode::Backspace => {
                                self.input_buf.pop();
                            }
                            KeyCode::Char(c) => {
                                self.input_buf.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            io::stdout().execute(LeaveAlternateScreen)?;
                            disable_raw_mode()?;
                            terminal.show_cursor()?;
                            return Ok(false);
                        }
                        KeyCode::Char('?') => self.show_help = !self.show_help,
                        KeyCode::Tab | KeyCode::BackTab => {
                            let forward = key.code == KeyCode::Tab;
                            // Context-sensitive: when the selected backend is
                            // expanded, Tab cycles its model list instead of
                            // the panels.
                            if self.active_panel == Panel::Backends
                                && self.cycle_model_cursor(&snapshot, forward)
                            {
                                // model cursor moved
                            } else if forward {
                                self.active_panel = self.active_panel.next();
                            } else {
                                self.active_panel = self.active_panel.prev();
                            }
                        }
                        KeyCode::Char('l') => {
                            self.active_panel = self.active_panel.next();
                        }
                        KeyCode::Char('h') => {
                            self.active_panel = self.active_panel.prev();
                        }
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            if self.active_panel == Panel::Backends {
                                if let Some(i) = self.backend_table_state.selected() {
                                    if i < snapshot.backends.len() {
                                        let url = snapshot.backends[i].url.clone();
                                        if self.expanded_backends.contains(&url) {
                                            self.expanded_backends.remove(&url);
                                        } else {
                                            self.expanded_backends.insert(url);
                                        }
                                    }
                                }
                            } else if self.active_panel == Panel::Logs
                                && key.code == KeyCode::Enter
                            {
                                // Enter on a logs row opens the full-content
                                // detail view; rows without captured content
                                // just flash a note.
                                if let Some(i) = self
                                    .log_table_state
                                    .selected()
                                    .filter(|&i| i < snapshot.logs.len())
                                {
                                    let ev = &snapshot.logs[i];
                                    if ev
                                        .content
                                        .as_deref()
                                        .is_some_and(|c| !c.is_empty())
                                    {
                                        self.log_detail = Some(LogDetail {
                                            event: ev.clone(),
                                            offset: 0,
                                        });
                                    } else {
                                        self.flash = Some(FlashMsg {
                                            text: "no content captured for this event".to_string(),
                                            ok: false,
                                            at: Instant::now(),
                                        });
                                    }
                                }
                            }
                        }
                        KeyCode::Char('r') => {
                            // Re-read appconf.yaml and re-apply it.
                            match control::reload_model_config(state) {
                                Ok(n) => {
                                    self.flash = Some(FlashMsg {
                                        text: format!(
                                            "model config reloaded: {} model(s), applying",
                                            n
                                        ),
                                        ok: true,
                                        at: Instant::now(),
                                    });
                                }
                                Err(e) => {
                                    self.flash = Some(FlashMsg {
                                        text: format!("config reload failed: {}", e),
                                        ok: false,
                                        at: Instant::now(),
                                    });
                                }
                            }
                        }
                        KeyCode::Char('L') | KeyCode::Char('U') => {
                            if self.active_panel == Panel::Backends
                                && let Some(i) = self
                                    .backend_table_state
                                    .selected()
                                    .filter(|&i| i < snapshot.backends.len())
                            {
                                let action = if key.code == KeyCode::Char('L') {
                                    ControlAction::Load
                                } else {
                                    ControlAction::Unload
                                };
                                // When the backend is expanded and a model is
                                // under the cursor, act on it directly.
                                let b = &snapshot.backends[i];
                                if self.expanded_backends.contains(&b.url) {
                                    let mut models: Vec<String> =
                                        b.available_models.iter().cloned().collect();
                                    models.sort();
                                    if let Some(model) = self
                                        .model_cursor
                                        .get(&b.url)
                                        .filter(|&c| *c < models.len())
                                        .map(|&c| models[c].clone())
                                    {
                                        self.submit_control(
                                            state,
                                            i,
                                            action,
                                            model,
                                            LoadOptions::default(),
                                        );
                                        continue;
                                    }
                                }
                                self.input_buf.clear();
                                self.input_mode = Some(if action == ControlAction::Load {
                                    InputMode::Load { backend_idx: i }
                                } else {
                                    InputMode::Unload { backend_idx: i }
                                });
                            }
                        }
                        KeyCode::Char('a') => {
                            if self.active_panel != Panel::Backends {
                                // not in backends panel
                            } else if let Some(i) = self
                                .backend_table_state
                                .selected()
                                .filter(|&i| i < snapshot.backends.len())
                            {
                                let url = snapshot.backends[i].url.clone();
                                if self.show_all_backends.contains(&url) {
                                    self.show_all_backends.remove(&url);
                                } else {
                                    self.show_all_backends.insert(url);
                                }
                            }
                        }
                        KeyCode::Char('p') => {
                            if self.active_panel == Panel::Users {
                                if let Some(i) = self.table_state.selected() {
                                    if i < snapshot.user_ids.len() {
                                        let user_id = snapshot.user_ids[i].clone();

                                        // 1. Handle VIP
                                        {
                                            let mut vip = state.vip_user.lock().unwrap();
                                            if vip.as_ref() == Some(&user_id) {
                                                *vip = None;
                                            } else {
                                                *vip = Some(user_id.clone());
                                            }
                                        }

                                        // 2. Clear Boost if we just set VIP
                                        {
                                            let mut boost = state.boost_user.lock().unwrap();
                                            if boost.as_ref() == Some(&user_id) {
                                                *boost = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('b') => {
                            if self.active_panel == Panel::Users {
                                if let Some(i) = self.table_state.selected() {
                                    if i < snapshot.user_ids.len() {
                                        let user_id = snapshot.user_ids[i].clone();

                                        // 1. Handle Boost
                                        {
                                            let mut boost = state.boost_user.lock().unwrap();
                                            if boost.as_ref() == Some(&user_id) {
                                                *boost = None;
                                            } else {
                                                *boost = Some(user_id.clone());
                                            }
                                        }

                                        // 2. Clear VIP if we just set Boost
                                        {
                                            let mut vip = state.vip_user.lock().unwrap();
                                            if vip.as_ref() == Some(&user_id) {
                                                *vip = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('x') => {
                            if self.active_panel == Panel::Users {
                                if let Some(i) = self.table_state.selected() {
                                    if i < snapshot.user_ids.len() {
                                        let user_id = snapshot.user_ids[i].clone();
                                        state.block_user(user_id);
                                    }
                                }
                            }
                        }
                        KeyCode::Char('X') => {
                            if self.active_panel == Panel::Users {
                                if let Some(i) = self.table_state.selected() {
                                    if i < snapshot.user_ids.len() {
                                        let user_id = &snapshot.user_ids[i];
                                        if let Some(ip) = snapshot.user_ips.get(user_id) {
                                            state.block_ip(*ip);
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('u') => {
                            if self.active_panel == Panel::Blocked {
                                let selected = self.blocked_table_state.selected();
                                if let Some(i) = selected {
                                    let mut items = Vec::new();
                                    for ip in snapshot.blocked_ips.iter() {
                                        items.push(("IP", ip.to_string()));
                                    }
                                    for user in snapshot.blocked_users.iter() {
                                        items.push(("USER", user.clone()));
                                    }
                                    items.sort_by(|a, b| a.1.cmp(&b.1));

                                    if i < items.len() {
                                        let (kind, value) = &items[i];
                                        if *kind == "IP" {
                                            if let Ok(ip) = value.parse() {
                                                state.unblock_ip(ip);
                                            }
                                        } else {
                                            state.unblock_user(value);
                                        }
                                    }
                                }
                            } else if self.active_panel == Panel::Users {
                                if let Some(i) = self.table_state.selected() {
                                    if i < snapshot.user_ids.len() {
                                        let user_id = &snapshot.user_ids[i];
                                        state.unblock_user(user_id);
                                        if let Some(ip) = snapshot.user_ips.get(user_id) {
                                            state.unblock_ip(*ip);
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if self.active_panel == Panel::Backends {
                                let i = self
                                    .backend_table_state
                                    .selected()
                                    .unwrap_or(0)
                                    .saturating_sub(1);
                                self.backend_table_state.select(Some(i));
                            } else if self.active_panel == Panel::Users {
                                let i = self.table_state.selected().unwrap_or(0).saturating_sub(1);
                                self.table_state.select(Some(i));
                            } else if self.active_panel == Panel::Logs {
                                let i = self
                                    .log_table_state
                                    .selected()
                                    .unwrap_or(0)
                                    .saturating_sub(1);
                                self.log_table_state.select(Some(i));
                            } else {
                                let i = self
                                    .blocked_table_state
                                    .selected()
                                    .unwrap_or(0)
                                    .saturating_sub(1);
                                self.blocked_table_state.select(Some(i));
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if self.active_panel == Panel::Backends {
                                let len = snapshot.backends.len();
                                if len > 0 {
                                    let i = self
                                        .backend_table_state
                                        .selected()
                                        .map(|s| (s + 1).min(len.saturating_sub(1)))
                                        .unwrap_or(0);
                                    self.backend_table_state.select(Some(i));
                                }
                            } else if self.active_panel == Panel::Users {
                                let len = snapshot.user_ids.len();
                                if len > 0 {
                                    let i = self
                                        .table_state
                                        .selected()
                                        .map(|s| (s + 1).min(len.saturating_sub(1)))
                                        .unwrap_or(0);
                                    self.table_state.select(Some(i));
                                }
                            } else if self.active_panel == Panel::Logs {
                                let len = snapshot.logs.len();
                                if len > 0 {
                                    let i = self
                                        .log_table_state
                                        .selected()
                                        .map(|s| (s + 1).min(len.saturating_sub(1)))
                                        .unwrap_or(0);
                                    self.log_table_state.select(Some(i));
                                }
                            } else {
                                let len = snapshot.blocked_ips.len() + snapshot.blocked_users.len();
                                if len > 0 {
                                    let i = self
                                        .blocked_table_state
                                        .selected()
                                        .map(|s| (s + 1).min(len.saturating_sub(1)))
                                        .unwrap_or(0);
                                    self.blocked_table_state.select(Some(i));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Start a control op and flash the outcome.
    fn submit_control(
        &mut self,
        state: &Arc<AppState>,
        backend_idx: usize,
        action: ControlAction,
        model: String,
        options: LoadOptions,
    ) {
        let action_verb = action.verb();
        match control::start_model_control(state, backend_idx, action, model.clone(), options) {
            Ok(canonical) => {
                self.flash = Some(FlashMsg {
                    text: format!("{} {} started", action_verb, canonical),
                    ok: true,
                    at: Instant::now(),
                });
            }
            Err(e) => {
                self.flash = Some(FlashMsg {
                    text: format!("{} '{}' rejected: {}", action_verb, model, e),
                    ok: false,
                    at: Instant::now(),
                });
            }
        }
    }

    /// Advance the model cursor of the selected (expanded) backend.
    /// Returns false when the Backends panel has no expanded backend with
    /// models (so Tab should fall through to panel cycling).
    fn cycle_model_cursor(&mut self, snapshot: &StateSnapshot, forward: bool) -> bool {
        let Some(i) = self
            .backend_table_state
            .selected()
            .filter(|&i| i < snapshot.backends.len())
        else {
            return false;
        };
        let b = &snapshot.backends[i];
        if !self.expanded_backends.contains(&b.url) {
            return false;
        }
        let mut models: Vec<String> = b.available_models.iter().cloned().collect();
        models.sort();
        if models.is_empty() {
            return false;
        }
        let len = models.len();
        let next = match self.model_cursor.get(&b.url).copied() {
            None => 0,
            Some(cur) if forward => (cur + 1) % len,
            Some(cur) => (cur + len - 1) % len,
        };
        self.model_cursor.insert(b.url.clone(), next);
        // Keep the cursor model visible when the list is folded to 5.
        if next >= 5 && !self.show_all_backends.contains(&b.url) {
            self.show_all_backends.insert(b.url.clone());
        }
        true
    }

    fn render(&mut self, f: &mut Frame, snapshot: &StateSnapshot) {
        // Log detail overlay takes over the screen while open. Take/restore
        // keeps the borrow of `log_detail` from overlapping the `&mut self`
        // receiver of `render_log_detail`.
        if let Some(mut detail) = self.log_detail.take() {
            self.render_log_detail(f, &mut detail);
            self.log_detail = Some(detail);
            return;
        }

        match self.active_panel {
            Panel::Backends => {
                if snapshot.backends.is_empty() {
                    self.backend_table_state.select(None);
                } else if self.backend_table_state.selected().is_none() {
                    self.backend_table_state.select(Some(0));
                }
            }
            Panel::Users => {
                if snapshot.user_ids.is_empty() {
                    self.table_state.select(None);
                } else if self.table_state.selected().is_none() {
                    self.table_state.select(Some(0));
                }
            }
            Panel::Logs => {
                if snapshot.logs.is_empty() {
                    self.log_table_state.select(None);
                } else if self.log_table_state.selected().is_none() {
                    self.log_table_state.select(Some(0));
                }
            }
            Panel::Blocked => {
                let blocked_total = snapshot.blocked_ips.len() + snapshot.blocked_users.len();
                if blocked_total == 0 {
                    self.blocked_table_state.select(None);
                } else if self.blocked_table_state.selected().is_none() {
                    self.blocked_table_state.select(Some(0));
                }
            }
        }

        let area = f.area();
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Stats
                Constraint::Min(0),    // Content
                Constraint::Length(9), // Request logs (in/out)
                Constraint::Length(3), // Help bar
                if self.show_help {
                    Constraint::Length(12)
                } else {
                    Constraint::Length(0)
                },
            ])
            .split(area);

        f.render_widget(self.render_stats(snapshot), main_chunks[0]);

        let content_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Percentage(35),
                Constraint::Percentage(30),
            ])
            .split(main_chunks[1]);

        f.render_stateful_widget(
            self.render_backends(snapshot),
            content_chunks[0],
            &mut self.backend_table_state,
        );
        f.render_stateful_widget(
            self.render_users(snapshot),
            content_chunks[1],
            &mut self.table_state,
        );

        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(content_chunks[2]);

        f.render_stateful_widget(
            self.render_queues(snapshot, right_chunks[0].width),
            right_chunks[0],
            &mut self.table_state,
        );
        f.render_stateful_widget(
            self.render_blocked(snapshot),
            right_chunks[1],
            &mut self.blocked_table_state,
        );

        f.render_widget(self.render_logs(snapshot), main_chunks[2]);

        f.render_widget(self.render_help(snapshot), main_chunks[3]);
        if self.show_help {
            f.render_widget(self.render_detailed_help(), main_chunks[4]);
        }
    }

    /// Full-content detail overlay for one log event: a centered 80% x 80%
    /// block with a header (time/direction/user/model/backend/info) on top,
    /// a manually pre-wrapped, scrollable content area, and a key footer.
    fn render_log_detail(&mut self, f: &mut Frame, detail: &mut LogDetail) {
        let area = f.area();

        // Tiny terminal: a centered 80x80 overlay has no room; draw a
        // minimal placeholder instead.
        if area.width < 20 || area.height < 10 {
            f.render_widget(
                Block::default()
                    .title(" Log detail (terminal too small) ")
                    .borders(Borders::ALL),
                area,
            );
            return;
        }

        let width = area.width * 4 / 5;
        let height = area.height * 4 / 5;
        let block_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block = Block::default()
            .title(format!(" Log detail — {} ", detail.event.dir))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(block_area);
        f.render_widget(block, block_area);

        // Manually pre-wrap the content: split it into lines, then split each
        // line into chunks of `inner.width` chars, collecting the visual
        // lines. (No ratatui `Wrap`: it can't be scrolled by visual line.)
        let raw = detail
            .event
            .content
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or("(no content captured)");
        let mut visual: Vec<String> = Vec::new();
        if inner.width > 0 {
            for line in raw.lines() {
                let mut rest = line;
                loop {
                    if rest.len() <= inner.width as usize {
                        visual.push(rest.to_string());
                        break;
                    }
                    let cut = rest
                        .char_indices()
                        .nth(inner.width as usize)
                        .map(|(i, _)| i)
                        .unwrap_or(rest.len());
                    visual.push(rest[..cut].to_string());
                    rest = &rest[cut..];
                }
            }
        }
        let total_visual = visual.len();

        // Fixed 6-line header on top, 1-line footer at the bottom; the
        // content area gets whatever remains. Clamp the scroll offset to it.
        let header_len = 6;
        let footer_len = 1;
        let visible =
            inner.height.saturating_sub((header_len + footer_len) as u16) as usize;
        detail.offset = detail.offset.min(total_visual.saturating_sub(visible));

        let header = Paragraph::new(Text::from(vec![
            Line::from(format!("Time: {}", Self::fmt_event_time(detail.event.at))),
            Line::from(format!("Direction: {}", detail.event.dir)),
            Line::from(format!("User: {}", detail.event.user)),
            Line::from(format!(
                "Model: {}",
                detail.event.model.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "Backend: {}",
                detail.event.backend.as_deref().unwrap_or("-")
            )),
            Line::from(format!("Info: {}", detail.event.info)),
        ]));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_len),
                Constraint::Min(0),
                Constraint::Length(footer_len),
            ])
            .split(inner);

        f.render_widget(header, chunks[0]);

        let content: Vec<Line> = visual
            .iter()
            .skip(detail.offset)
            .take(visible)
            .map(|l| Line::from(l.as_str()))
            .collect();
        f.render_widget(Paragraph::new(content), chunks[1]);

        let footer =
            Paragraph::new("j/k: scroll  PgUp/PgDn: page  q/Esc: close")
                .style(Style::default().fg(Color::DarkGray));
        f.render_widget(footer, chunks[2]);
    }

    fn render_stats(&self, snapshot: &StateSnapshot) -> Paragraph<'static> {
        let total_queued: usize = snapshot.queues_len.values().sum();
        let total_processing: usize = snapshot.processing_counts.values().sum();
        let total_processed: usize = snapshot.processed_counts.values().sum();
        let total_dropped: usize = snapshot.dropped_counts.values().sum();

        let stats_line = vec![
            Span::styled(" ollamaMQ ", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" | "),
            Span::styled("Panel: ", Style::default().fg(Color::White)),
            Span::styled(
                self.active_panel.label(),
                Style::default().fg(Color::Yellow).bold(),
            ),
            Span::raw(" | "),
            Span::styled("VIP: ", Style::default().fg(Color::Magenta)),
            Span::styled(
                snapshot
                    .vip_user
                    .clone()
                    .unwrap_or_else(|| "None".to_string()),
                Style::default().fg(Color::Magenta).bold(),
            ),
            Span::raw(" | "),
            Span::styled("Boost: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                snapshot
                    .boost_user
                    .clone()
                    .unwrap_or_else(|| "None".to_string()),
                Style::default().fg(Color::Yellow).bold(),
            ),
            Span::raw(" | "),
            Span::styled("Q: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                (total_queued + total_processing).to_string(),
                Style::default().fg(Color::Yellow).bold(),
            ),
            Span::raw(" | "),
            Span::styled("Done: ", Style::default().fg(Color::Green)),
            Span::styled(
                total_processed.to_string(),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::raw(" | "),
            Span::styled("Drop: ", Style::default().fg(Color::Red)),
            Span::styled(
                total_dropped.to_string(),
                Style::default().fg(Color::Red).bold(),
            ),
            Span::raw(" | "),
            Span::styled("LogDrop: ", Style::default().fg(Color::Red)),
            Span::styled(
                snapshot.reqlog_dropped.to_string(),
                Style::default().fg(Color::Red).bold(),
            ),
        ];

        Paragraph::new(Line::from(stats_line)).block(Block::default().borders(Borders::ALL))
    }

    fn render_backends(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let rows: Vec<Row> = snapshot
            .backends
            .iter()
            .enumerate()
            .map(|(idx, b)| {
                let url = b.url.replace("http://", "").replace("https://", "");
                let is_expanded = self.expanded_backends.contains(&b.url);

                let (status_sym, status_style) = if b.is_online {
                    ("● ", Style::default().fg(Color::Green))
                } else {
                    ("○ ", Style::default().fg(Color::Red))
                };

                let type_str = b.api_type.display();
                let type_style = match b.api_type {
                    BackendApiType::Unknown => Style::default().fg(Color::Yellow),
                    BackendApiType::Both => Style::default().fg(Color::Rgb(0, 255, 255)).bold(),
                    BackendApiType::Ollama => Style::default().fg(Color::Green),
                    BackendApiType::OpenAi => Style::default().fg(Color::Blue),
                };

                let req_style = if b.active_requests > 0 {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };

                let mut name_lines = vec![Line::from(vec![
                    Span::styled(
                        if is_expanded { "▼ " } else { "▶ " },
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(status_sym, status_style),
                    Span::styled(
                        url,
                        if b.is_online {
                            Style::default().fg(Color::White)
                        } else {
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::CROSSED_OUT)
                        },
                    ),
                ])];

                // Display Active or Last used model on a new line
                if let Some(model) = b.current_model.clone() {
                    let prefix = if b.active_requests > 0 {
                        "  ▶ Active: "
                    } else {
                        "  ↺ Last:   "
                    };
                    let color = if b.active_requests > 0 {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    };
                    name_lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(color)),
                        Span::styled(model, Style::default().fg(color).bold()),
                    ]));
                }

                // In-flight model control operation (load/unload)
                if let Some(op) = snapshot.active_ops.iter().find(|op| op.backend_idx == idx) {
                    name_lines.push(Line::from(vec![
                        Span::styled("  ⟳ ", Style::default().fg(Color::Cyan).bold()),
                        Span::styled(
                            format!("{}: {}", op.verb, op.model),
                            Style::default().fg(Color::Cyan),
                        ),
                        if let Some(id) = &op.identifier {
                            Span::styled(format!(" [{}]", id), Style::default().fg(Color::Cyan))
                        } else {
                            Span::raw("")
                        },
                        Span::styled(
                            format!(" ({}s…)", op.elapsed_secs),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }

                // Recent control result (visible for a few seconds)
                if let Some(r) = snapshot
                    .recent_results
                    .iter()
                    .find(|r| r.backend_idx == idx)
                {
                    if r.ok {
                        name_lines.push(Line::from(vec![
                            Span::styled("  ✓ ", Style::default().fg(Color::Green)),
                            Span::styled(
                                format!("{}: {}", r.verb, r.model),
                                Style::default().fg(Color::Green),
                            ),
                            if let Some(id) = &r.identifier {
                                Span::styled(format!(" [{}]", id), Style::default().fg(Color::Green))
                            } else {
                                Span::raw("")
                            },
                        ]));
                    } else {
                        name_lines.push(Line::from(vec![
                            Span::styled("  ✖ ", Style::default().fg(Color::Red)),
                            Span::styled(
                                format!(
                                    "{} failed: {}",
                                    r.verb,
                                    r.error.clone().unwrap_or_default()
                                ),
                                Style::default().fg(Color::Red),
                            ),
                        ]));
                    }
                }

                if is_expanded {
                    let mut models: Vec<String> = b.available_models.iter().cloned().collect();
                    models.sort();

                    if models.is_empty() {
                        name_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(
                                "└ (No models discovered yet)",
                                Style::default().fg(Color::DarkGray).italic(),
                            ),
                        ]));
                    } else {
                        let total_models = models.len();
                        let show_all = self.show_all_backends.contains(&b.url);
                        let limit = if show_all {
                            total_models
                        } else {
                            5.min(total_models)
                        };
                        let cursor = self.model_cursor.get(&b.url).copied();
                        for (mi, m) in models.into_iter().enumerate().take(limit) {
                            let is_cursor = cursor == Some(mi);
                            let is_loaded = b.loaded_models.contains(&m);
                            let m_style = if is_cursor {
                                Style::default().fg(Color::Yellow).bold()
                            } else if is_loaded {
                                Style::default().fg(Color::Green).bold()
                            } else {
                                Style::default().fg(Color::DarkGray)
                            };
                            let (sym, sym_style) = if is_cursor {
                                ("▶ ", Style::default().fg(Color::Yellow).bold())
                            } else {
                                ("└ ", Style::default().fg(Color::DarkGray))
                            };

                            name_lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(sym, sym_style),
                                Span::styled(m, m_style),
                                if is_loaded {
                                    Span::styled(
                                        " (In RAM)",
                                        Style::default().fg(Color::Green).italic(),
                                    )
                                } else {
                                    Span::raw("")
                                },
                            ]));
                        }
                        if !show_all && total_models > 5 {
                            name_lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    format!("  ... and {} more", total_models - 5),
                                    Style::default().fg(Color::DarkGray).italic(),
                                ),
                            ]));
                        }
                    }
                }

                let height = name_lines.len() as u16;

                Row::new(vec![
                    Cell::from(Text::from(name_lines)),
                    Cell::from(type_str).style(type_style),
                    Cell::from(b.active_requests.to_string()).style(req_style),
                    Cell::from(b.processed_count.to_string())
                        .style(Style::default().fg(Color::DarkGray)),
                ])
                .height(height)
            })
            .collect();

        Table::new(
            rows,
            [
                Constraint::Min(15),
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(6),
            ],
        )
        .header(
            Row::new(vec!["Backend", "API", "Act", "Done"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 40))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Backend Instances ")
                .borders(Borders::ALL)
                .border_style(if self.active_panel == Panel::Backends {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        )
    }

    fn render_users(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let rows: Vec<Row> = snapshot
            .user_ids
            .iter()
            .map(|user| {
                let queue_len = snapshot.queues_len.get(user).unwrap_or(&0)
                    + snapshot.processing_counts.get(user).unwrap_or(&0);
                let processed = snapshot.processed_counts.get(user).unwrap_or(&0);
                let dropped = snapshot.dropped_counts.get(user).unwrap_or(&0);
                let ip_str = snapshot
                    .user_ips
                    .get(user)
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                let is_blocked = snapshot.blocked_users.contains(user)
                    || snapshot
                        .user_ips
                        .get(user)
                        .map_or(false, |ip| snapshot.blocked_ips.contains(ip));
                let is_vip = snapshot.vip_user.as_ref() == Some(user);
                let is_boost = snapshot.boost_user.as_ref() == Some(user);

                let (sym, style) = if is_blocked {
                    ("✖ ", Style::default().fg(Color::Red))
                } else if is_vip {
                    ("★ ", Style::default().fg(Color::Magenta))
                } else if is_boost {
                    ("⚡", Style::default().fg(Color::Yellow))
                } else if *snapshot.processing_counts.get(user).unwrap_or(&0) > 0 {
                    ("▶ ", Style::default().fg(Color::Cyan))
                } else if *snapshot.queues_len.get(user).unwrap_or(&0) > 0 {
                    ("● ", Style::default().fg(Color::Green))
                } else {
                    ("○ ", Style::default().fg(Color::DarkGray))
                };

                let mut spans = vec![
                    Span::styled(sym, style),
                    Span::styled(
                        user.clone(),
                        if is_blocked {
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::CROSSED_OUT)
                        } else if is_vip {
                            Style::default().fg(Color::Magenta).bold()
                        } else if is_boost {
                            Style::default().fg(Color::Yellow).bold()
                        } else {
                            Style::default().fg(Color::White)
                        },
                    ),
                ];
                if is_vip {
                    spans.push(Span::styled(
                        " [VIP]",
                        Style::default().fg(Color::Magenta).bold(),
                    ));
                }
                if is_boost {
                    spans.push(Span::styled(
                        " [BST]",
                        Style::default().fg(Color::Yellow).bold(),
                    ));
                }
                if is_blocked {
                    spans.push(Span::styled(
                        " [BLOCKED]",
                        Style::default().fg(Color::Red).bold(),
                    ));
                }

                Row::new(vec![
                    Cell::from(Line::from(spans)),
                    Cell::from(ip_str).style(Style::default().fg(Color::Cyan)),
                    Cell::from(queue_len.to_string()),
                    Cell::from(processed.to_string()),
                    Cell::from(dropped.to_string()),
                ])
            })
            .collect();

        Table::new(
            rows,
            [
                Constraint::Percentage(45),
                Constraint::Percentage(25),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
                Constraint::Percentage(10),
            ],
        )
        .header(
            Row::new(vec!["User ID", "Last IP", "Q", "Done", "Drop"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 40))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Active Users ")
                .borders(Borders::ALL)
                .border_style(if self.active_panel == Panel::Users {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        )
    }

    fn render_queues(&self, snapshot: &StateSnapshot, available_width: u16) -> Table<'static> {
        let total_queued = snapshot.queues_len.values().sum::<usize>()
            + snapshot.processing_counts.values().sum::<usize>();
        let bar_max_width = ((available_width as f32) * 0.45) as usize;

        let rows: Vec<Row> = snapshot
            .user_ids
            .iter()
            .map(|user| {
                let q_len = snapshot.queues_len.get(user).unwrap_or(&0)
                    + snapshot.processing_counts.get(user).unwrap_or(&0);
                let bar_len = if q_len > 0 {
                    ((q_len as f32 / 20.0).min(1.0) * bar_max_width as f32) as usize
                } else {
                    0
                };
                let color = if snapshot.vip_user.as_ref() == Some(user) {
                    Color::Magenta
                } else if snapshot.boost_user.as_ref() == Some(user) {
                    Color::Yellow
                } else if *snapshot.processing_counts.get(user).unwrap_or(&0) > 0 {
                    Color::Cyan
                } else {
                    Color::Green
                };
                let bar = format!("{:<width$}", "⠿".repeat(bar_len), width = bar_max_width);
                let pct = if total_queued > 0 {
                    (q_len as f64 / total_queued as f64) * 100.0
                } else {
                    0.0
                };
                Row::new(vec![
                    Cell::from(user.clone()),
                    Cell::from(bar).style(Style::default().fg(color)),
                    Cell::from(format!("{} ({:.0}%)", q_len, pct))
                        .style(Style::default().fg(color).bold()),
                ])
            })
            .collect();

        Table::new(
            rows,
            [
                Constraint::Percentage(30),
                Constraint::Percentage(45),
                Constraint::Percentage(25),
            ],
        )
        .header(
            Row::new(vec!["User ID", "Progress", "Num"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 40))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Queue Status ")
                .borders(Borders::ALL),
        )
    }

    fn render_blocked(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let mut items = Vec::new();
        for ip in snapshot.blocked_ips.iter() {
            items.push(("IP", ip.to_string()));
        }
        for user in snapshot.blocked_users.iter() {
            items.push(("USER", user.clone()));
        }
        items.sort_by(|a, b| a.1.cmp(&b.1));

        let rows: Vec<Row> = items
            .iter()
            .map(|(kind, val)| {
                Row::new(vec![
                    Cell::from(kind.to_string()).style(if *kind == "IP" {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Magenta)
                    }),
                    Cell::from(val.clone()),
                ])
            })
            .collect();

        Table::new(
            rows,
            [Constraint::Percentage(30), Constraint::Percentage(70)],
        )
        .header(
            Row::new(vec!["Type", "Value"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 40))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Blocked Items ")
                .borders(Borders::ALL)
                .border_style(if self.active_panel == Panel::Blocked {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        )
    }

    /// HH:MM:SS (UTC) from a SystemTime, without a chrono dependency.
    fn fmt_event_time(t: std::time::SystemTime) -> String {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() % 86400)
            .unwrap_or(0);
        format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }

    fn render_logs(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let rows: Vec<Row> = snapshot
            .logs
            .iter()
            .map(|ev| {
                let (dir_sym, dir_style) = match ev.dir {
                    "IN" => ("→", Style::default().fg(Color::Cyan)),
                    "OUT" => ("←", Style::default().fg(Color::Green)),
                    _ => ("⟳", Style::default().fg(Color::Yellow)),
                };
                let backend = ev
                    .backend
                    .as_deref()
                    .map(|u| u.replace("http://", "").replace("https://", ""))
                    .unwrap_or_else(|| "-".into());
                let info_style = if ev.info.starts_with("dropped") {
                    Style::default().fg(Color::Red)
                } else if ev.info.contains("rejected") {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::Gray)
                };
                // Append a short single-line content preview when present.
                let info_text = match ev.content.as_deref().filter(|c| !c.is_empty()) {
                    Some(c) => format!(
                        "{} [{}]",
                        ev.info,
                        c.lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(30)
                            .collect::<String>()
                    ),
                    None => ev.info.clone(),
                };
                Row::new(vec![
                    Cell::from(Self::fmt_event_time(ev.at))
                        .style(Style::default().fg(Color::DarkGray)),
                    Cell::from(dir_sym).style(dir_style),
                    Cell::from(ev.user.clone()).style(Style::default().fg(Color::White)),
                    Cell::from(ev.model.clone().unwrap_or_else(|| "-".into()))
                        .style(Style::default().fg(Color::Cyan)),
                    Cell::from(backend).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(info_text).style(info_style),
                ])
            })
            .collect();

        Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(3),
                Constraint::Percentage(18),
                Constraint::Percentage(28),
                Constraint::Percentage(19),
                Constraint::Percentage(22),
            ],
        )
        .header(
            Row::new(vec!["Time", " ", "User", "Model", "Backend", "Info"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .block(
            Block::default()
                .title(" Requests (newest first) ")
                .borders(Borders::ALL)
                .border_style(if self.active_panel == Panel::Logs {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        )
    }

    fn render_help(&self, snapshot: &StateSnapshot) -> Paragraph<'static> {
        let base = " h/l/Tab: Panel (Tab: model when expanded) | j/k: Nav | Space: Expand | L/U: Load/Unload | r: Reload appconf.yaml | ?: Help | q: Quit";

        let line = if let Some(mode) = &self.input_mode {
            let url = snapshot
                .backends
                .get(match mode {
                    InputMode::Load { backend_idx } | InputMode::Unload { backend_idx } => {
                        *backend_idx
                    }
                })
                .map(|b| b.url.replace("http://", "").replace("https://", ""))
                .unwrap_or_default();
            let verb = match mode {
                InputMode::Load { .. } => "Load",
                InputMode::Unload { .. } => "Unload",
            };
            Line::from(vec![
                Span::styled(
                    format!(" {} model on {} [", verb, url),
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Span::styled(self.input_buf.clone(), Style::default().fg(Color::White)),
                Span::styled(
                    "]  Enter: send | Esc: cancel",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else if let Some(flash) = self
            .flash
            .as_ref()
            .filter(|f| f.at.elapsed() < std::time::Duration::from_secs(5))
        {
            Line::from(vec![
                Span::styled(
                    flash.text.clone(),
                    if flash.ok {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red)
                    },
                ),
                Span::raw("   "),
                Span::styled(base, Style::default().fg(Color::Gray)),
            ])
        } else {
            Line::from(base)
        };

        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title_bottom(
            Line::from(format!(" v{} ", env!("CARGO_PKG_VERSION"))).alignment(Alignment::Right),
        ))
    }

    fn render_detailed_help(&self) -> Paragraph<'static> {
        Paragraph::new("\n  EXPAND MODELS: 'Space' or 'Enter' (in Backends panel)\n  CYCLE MODELS: 'Tab' / 'Shift+Tab' on an expanded backend (▶ cursor); 'L'/'U' then act on the cursor model directly\n  SHOW ALL MODELS: 'a' (in Backends panel)\n  MODEL CONTROL: 'L' load / 'U' unload (Backends panel)\n  CONFIG: 'r' re-reads appconf.yaml and re-applies it (loads listed models on their backends)\n  LOGS: bottom panel shows requests in (→) / out (←) and control ops (⟳), newest first\n  VIP: 'p' | BOOST: 'b' | BLOCK: 'x' (User) / 'X' (IP) | UNBLOCK: 'u'\n  PANELS: 'Tab' | QUIT: 'q' or 'Esc'\n\n  ★ VIP | ⚡ Boost | ✖ Blocked | ▶ Processing / cursor | ● Queued | ⟳ Control op in progress")
            .block(Block::default().title(" Help ").borders(Borders::ALL))
            .style(Style::default().fg(Color::Gray))
    }
}
