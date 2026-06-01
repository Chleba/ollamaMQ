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

use crate::dispatcher::{AppState, BackendApiType, BackendStatus};

#[derive(PartialEq)]
enum Panel {
    Backends,
    Users,
    Blocked,
}

struct StateSnapshot {
    queues_len: HashMap<String, usize>,
    processing_counts: HashMap<String, usize>,
    processed_counts: HashMap<String, usize>,
    dropped_counts: HashMap<String, usize>,
    user_ips: HashMap<String, IpAddr>,
    blocked_ips: HashSet<IpAddr>,
    blocked_users: HashSet<String>,
    vip_user: Option<String>,
    boost_user: Option<String>,
    user_ids: Vec<String>,
    backends: Vec<BackendStatus>,
}

pub struct TuiDashboard {
    table_state: TableState,
    backend_table_state: TableState,
    blocked_table_state: TableState,
    active_panel: Panel,
    expanded_backends: HashSet<String>,
    show_help: bool,
}

impl TuiDashboard {
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            backend_table_state: TableState::default(),
            blocked_table_state: TableState::default(),
            active_panel: Panel::Users,
            expanded_backends: HashSet::new(),
            show_help: false,
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
        let user_ips = state.user_ips.lock().unwrap().clone();
        let blocked_ips = state.blocked_ips.lock().unwrap().clone();
        let blocked_users = state.blocked_users.lock().unwrap().clone();
        let vip_user = state.vip_user.lock().unwrap().clone();
        let boost_user = state.boost_user.lock().unwrap().clone();
        let backends = state.backends.lock().unwrap().clone();

        let mut user_ids: Vec<String> = queues_len.keys().cloned().collect();
        user_ids.sort_by(|a, b| {
            let a_q = queues_len.get(a).unwrap_or(&0) + processing_counts.get(a).unwrap_or(&0);
            let b_q = queues_len.get(b).unwrap_or(&0) + processing_counts.get(b).unwrap_or(&0);
            let a_total = processed_counts.get(a).unwrap_or(&0) + dropped_counts.get(a).unwrap_or(&0);
            let b_total = processed_counts.get(b).unwrap_or(&0) + dropped_counts.get(b).unwrap_or(&0);

            b_q.cmp(&a_q)
                .then_with(|| b_total.cmp(&a_total))
                .then_with(|| a.cmp(b))
        });

