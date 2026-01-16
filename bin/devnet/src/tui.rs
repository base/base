//! Terminal UI for the devnet using ratatui.
//!
//! Provides a Montana-style interface with:
//! - Split log panels for client and builder
//! - Status bar with network info
//! - Real-time log streaming

use std::{
    collections::VecDeque,
    fs,
    io::{self, Stdout},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

/// Maximum number of log lines to keep per panel.
const MAX_LOG_LINES: usize = 1000;

/// Shared state for log lines from processes.
#[derive(Debug, Default)]
pub(crate) struct LogState {
    pub client_logs: VecDeque<String>,
    pub builder_logs: VecDeque<String>,
    pub client_running: bool,
    pub builder_running: bool,
    pub client_pid: Option<u32>,
    pub builder_pid: Option<u32>,
    pub start_time: Option<Instant>,
    pub latest_block: Option<u64>,
}

impl LogState {
    pub(crate) fn new() -> Self {
        Self {
            client_logs: VecDeque::with_capacity(MAX_LOG_LINES),
            builder_logs: VecDeque::with_capacity(MAX_LOG_LINES),
            client_running: false,
            builder_running: false,
            client_pid: None,
            builder_pid: None,
            start_time: Some(Instant::now()),
            latest_block: None,
        }
    }

    pub(crate) fn push_client_log(&mut self, line: String) {
        // Parse block number from log lines like "Produced block 23" or "block_number=23"
        if let Some(block_num) = parse_block_number(&line) {
            self.latest_block = Some(block_num);
        }
        // Filter out noisy repetitive messages
        if is_noisy_log(&line) {
            return;
        }
        if self.client_logs.len() >= MAX_LOG_LINES {
            self.client_logs.pop_front();
        }
        self.client_logs.push_back(line);
    }

    pub(crate) fn push_builder_log(&mut self, line: String) {
        // Filter out noisy repetitive messages
        if is_noisy_log(&line) {
            return;
        }
        if self.builder_logs.len() >= MAX_LOG_LINES {
            self.builder_logs.pop_front();
        }
        self.builder_logs.push_back(line);
    }

    pub(crate) fn uptime(&self) -> Duration {
        self.start_time.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Check if processes are still alive and update running status.
    pub(crate) fn update_process_status(&mut self) {
        if let Some(pid) = self.client_pid
            && !is_process_alive(pid) {
                self.client_running = false;
                self.client_pid = None; // Clear so we don't keep checking
            }
        if let Some(pid) = self.builder_pid
            && !is_process_alive(pid) {
                self.builder_running = false;
                self.builder_pid = None;
            }
    }
}

/// Check if a log line is noisy/repetitive and should be filtered out.
fn is_noisy_log(line: &str) -> bool {
    let dominated_patterns = [
        "Forkchoice updated",
        "forkchoice_updated",
        "New payload job created",
        "Received block from consensus",
        "Canonical chain committed",
    ];
    for pattern in dominated_patterns {
        if line.contains(pattern) {
            return true;
        }
    }
    false
}

/// Parse block number from log lines.
/// Looks for patterns like "Block added to canonical chain number=N" or "Produced block N"
fn parse_block_number(line: &str) -> Option<u64> {
    // Strip ANSI codes first - logs contain escape sequences like [3m, [0m, etc.
    let clean = strip_ansi(line);

    // Pattern: "number=N" (reth log format, e.g., "Block added to canonical chain number=11")
    if let Some(idx) = clean.find("number=") {
        let rest = &clean[idx + 7..];
        let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num_str.parse() {
            return Some(n);
        }
    }
    // Pattern: "Produced block N" (block driver debug output)
    if let Some(idx) = clean.find("Produced block ") {
        let rest = &clean[idx + 15..];
        let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num_str.parse() {
            return Some(n);
        }
    }
    None
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip escape sequence
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Check if a process with the given PID is still alive.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks if process exists without sending a signal
    // Returns 0 if process exists, -1 with ESRCH if not
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    // Can't easily check on non-Unix, assume alive
    true
}

/// Which panel is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Client,
    Builder,
}

impl Focus {
    pub(crate) const fn toggle(self) -> Self {
        match self {
            Self::Client => Self::Builder,
            Self::Builder => Self::Client,
        }
    }
}

/// TUI application state.
pub(crate) struct App {
    pub log_state: Arc<Mutex<LogState>>,
    pub focus: Focus,
    pub client_list_state: ListState,
    pub builder_list_state: ListState,
    pub should_quit: bool,
    pub should_restart: bool,
    pub chain_id: u64,
    #[allow(dead_code)] // Reserved for future RPC info display
    pub http_port: u16,
    #[allow(dead_code)] // Reserved for future RPC info display
    pub ws_port: u16,
    pub auto_scroll: bool,
    pub status_message: Option<(String, Instant)>,
}

impl App {
    pub(crate) fn new(
        log_state: Arc<Mutex<LogState>>,
        chain_id: u64,
        http_port: u16,
        ws_port: u16,
    ) -> Self {
        Self {
            log_state,
            focus: Focus::Client,
            client_list_state: ListState::default(),
            builder_list_state: ListState::default(),
            should_quit: false,
            should_restart: false,
            chain_id,
            http_port,
            ws_port,
            auto_scroll: true,
            status_message: None,
        }
    }

    /// Handle keyboard input.
    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => self.focus = self.focus.toggle(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.auto_scroll = false;
                self.scroll_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down();
            }
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.page_up();
            }
            KeyCode::PageDown => {
                self.page_down();
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.auto_scroll = false;
                self.scroll_to_top();
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.auto_scroll = true;
                self.scroll_to_bottom();
            }
            KeyCode::Char('f') => {
                self.auto_scroll = !self.auto_scroll;
            }
            KeyCode::Char('r') => {
                self.should_restart = true;
            }
            KeyCode::Char('s') => {
                self.save_logs();
            }
            _ => {}
        }
    }

    fn scroll_up(&mut self) {
        let state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        let i = state.selected().unwrap_or(0);
        state.select(Some(i.saturating_sub(1)));
    }

    fn scroll_down(&mut self) {
        let log_len = {
            let state = self.log_state.lock().unwrap();
            match self.focus {
                Focus::Client => state.client_logs.len(),
                Focus::Builder => state.builder_logs.len(),
            }
        };
        let list_state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        let i = list_state.selected().unwrap_or(0);
        if i < log_len.saturating_sub(1) {
            list_state.select(Some(i + 1));
        }
    }

    fn page_up(&mut self) {
        let state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        let i = state.selected().unwrap_or(0);
        state.select(Some(i.saturating_sub(20)));
    }

    fn page_down(&mut self) {
        let log_len = {
            let state = self.log_state.lock().unwrap();
            match self.focus {
                Focus::Client => state.client_logs.len(),
                Focus::Builder => state.builder_logs.len(),
            }
        };
        let list_state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        let i = list_state.selected().unwrap_or(0);
        let new_i = (i + 20).min(log_len.saturating_sub(1));
        list_state.select(Some(new_i));
    }

    fn scroll_to_top(&mut self) {
        let state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        state.select(Some(0));
    }

    fn scroll_to_bottom(&mut self) {
        let log_len = {
            let state = self.log_state.lock().unwrap();
            match self.focus {
                Focus::Client => state.client_logs.len(),
                Focus::Builder => state.builder_logs.len(),
            }
        };
        let list_state = match self.focus {
            Focus::Client => &mut self.client_list_state,
            Focus::Builder => &mut self.builder_list_state,
        };
        if log_len > 0 {
            list_state.select(Some(log_len - 1));
        }
    }

    /// Save logs to files.
    fn save_logs(&mut self) {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let client_path = PathBuf::from(format!("devnet_client_{}.log", timestamp));
        let builder_path = PathBuf::from(format!("devnet_builder_{}.log", timestamp));

        let state = self.log_state.lock().unwrap();

        // Save client logs
        let client_content: String = state.client_logs.iter().map(|s| format!("{}\n", s)).collect();
        let builder_content: String =
            state.builder_logs.iter().map(|s| format!("{}\n", s)).collect();
        drop(state);

        let mut messages = Vec::new();

        match fs::write(&client_path, client_content) {
            Ok(_) => messages.push(format!("client: {}", client_path.display())),
            Err(e) => messages.push(format!("client err: {}", e)),
        }

        match fs::write(&builder_path, builder_content) {
            Ok(_) => messages.push(format!("builder: {}", builder_path.display())),
            Err(e) => messages.push(format!("builder err: {}", e)),
        }

        self.status_message = Some((format!("Saved {}", messages.join(", ")), Instant::now()));
    }

    /// Clear expired status messages.
    fn clear_expired_status(&mut self) {
        if let Some((_, time)) = &self.status_message
            && time.elapsed() > Duration::from_secs(5) {
                self.status_message = None;
            }
    }

    /// Auto-scroll to bottom if enabled.
    pub(crate) fn maybe_auto_scroll(&mut self) {
        if self.auto_scroll {
            let (client_len, builder_len) = {
                let state = self.log_state.lock().unwrap();
                (state.client_logs.len(), state.builder_logs.len())
            };
            if client_len > 0 {
                self.client_list_state.select(Some(client_len - 1));
            }
            if builder_len > 0 {
                self.builder_list_state.select(Some(builder_len - 1));
            }
        }
    }
}

