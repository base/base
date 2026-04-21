use std::{collections::HashMap, fmt, io::Stdout, sync::atomic::Ordering, time::Duration};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use tokio::sync::oneshot;

use super::{Action, Resources, Router, View, ViewId, runner::start_background_services};
use crate::{
    commands::{COLOR_BASE_BLUE, EVENT_POLL_TIMEOUT},
    config::MonitoringConfig,
    tui::{AppFrame, Toast, restore_terminal, setup_terminal},
};

// ---------------------------------------------------------------------------
// Network picker
// ---------------------------------------------------------------------------

/// Overlay state for the network-switching picker.
#[derive(Debug)]
struct NetworkPicker {
    options: Vec<String>,
    cursor: usize,
}

impl NetworkPicker {
    fn new() -> Self {
        Self { options: MonitoringConfig::available_names(), cursor: 0 }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Main TUI application that manages views, routing, and the event loop.
pub struct App {
    router: Router,
    resources: Resources,
    show_help: bool,
    view_cache: HashMap<ViewId, Box<dyn View>>,
    /// Network picker overlay; `None` when closed.
    network_picker: Option<NetworkPicker>,
    /// Pending async network-load result. `Some` while a switch is in flight.
    pending_network: Option<oneshot::Receiver<anyhow::Result<MonitoringConfig>>>,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("router", &self.router)
            .field("resources", &self.resources)
            .field("show_help", &self.show_help)
            .field("view_cache", &format_args!("({} views)", self.view_cache.len()))
            .field("network_picker", &self.network_picker.as_ref().map(|_| ".."))
            .field("pending_network", &self.pending_network.is_some())
            .finish()
    }
}

impl App {
    /// Creates a new application with the given resources and initial view.
    pub fn new(resources: Resources, initial_view: ViewId) -> Self {
        Self {
            router: Router::new(initial_view),
            resources,
            show_help: false,
            view_cache: HashMap::new(),
            network_picker: None,
            pending_network: None,
        }
    }

    /// Runs the application event loop using the given view factory.
    pub async fn run<F>(mut self, mut view_factory: F) -> Result<()>
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let mut terminal = setup_terminal()?;
        let result = self.run_loop(&mut terminal, &mut view_factory).await;
        restore_terminal(&mut terminal)?;

        if let Some(task) = self.resources.load_test_task.take() {
            task.stop_flag.store(true, Ordering::SeqCst);
            eprintln!("Waiting for load test to drain accounts...");
            match tokio::time::timeout(Duration::from_secs(120), task.handle).await {
                Ok(Ok(())) => eprintln!("Load test shutdown complete."),
                Ok(Err(e)) => eprintln!("Load test task panicked: {e}"),
                Err(_) => {
                    eprintln!("Load test drain timed out. Funds may remain in test accounts.")
                }
            }
        }

        result
    }