        StateSnapshot {
            queues_len,
            processing_counts,
            processed_counts,
            dropped_counts,
            user_ips,
            blocked_ips,
            blocked_users,
            vip_user,
            boost_user,
            user_ids,
            backends,
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
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            io::stdout().execute(LeaveAlternateScreen)?;
                            disable_raw_mode()?;
                            terminal.show_cursor()?;
                            return Ok(false);
                        }
                        KeyCode::Char('?') => self.show_help = !self.show_help,
                        KeyCode::Tab | KeyCode::Char('l') => {
                            self.active_panel = match self.active_panel {
                                Panel::Backends => Panel::Users,
                                Panel::Users => Panel::Blocked,
                                Panel::Blocked => Panel::Backends,
                            };
                        }
                        KeyCode::Char('h') => {
                            self.active_panel = match self.active_panel {
                                Panel::Backends => Panel::Blocked,
                                Panel::Users => Panel::Backends,
                                Panel::Blocked => Panel::Users,
                            };
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
                                let i = self.backend_table_state.selected().unwrap_or(0).saturating_sub(1);
                                self.backend_table_state.select(Some(i));
                            } else if self.active_panel == Panel::Users {
                                let i = self.table_state.selected().unwrap_or(0).saturating_sub(1);
                                self.table_state.select(Some(i));
                            } else {
                                let i = self.blocked_table_state.selected().unwrap_or(0).saturating_sub(1);
                                self.blocked_table_state.select(Some(i));
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if self.active_panel == Panel::Backends {
                                let len = snapshot.backends.len();
                                if len > 0 {
                                    let i = self.backend_table_state.selected().map(|s| (s + 1).min(len.saturating_sub(1))).unwrap_or(0);
                                    self.backend_table_state.select(Some(i));
                                }
                            } else if self.active_panel == Panel::Users {
                                let len = snapshot.user_ids.len();
                                if len > 0 {
                                    let i = self.table_state.selected().map(|s| (s + 1).min(len.saturating_sub(1))).unwrap_or(0);
                                    self.table_state.select(Some(i));
                                }
                            } else {
                                let len = snapshot.blocked_ips.len() + snapshot.blocked_users.len();
                                if len > 0 {
                                    let i = self.blocked_table_state.selected().map(|s| (s + 1).min(len.saturating_sub(1))).unwrap_or(0);
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

    fn render(&mut self, f: &mut Frame, snapshot: &StateSnapshot) {
        if self.active_panel == Panel::Backends {
            if snapshot.backends.is_empty() {
                self.backend_table_state.select(None);
            } else if self.backend_table_state.selected().is_none() {
                self.backend_table_state.select(Some(0));
            }
        } else if self.active_panel == Panel::Users {
            if snapshot.user_ids.is_empty() {
                self.table_state.select(None);
            } else if self.table_state.selected().is_none() {
                self.table_state.select(Some(0));
            }
        } else {
            let blocked_total = snapshot.blocked_ips.len() + snapshot.blocked_users.len();
            if blocked_total == 0 {
                self.blocked_table_state.select(None);
            } else if self.blocked_table_state.selected().is_none() {
                self.blocked_table_state.select(Some(0));
            }
        }

        let area = f.area();
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Stats
                Constraint::Min(0),    // Content
                Constraint::Length(3), // Help bar
                if self.show_help { Constraint::Length(12) } else { Constraint::Length(0) },
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

        f.render_stateful_widget(self.render_backends(snapshot), content_chunks[0], &mut self.backend_table_state);
        f.render_stateful_widget(self.render_users(snapshot), content_chunks[1], &mut self.table_state);

        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(content_chunks[2]);

        f.render_stateful_widget(self.render_queues(snapshot, right_chunks[0].width), right_chunks[0], &mut self.table_state);
        f.render_stateful_widget(self.render_blocked(snapshot), right_chunks[1], &mut self.blocked_table_state);

        f.render_widget(self.render_help(), main_chunks[2]);
        if self.show_help {
            f.render_widget(self.render_detailed_help(), main_chunks[3]);
        }
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
            Span::styled(if self.active_panel == Panel::Users { "USERS" } else { "BLOCKED" }, Style::default().fg(Color::Yellow).bold()),
            Span::raw(" | "),
            Span::styled("VIP: ", Style::default().fg(Color::Magenta)),
            Span::styled(snapshot.vip_user.clone().unwrap_or_else(|| "None".to_string()), Style::default().fg(Color::Magenta).bold()),
            Span::raw(" | "),
            Span::styled("Boost: ", Style::default().fg(Color::Yellow)),
            Span::styled(snapshot.boost_user.clone().unwrap_or_else(|| "None".to_string()), Style::default().fg(Color::Yellow).bold()),
            Span::raw(" | "),
            Span::styled("Q: ", Style::default().fg(Color::Yellow)),
            Span::styled((total_queued + total_processing).to_string(), Style::default().fg(Color::Yellow).bold()),
            Span::raw(" | "),
            Span::styled("Done: ", Style::default().fg(Color::Green)),
            Span::styled(total_processed.to_string(), Style::default().fg(Color::Green).bold()),
            Span::raw(" | "),
            Span::styled("Drop: ", Style::default().fg(Color::Red)),
            Span::styled(total_dropped.to_string(), Style::default().fg(Color::Red).bold()),
        ];

        Paragraph::new(Line::from(stats_line)).block(Block::default().borders(Borders::ALL))
    }

    fn render_backends(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let rows: Vec<Row> = snapshot.backends.iter().map(|b| {
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

            let mut name_lines = vec![
                Line::from(vec![
                    Span::styled(if is_expanded { "▼ " } else { "▶ " }, Style::default().fg(Color::DarkGray)),
                    Span::styled(status_sym, status_style),
                    Span::styled(url, if b.is_online { Style::default().fg(Color::White) } else { Style::default().fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT) }),
                ])
            ];

            // Display Active or Last used model on a new line
            if let Some(model) = b.current_model.clone() {
                let prefix = if b.active_requests > 0 { "  ▶ Active: " } else { "  ↺ Last:   " };
                let color = if b.active_requests > 0 { Color::Cyan } else { Color::DarkGray };
                name_lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(color)),
                    Span::styled(model, Style::default().fg(color).bold()),
                ]));
            }

