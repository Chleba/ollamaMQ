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
use std::io;
use std::sync::Arc;

use crate::dispatcher::AppState;

pub struct TuiDashboard {
    table_state: TableState,
    show_help: bool,
}

impl TuiDashboard {
    pub fn new() -> Self {
        Self {
            table_state: TableState::default(),
            show_help: false,
        }
    }

    pub fn run(&mut self, state: &Arc<AppState>) -> io::Result<bool> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;

        loop {
            terminal.draw(|f| self.render(f, state)).unwrap();

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
                        KeyCode::Up | KeyCode::Char('k') => {
                            let i = self.table_state.selected().unwrap_or(0).saturating_sub(1);
                            self.table_state.select(Some(i));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let len = {
                                let queues = state.queues.lock().unwrap();
                                queues.len()
                            };
                            if len > 0 {
                                let i = self
                                    .table_state
                                    .selected()
                                    .map(|s| (s + 1).min(len.saturating_sub(1)))
                                    .unwrap_or(0);
                                self.table_state.select(Some(i));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, state: &Arc<AppState>) {
        let area = f.area();
        
        // Vertical layout: Stats (top), Content (middle), Help (bottom)
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Stats
                Constraint::Min(0),    // Content
                Constraint::Length(3), // Help bar
                if self.show_help { Constraint::Length(8) } else { Constraint::Length(0) }, // Detailed Help
            ])
            .split(area);

        // Render Stats
        f.render_widget(self.render_stats(state), main_chunks[0]);

        // Middle Content: Horizontal split (Users left, Queues right)
        let content_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(60),
            ])
            .split(main_chunks[1]);

        // Render Users Table
        let users_table = self.render_users(state);
        f.render_stateful_widget(users_table, content_chunks[0], &mut self.table_state);

        // Render Queues Table (using same state for sync scrolling)
        let queues_table = self.render_queues(state, content_chunks[1].width);
        f.render_stateful_widget(queues_table, content_chunks[1], &mut self.table_state);

        // Render Help Bar (now also showing version)
        f.render_widget(self.render_help(), main_chunks[2]);

        // Render Detailed Help if toggled
        if self.show_help {
            f.render_widget(self.render_detailed_help(), main_chunks[3]);
        }
    }

    fn render_stats(&self, state: &Arc<AppState>) -> Paragraph<'_> {
        let queues = state.queues.lock().unwrap();
        let counts = state.processed_counts.lock().unwrap();
        let dropped = state.dropped_counts.lock().unwrap();
        let user_count = queues.len();
        let total_queued: usize = queues.values().map(|q| q.len()).sum();
        let total_processed: usize = counts.values().sum();
        let total_dropped: usize = dropped.values().sum();

        let content = Line::from(vec![
            Span::styled(" ollamaMQ Dashboard ", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" | "),
            Span::styled("Users: ", Style::default().fg(Color::White)),
            Span::styled(user_count.to_string(), Style::default().fg(Color::White).bold()),
            Span::raw(" | "),
            Span::styled("Queued: ", Style::default().fg(Color::Yellow)),
            Span::styled(total_queued.to_string(), Style::default().fg(Color::Yellow).bold()),
            Span::raw(" | "),
            Span::styled("Processed: ", Style::default().fg(Color::Green)),
            Span::styled(total_processed.to_string(), Style::default().fg(Color::Green).bold()),
            Span::raw(" | "),
            Span::styled("Dropped: ", Style::default().fg(Color::Red)),
            Span::styled(total_dropped.to_string(), Style::default().fg(Color::Red).bold()),
        ]);

        Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL))
    }

    fn render_users(&self, state: &Arc<AppState>) -> Table<'static> {
        let queues = state.queues.lock().unwrap();
        let counts = state.processed_counts.lock().unwrap();
        let dropped_counts = state.dropped_counts.lock().unwrap();
        let mut users: Vec<_> = queues.keys().cloned().collect();
        users.sort_by(|a, b| {
            let a_q = queues.get(a).map(|q| q.len()).unwrap_or(0);
            let b_q = queues.get(b).map(|q| q.len()).unwrap_or(0);
            let a_p = counts.get(a).cloned().unwrap_or(0);
            let b_p = counts.get(b).cloned().unwrap_or(0);
            let a_d = dropped_counts.get(a).cloned().unwrap_or(0);
            let b_d = dropped_counts.get(b).cloned().unwrap_or(0);

            b_q.cmp(&a_q)
                .then_with(|| (b_p + b_d).cmp(&(a_p + a_d)))
                .then_with(|| a.cmp(b))
        });

        let rows: Vec<Row> = users
            .iter()
            .map(|user| {
                let queue_len = queues.get(user).map(|q| q.len()).unwrap_or(0);
                let processed = counts.get(user).cloned().unwrap_or(0);
                let dropped = dropped_counts.get(user).cloned().unwrap_or(0);
                
                let (status_symbol, status_style) = if queue_len > 0 {
                    ("● ", Style::default().fg(Color::Green))
                } else {
                    ("○ ", Style::default().fg(Color::DarkGray))
                };

                let queue_style = if queue_len > 5 { 
                    Style::default().fg(Color::Red) 
                } else if queue_len > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };

                Row::new(vec![
                    Cell::from(Line::from(vec![
                        Span::styled(status_symbol, status_style),
                        Span::styled(user.clone(), Style::default().fg(Color::White)),
                    ])),
                    Cell::from(queue_len.to_string()).style(queue_style),
                    Cell::from(processed.to_string()).style(Style::default().fg(Color::Green)),
                    Cell::from(dropped.to_string()).style(Style::default().fg(Color::Red)),
                ])
            })
            .collect();

        Table::new(
            rows,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ],
        )
        .header(
            Row::new(vec!["User ID", "Queued", "Done", "Drop"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Active Users ")
                .borders(Borders::ALL)
        )
    }

    fn render_queues(&self, state: &Arc<AppState>, available_width: u16) -> Table<'static> {
        let queues = state.queues.lock().unwrap();
        let counts = state.processed_counts.lock().unwrap();
        let total_queued: usize = queues.values().map(|q| q.len()).sum();

        // Column widths for visualization
        let col_widths = [
            Constraint::Percentage(25),
            Constraint::Percentage(50),
            Constraint::Percentage(25),
        ];
        
        // Approximate width of the visualization column in characters
        let bar_max_width = ((available_width as f32) * 0.5) as usize;
        let max_queue_threshold = 20;

        let mut users: Vec<_> = queues.keys().cloned().collect();
        users.sort_by(|a, b| {
            let a_q = queues.get(a).map(|q| q.len()).unwrap_or(0);
            let b_q = queues.get(b).map(|q| q.len()).unwrap_or(0);
            let a_p = counts.get(a).cloned().unwrap_or(0);
            let b_p = counts.get(b).cloned().unwrap_or(0);

            b_q.cmp(&a_q)
                .then_with(|| b_p.cmp(&a_p))
                .then_with(|| a.cmp(b))
        });

        let rows: Vec<Row> = users
            .iter()
            .map(|user| {
                let queue_len = queues.get(user).map(|q| q.len()).unwrap_or(0);
                
                // Calculate fill percentage relative to threshold
                let fill_ratio = (queue_len as f32 / max_queue_threshold as f32).min(1.0);
                let bar_len = (fill_ratio * bar_max_width as f32) as usize;
                
                // Colors change based on column fill percentage - more sensitive thresholds
                let bar_color = if fill_ratio >= 0.5 {
                    Color::LightRed
                } else if fill_ratio >= 0.2 {
                    Color::Yellow
                } else if fill_ratio > 0.0 {
                    Color::Green
                } else {
                    Color::DarkGray
                };

                let bar_str = "⠿".repeat(bar_len);
                // Padded with spaces to fill the width (ensures background highlight works well)
                let bar_padded = format!("{:<width$}", bar_str, width = bar_max_width);
                
                let percentage = if total_queued > 0 {
                    (queue_len as f64 / total_queued as f64) * 100.0
                } else {
                    0.0
                };
                let num_str = format!("{} ({:.1}%)", queue_len, percentage);

                Row::new(vec![
                    Cell::from(user.clone()).style(Style::default().fg(Color::Gray)),
                    Cell::from(bar_padded).style(Style::default().fg(bar_color)),
                    Cell::from(num_str).style(Style::default().fg(bar_color).bold()),
                ])
            })
            .collect();

        Table::new(rows, col_widths)
        .header(
            Row::new(vec!["User ID", "Progress", "Num (%)"])
                .style(Style::default().fg(Color::Yellow).bold())
                .bottom_margin(1),
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ")
        .block(
            Block::default()
                .title(" Queue Status ")
                .borders(Borders::ALL)
        )
    }

    fn render_help(&self) -> Paragraph<'_> {
        let version = env!("CARGO_PKG_VERSION");
        let version_span = Span::styled(
            format!(" v{} ", version),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        );

        Paragraph::new(" Press '?' for help, 'q' to quit, 'j/k' to scroll")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title_bottom(
                        Line::from(version_span).alignment(Alignment::Right)
                    ),
            )
            .style(Style::default().fg(Color::White))
    }

    fn render_detailed_help(&self) -> Paragraph<'_> {
        let help_text = "
  QUIT:    'q' or 'Esc'
  HELP:    '?' (toggle this view)
  SCROLL:  'j' / 'Down' | 'k' / 'Up'
  
  VISUALS: ⠿ (Queue status bar)
           Colors change based on load (Green -> Yellow -> Red)
";
        Paragraph::new(help_text)
            .block(Block::default().title(" Help & Keybindings ").borders(Borders::ALL))
            .style(Style::default().fg(Color::Gray))
    }
}