    async fn run_loop<F>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        view_factory: &mut F,
    ) -> Result<()>
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let mut current_view = view_factory(self.router.current());

        loop {
            self.resources.da.poll();
            self.resources.flash.poll();
            self.resources.toasts.poll();
            self.resources.conductor.poll();
            self.resources.validators.poll();
            self.resources.proofs.poll();
            // When a conductor cluster is configured, bridge the Raft leader's
            // safe head into the DA tracker each tick.  The conductor poller
            // already queries `sync_status` from every node's CL, so the
            // leader's value is available here without an extra RPC.  This
            // ensures the DA monitor advances even before sequencer-0's EL has
            // P2P-synced blocks that were produced by a different leader.
            if let Some(safe_head) = self.resources.conductor.leader_safe_l2_block() {
                self.resources.da.apply_conductor_safe_head(safe_head);
            }
            self.resources.poll_sys_config();

            // Check for a completed async network switch.
            self.poll_pending_network(&mut current_view, view_factory);

            let action = current_view.tick(&mut self.resources);
            if self.handle_action(action, &mut current_view, view_factory) {
                break;
            }

            // Show `[…]` in the badge while a network switch is in progress.
            // Use ASCII "..." so badge.len() == display width (avoids multi-byte
            // width miscalculation from the 3-byte U+2026 HORIZONTAL ELLIPSIS).
            let badge_name = if self.pending_network.is_some() {
                "...".to_string()
            } else {
                self.resources.chain_name().to_string()
            };

            terminal.draw(|frame| {
                let layout = AppFrame::split_layout(frame.area(), self.show_help);
                current_view.render(frame, layout.content, &self.resources);
                AppFrame::render(frame, &layout, &badge_name, current_view.keybindings());
                // Network picker overlays the entire frame, above the view.
                if let Some(ref picker) = self.network_picker {
                    render_network_picker(frame, frame.area(), picker, self.resources.chain_name());
                }
                self.resources.toasts.render(frame, frame.area());
            })?;

            if event::poll(EVENT_POLL_TIMEOUT)?
                && let Event::Key(key) = event::read()?
            {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }

                let action = if self.network_picker.is_some() {
                    // All keys feed the picker when it is open.
                    self.handle_network_picker_key(key);
                    Action::None
                } else {
                    match key.code {
                        KeyCode::Char('?') => {
                            self.show_help = !self.show_help;
                            Action::None
                        }
                        // `n` opens the network picker from anywhere, unless the
                        // active view is in a text-input mode that needs all chars.
                        KeyCode::Char('n')
                            if self.pending_network.is_none()
                                && !current_view.captures_char_input() =>
                        {
                            self.show_help = false;
                            self.network_picker = Some(NetworkPicker::new());
                            Action::None
                        }
                        KeyCode::Char('q') => {
                            if current_view.consumes_quit() {
                                current_view.handle_key(key, &mut self.resources)
                            } else {
                                Action::Quit
                            }
                        }
                        KeyCode::Esc => {
                            if current_view.consumes_esc() {
                                current_view.handle_key(key, &mut self.resources)
                            } else if self.router.current() == ViewId::Home {
                                Action::Quit
                            } else {
                                Action::SwitchView(ViewId::Home)
                            }
                        }
                        _ => current_view.handle_key(key, &mut self.resources),
                    }
                };

                if self.handle_action(action, &mut current_view, view_factory) {
                    break;
                }
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Network switching
    // -----------------------------------------------------------------------

    fn handle_network_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ref mut picker) = self.network_picker {
                    picker.cursor = picker.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ref mut picker) = self.network_picker {
                    let n = picker.options.len();
                    if picker.cursor + 1 < n {
                        picker.cursor += 1;
                    }
                }
            }
            KeyCode::Enter => {
                // Extract selection before dropping the picker borrow.
                let selected = self
                    .network_picker
                    .as_ref()
                    .map(|p| p.options[p.cursor].clone())
                    .unwrap_or_default();
                let current = self.resources.chain_name().to_string();
                self.network_picker = None;
                if !selected.is_empty() && selected != current {
                    self.start_network_switch(selected);
                }
            }
            KeyCode::Esc => {
                self.network_picker = None;
            }
            _ => {}
        }
    }

    fn start_network_switch(&mut self, name: String) {
        self.resources.toasts.push(Toast::info(format!("Connecting to {name}…")));
        let (tx, rx) = oneshot::channel();
        self.pending_network = Some(rx);
        tokio::spawn(async move {
            let result = MonitoringConfig::load(&name).await;
            let _ = tx.send(result);
        });
    }

    fn poll_pending_network<F>(&mut self, current_view: &mut Box<dyn View>, view_factory: &mut F)
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let outcome = match self.pending_network {
            None => return,
            Some(ref mut rx) => match rx.try_recv() {
                Ok(result) => {
                    self.pending_network = None;
                    Some(result)
                }
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.pending_network = None;
                    self.resources
                        .toasts
                        .push(Toast::warning("Network switch failed: task dropped".to_string()));
                    None
                }
            },
        };

        match outcome {
            Some(Ok(new_config)) => {
                self.apply_network_switch(new_config, current_view, view_factory);
            }
            Some(Err(e)) => {
                self.resources.toasts.push(Toast::warning(format!("Network switch failed: {e}")));
            }
            None => {}
        }
    }

    fn apply_network_switch<F>(
        &mut self,
        new_config: MonitoringConfig,
        current_view: &mut Box<dyn View>,
        view_factory: &mut F,
    ) where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        let name = new_config.name.clone();
        // Replace resources entirely — dropping old receivers causes background
        // tasks from the previous network to exit naturally on their next send.
        self.resources = Resources::new(new_config.clone());
        start_background_services(&new_config, &mut self.resources);
        // Discard all cached view state so views re-initialise for the new network.
        self.view_cache.clear();
        self.router = Router::new(ViewId::Home);
        *current_view = view_factory(ViewId::Home);
        self.resources.toasts.push(Toast::info(format!("Connected to {name}")));
    }

    // -----------------------------------------------------------------------
    // View routing
    // -----------------------------------------------------------------------

    fn handle_action<F>(
        &mut self,
        action: Action,
        current_view: &mut Box<dyn View>,
        view_factory: &mut F,
    ) -> bool
    where
        F: FnMut(ViewId) -> Box<dyn View>,
    {
        match action {
            Action::None => false,
            Action::Quit => true,
            Action::SwitchView(view_id) => {
                let old_view_id = self.router.current();
                self.router.switch_to(view_id);
                let new_view =
                    self.view_cache.remove(&view_id).unwrap_or_else(|| view_factory(view_id));
                let old_view = std::mem::replace(current_view, new_view);
                self.view_cache.insert(old_view_id, old_view);
                self.show_help = false;
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Network picker renderer
// ---------------------------------------------------------------------------

fn render_network_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &NetworkPicker,
    active_network: &str,
) {
    let n = picker.options.len();
    let popup_w = 34u16.min(area.width.saturating_sub(4));
    let popup_h = (n as u16 + 5).min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Switch Network ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let cursor_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let active_style = Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::Gray);
    let dim_style = Style::default().fg(Color::DarkGray);
    let hint_style = Style::default().fg(Color::DarkGray);

    // Compute scroll so the cursor row is always visible.
    let visible_rows = (inner.height as usize).saturating_sub(3); // blank + blank + hint
    let scroll = if picker.cursor >= visible_rows { picker.cursor - visible_rows + 1 } else { 0 };

    let mut lines: Vec<Line<'_>> = vec![Line::from("")];

    let end = (scroll + visible_rows).min(n);
    for i in scroll..end {
        let name = &picker.options[i];
        let is_cursor = i == picker.cursor;
        let is_active = name.as_str() == active_network;

        let selector = if is_cursor { "▸ " } else { "  " };
        let selector_style = if is_cursor { cursor_style } else { dim_style };
        let name_style = if is_cursor {
            cursor_style
        } else if is_active {
            active_style
        } else {
            normal_style
        };
        let check = if is_active { Span::styled(" ✓", active_style) } else { Span::raw("") };

        lines.push(Line::from(vec![
            Span::styled(selector, selector_style),
            Span::styled(name.as_str(), name_style),
            check,
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled("  ↑/↓ move  Enter confirm  Esc cancel", hint_style)]));

    frame.render_widget(Paragraph::new(lines), inner);
}