            if is_expanded {
                let mut models: Vec<String> = b.available_models.iter().cloned().collect();
                models.sort();
                
                if models.is_empty() {
                    name_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("└ (No models discovered yet)", Style::default().fg(Color::DarkGray).italic()),
                    ]));
                } else {
                    let total_models = models.len();
                    for m in models.into_iter().take(5) {
                        let is_loaded = b.loaded_models.contains(&m);
                        let m_style = if is_loaded {
                            Style::default().fg(Color::Green).bold()
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };

                        name_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled("└ ", Style::default().fg(Color::DarkGray)),
                            Span::styled(m, m_style),
                            if is_loaded { Span::styled(" (In RAM)", Style::default().fg(Color::Green).italic()) } else { Span::raw("") },
                        ]));
                    }
                    if total_models > 5 {
                        name_lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(format!("  ... and {} more", total_models - 5), Style::default().fg(Color::DarkGray).italic()),
                        ]));
                    }
                }
            }

            let height = name_lines.len() as u16;

            Row::new(vec![
                Cell::from(Text::from(name_lines)),
                Cell::from(type_str).style(type_style),
                Cell::from(b.active_requests.to_string()).style(req_style),
                Cell::from(b.processed_count.to_string()).style(Style::default().fg(Color::DarkGray)),
            ]).height(height)
        }).collect();

        Table::new(rows, [
            Constraint::Min(15),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(6),
        ])
        .header(Row::new(vec!["Backend", "API", "Act", "Done"]).style(Style::default().fg(Color::Yellow).bold()).bottom_margin(1))
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ")
        .block(Block::default().title(" Backend Instances ").borders(Borders::ALL).border_style(if self.active_panel == Panel::Backends { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }))
    }

    fn render_users(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let rows: Vec<Row> = snapshot.user_ids.iter().map(|user| {
            let queue_len = snapshot.queues_len.get(user).unwrap_or(&0) + snapshot.processing_counts.get(user).unwrap_or(&0);
            let processed = snapshot.processed_counts.get(user).unwrap_or(&0);
            let dropped = snapshot.dropped_counts.get(user).unwrap_or(&0);
            let ip_str = snapshot.user_ips.get(user).map(|i| i.to_string()).unwrap_or_default();
            let is_blocked = snapshot.blocked_users.contains(user) || snapshot.user_ips.get(user).map_or(false, |ip| snapshot.blocked_ips.contains(ip));
            let is_vip = snapshot.vip_user.as_ref() == Some(user);
            let is_boost = snapshot.boost_user.as_ref() == Some(user);

            let (sym, style) = if is_blocked { ("✖ ", Style::default().fg(Color::Red)) }
                              else if is_vip { ("★ ", Style::default().fg(Color::Magenta)) }
                              else if is_boost { ("⚡", Style::default().fg(Color::Yellow)) }
                              else if *snapshot.processing_counts.get(user).unwrap_or(&0) > 0 { ("▶ ", Style::default().fg(Color::Cyan)) }
                              else if *snapshot.queues_len.get(user).unwrap_or(&0) > 0 { ("● ", Style::default().fg(Color::Green)) }
                              else { ("○ ", Style::default().fg(Color::DarkGray)) };

            let mut spans = vec![Span::styled(sym, style), Span::styled(user.clone(), if is_blocked { Style::default().fg(Color::Red).add_modifier(Modifier::CROSSED_OUT) } else if is_vip { Style::default().fg(Color::Magenta).bold() } else if is_boost { Style::default().fg(Color::Yellow).bold() } else { Style::default().fg(Color::White) })];
            if is_vip { spans.push(Span::styled(" [VIP]", Style::default().fg(Color::Magenta).bold())); }
            if is_boost { spans.push(Span::styled(" [BST]", Style::default().fg(Color::Yellow).bold())); }
            if is_blocked { spans.push(Span::styled(" [BLOCKED]", Style::default().fg(Color::Red).bold())); }

            Row::new(vec![Cell::from(Line::from(spans)), Cell::from(ip_str).style(Style::default().fg(Color::Cyan)), Cell::from(queue_len.to_string()), Cell::from(processed.to_string()), Cell::from(dropped.to_string())])
        }).collect();

        Table::new(rows, [Constraint::Percentage(45), Constraint::Percentage(25), Constraint::Percentage(10), Constraint::Percentage(10), Constraint::Percentage(10)])
            .header(Row::new(vec!["User ID", "Last IP", "Q", "Done", "Drop"]).style(Style::default().fg(Color::Yellow).bold()).bottom_margin(1))
            .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
            .highlight_symbol(">> ")
            .block(Block::default().title(" Active Users ").borders(Borders::ALL).border_style(if self.active_panel == Panel::Users { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }))
    }

    fn render_queues(&self, snapshot: &StateSnapshot, available_width: u16) -> Table<'static> {
        let total_queued = snapshot.queues_len.values().sum::<usize>() + snapshot.processing_counts.values().sum::<usize>();
        let bar_max_width = ((available_width as f32) * 0.45) as usize;

        let rows: Vec<Row> = snapshot.user_ids.iter().map(|user| {
            let q_len = snapshot.queues_len.get(user).unwrap_or(&0) + snapshot.processing_counts.get(user).unwrap_or(&0);
            let bar_len = if q_len > 0 { ((q_len as f32 / 20.0).min(1.0) * bar_max_width as f32) as usize } else { 0 };
            let color = if snapshot.vip_user.as_ref() == Some(user) { Color::Magenta } else if snapshot.boost_user.as_ref() == Some(user) { Color::Yellow } else if *snapshot.processing_counts.get(user).unwrap_or(&0) > 0 { Color::Cyan } else { Color::Green };
            let bar = format!("{:<width$}", "⠿".repeat(bar_len), width = bar_max_width);
            let pct = if total_queued > 0 { (q_len as f64 / total_queued as f64) * 100.0 } else { 0.0 };
            Row::new(vec![Cell::from(user.clone()), Cell::from(bar).style(Style::default().fg(color)), Cell::from(format!("{} ({:.0}%)", q_len, pct)).style(Style::default().fg(color).bold())])
        }).collect();

        Table::new(rows, [Constraint::Percentage(30), Constraint::Percentage(45), Constraint::Percentage(25)])
            .header(Row::new(vec!["User ID", "Progress", "Num"]).style(Style::default().fg(Color::Yellow).bold()).bottom_margin(1))
            .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
            .highlight_symbol(">> ")
            .block(Block::default().title(" Queue Status ").borders(Borders::ALL))
    }

    fn render_blocked(&self, snapshot: &StateSnapshot) -> Table<'static> {
        let mut items = Vec::new();
        for ip in snapshot.blocked_ips.iter() { items.push(("IP", ip.to_string())); }
        for user in snapshot.blocked_users.iter() { items.push(("USER", user.clone())); }
        items.sort_by(|a, b| a.1.cmp(&b.1));

        let rows: Vec<Row> = items.iter().map(|(kind, val)| Row::new(vec![Cell::from(kind.to_string()).style(if *kind == "IP" { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::Magenta) }), Cell::from(val.clone())])).collect();

        Table::new(rows, [Constraint::Percentage(30), Constraint::Percentage(70)])
            .header(Row::new(vec!["Type", "Value"]).style(Style::default().fg(Color::Yellow).bold()).bottom_margin(1))
            .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
            .highlight_symbol(">> ")
            .block(Block::default().title(" Blocked Items ").borders(Borders::ALL).border_style(if self.active_panel == Panel::Blocked { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) }))
    }

    fn render_help(&self) -> Paragraph<'static> {
        Paragraph::new(" h/l/Tab: Switch Panel | j/k: Nav | Space/Enter: Expand Models | p: VIP | b: Boost | q: Quit")
            .block(Block::default().borders(Borders::ALL).title_bottom(Line::from(format!(" v{} ", env!("CARGO_PKG_VERSION"))).alignment(Alignment::Right)))
    }

    fn render_detailed_help(&self) -> Paragraph<'static> {
        Paragraph::new("\n  EXPAND MODELS: 'Space' or 'Enter' (in Backends panel)\n  VIP: 'p' | BOOST: 'b' | BLOCK: 'x' (User) / 'X' (IP) | UNBLOCK: 'u'\n  PANELS: 'Tab' | QUIT: 'q' or 'Esc'\n\n  ★ VIP | ⚡ Boost | ✖ Blocked | ▶ Processing | ● Queued").block(Block::default().title(" Help ").borders(Borders::ALL)).style(Style::default().fg(Color::Gray))
    }
}