/// Initialize the terminal for TUI mode.
pub(crate) fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

/// Restore the terminal to normal mode.
pub(crate) fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Draw the TUI frame.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Log panels
            Constraint::Length(3), // Footer/help
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_logs(frame, chunks[1], app);
    draw_footer(frame, chunks[2], app);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let state = app.log_state.lock().unwrap();
    let uptime = state.uptime();
    let uptime_str = format!(
        "{:02}:{:02}:{:02}",
        uptime.as_secs() / 3600,
        (uptime.as_secs() % 3600) / 60,
        uptime.as_secs() % 60
    );

    // Status icons: ✓ running, ✖ stopped
    let client_status = if state.client_running { "✓" } else { "✖" };
    let builder_status = if state.builder_running { "✓" } else { "✖" };
    let client_color = if state.client_running { Color::Green } else { Color::Red };
    let builder_color = if state.builder_running { Color::Green } else { Color::Red };

    // Block number
    let block_str =
        state.latest_block.map(|n| format!("#{}", n)).unwrap_or_else(|| "#-".to_string());
    drop(state);

    let auto_scroll_indicator = if app.auto_scroll { "AUTO" } else { "MANUAL" };
    let scroll_color = if app.auto_scroll { Color::Green } else { Color::Yellow };

    let mut spans = vec![
        Span::styled(
            " BASE DEVNET ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::styled(client_status, Style::default().fg(client_color)),
        Span::styled(" Client ", Style::default().fg(Color::DarkGray)),
        Span::styled(builder_status, Style::default().fg(builder_color)),
        Span::styled(" Builder ", Style::default().fg(Color::DarkGray)),
        Span::raw(" │ "),
        // Stats: Block #, Chain ID, Block Time
        Span::styled("Block ", Style::default().fg(Color::DarkGray)),
        Span::styled(block_str, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled("Chain ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.chain_id), Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled("2s", Style::default().fg(Color::DarkGray)),
        Span::raw(" │ "),
        Span::styled("⏱ ", Style::default().fg(Color::DarkGray)),
        Span::styled(uptime_str, Style::default().fg(Color::White)),
        Span::raw(" │ "),
        Span::styled(auto_scroll_indicator, Style::default().fg(scroll_color)),
    ];

    // Show status message if present
    if let Some((msg, _)) = &app.status_message {
        spans.push(Span::raw(" │ "));
        spans.push(Span::styled(msg.as_str(), Style::default().fg(Color::Green)));
    }

    let header = Paragraph::new(Line::from(spans)).block(
        Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(header, area);
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Calculate max width for log lines (panel width minus borders)
    let panel_width = chunks[0].width.saturating_sub(2) as usize;

    let state = app.log_state.lock().unwrap();

    // Client logs
    let client_border_color =
        if app.focus == Focus::Client { Color::Cyan } else { Color::DarkGray };
    let client_items: Vec<ListItem<'_>> = state
        .client_logs
        .iter()
        .map(|log| {
            let has_tx = has_user_transactions(log); // Check BEFORE truncating
            let truncated = truncate_line(log, panel_width);
            ListItem::new(styled_log_line(&truncated, has_tx))
        })
        .collect();
    let client_list = List::new(client_items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Client Logs ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(client_border_color)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    // Need to drop the lock before rendering
    drop(state);

    frame.render_stateful_widget(client_list, chunks[0], &mut app.client_list_state);

    // Re-acquire lock for builder logs
    let state = app.log_state.lock().unwrap();

    // Builder logs
    let builder_border_color =
        if app.focus == Focus::Builder { Color::Magenta } else { Color::DarkGray };
    let builder_items: Vec<ListItem<'_>> = state
        .builder_logs
        .iter()
        .map(|log| {
            let has_tx = has_user_transactions(log); // Check BEFORE truncating
            let truncated = truncate_line(log, panel_width);
            ListItem::new(styled_log_line(&truncated, has_tx))
        })
        .collect();
    let builder_list = List::new(builder_items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Builder Logs ",
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(builder_border_color)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    drop(state);

    frame.render_stateful_widget(builder_list, chunks[1], &mut app.builder_list_state);
}

/// Strip ANSI escape sequences and truncate to max_width visible characters.
/// ratatui doesn't interpret ANSI codes, so we strip them and use Style instead.
fn truncate_line(line: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut result = String::with_capacity(max_width);
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ANSI escape sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // Skip '['
                // Skip until we hit a letter (end of CSI sequence)
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            // Regular visible character
            result.push(c);
            if result.len() >= max_width.saturating_sub(3) && chars.peek().is_some() {
                // Check if there's more content after stripping ANSI
                let has_more = chars.clone().any(|ch| ch != '\x1b');
                if has_more {
                    result.push_str("...");
                    break;
                }
            }
        }
    }

    result
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, _app: &App) {
    let help = Paragraph::new(Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" quit ", Style::default().fg(Color::DarkGray)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(" r", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" restart ", Style::default().fg(Color::DarkGray)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(" s", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(" save logs ", Style::default().fg(Color::DarkGray)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(" Tab", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" switch ", Style::default().fg(Color::DarkGray)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(" ↑↓/jk", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" scroll ", Style::default().fg(Color::DarkGray)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::styled(" f", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" auto-scroll ", Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(help, area);
}

/// Parse a log line into styled spans.
/// Timestamp = dim gray, Level = colored, Message = white.
/// Blocks with user txs (txs > 1) are highlighted in green.
fn styled_log_line(line: &str, has_user_tx: bool) -> Line<'static> {
    // Format: "2026-01-16T02:19:55.123456Z INFO message here"
    // Use split_whitespace to handle irregular spacing from ANSI stripping

    let mut parts = line.split_whitespace();

    if let Some(first) = parts.next() {
        // Check if first part looks like ISO timestamp
        let is_timestamp = first.len() > 10
            && first.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            && first.contains('T');

        if is_timestamp
            && let Some(level_str) = parts.next() {
                let level_upper = level_str.to_uppercase();

                // Determine level color
                let level_color = if level_upper.contains("ERR") {
                    Color::LightRed
                } else if level_upper.contains("WARN") {
                    Color::Yellow
                } else if level_upper.contains("INFO") {
                    Color::Cyan
                } else if level_upper.contains("DEBUG") {
                    Color::Blue
                } else if level_upper.contains("TRACE") {
                    Color::Magenta
                } else {
                    Color::White
                };

                // Collect remaining parts as message
                let message: String = parts.collect::<Vec<&str>>().join(" ");

                // Highlight blocks with user transactions
                let msg_style = if has_user_tx {
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };

                let mut spans = vec![
                    Span::styled(format!("{} ", first), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{} ", level_str), Style::default().fg(level_color)),
                ];

                if !message.is_empty() {
                    if has_user_tx {
                        // Add a tx indicator
                        spans.push(Span::styled("◆ ", Style::default().fg(Color::LightGreen)));
                    }
                    spans.push(Span::styled(message, msg_style));
                }

                return Line::from(spans);
            }
    }

    // Fallback: plain white
    Line::from(Span::styled(line.to_string(), Style::default().fg(Color::White)))
}

/// Check if a log line represents a block with user transactions (txs > 1).
fn has_user_transactions(line: &str) -> bool {
    let clean = strip_ansi(line);
    if let Some(idx) = clean.find("txs=") {
        let rest = &clean[idx + 4..];
        let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num_str.parse::<u32>() {
            return n > 1;
        }
    }
    false
}

/// Result of the TUI event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiResult {
    Quit,
    Restart,
}

/// Run the TUI event loop.
pub(crate) fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> io::Result<TuiResult> {
    loop {
        // Auto-scroll before drawing
        app.maybe_auto_scroll();
        // Clear expired status messages
        app.clear_expired_status();
        // Check if processes are still alive
        app.log_state.lock().unwrap().update_process_status();

        terminal.draw(|f| draw(f, app))?;

        // Poll for events with a short timeout to allow log updates
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }

        if app.should_quit {
            return Ok(TuiResult::Quit);
        }
        if app.should_restart {
            return Ok(TuiResult::Restart);
        }
    }
}
