use std::{collections::HashMap, str::FromStr};

use alloy_primitives::B256;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::mpsc;

use crate::{
    app::{Action, Resources, View},
    output::COLOR_BASE_BLUE,
    rpc::{
        ConductorNodeStatus, PausedPeers, ValidatorNodeStatus, conductor_pause_all_nodes,
        conductor_pause_node, conductor_resume_all_nodes, conductor_resume_node,
        pause_sequencer_node, restart_conductor_node, start_sequencer_node, stop_sequencer_node,
        transfer_conductor_leader, unpause_sequencer_node,
    },
    tui::{Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "←/→", description: "Select node" },
    Keybinding { key: "Enter", description: "Open action menu" },
    Keybinding { key: "t", description: "Transfer leader (any)" },
    Keybinding { key: "P", description: "Pause conductor on all nodes" },
    Keybinding { key: "R", description: "Resume conductor on all nodes" },
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

type PauseRx = Option<(String, mpsc::Receiver<Result<(String, PausedPeers), String>>)>;

/// Items rendered in the per-node action menu.
///
/// Each variant maps either to a [`PendingAction`] (after confirmation) or to a
/// transition into [`Overlay::HashInput`] (for inputs that require a value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionMenuItem {
    /// Transfer leadership to an unspecified healthy peer.
    TransferLeaderAny,
    /// Transfer leadership to the currently-selected node.
    TransferLeaderHere,
    /// Pause the conductor's control loop on the selected node.
    ConductorPause,
    /// Resume the conductor's control loop on the selected node.
    ConductorResume,
    /// Start the sequencer on the selected node at a chosen unsafe head.
    StartSequencer,
    /// Stop the sequencer on the selected node.
    StopSequencer,
    /// Toggle soft P2P isolation (disconnect / reconnect every CL+EL peer).
    P2PToggle,
    /// Restart the EL/CL/conductor docker containers in dependency order.
    RestartContainers,
}

const MENU_ITEMS: &[ActionMenuItem] = &[
    ActionMenuItem::TransferLeaderAny,
    ActionMenuItem::TransferLeaderHere,
    ActionMenuItem::ConductorPause,
    ActionMenuItem::ConductorResume,
    ActionMenuItem::StartSequencer,
    ActionMenuItem::StopSequencer,
    ActionMenuItem::P2PToggle,
    ActionMenuItem::RestartContainers,
];

impl ActionMenuItem {
    /// Returns the menu label, contextualized by node state where relevant.
    pub const fn label(self, _node: &ConductorNodeStatus, is_p2p_isolated: bool) -> &'static str {
        match self {
            Self::TransferLeaderAny => "Transfer leader (any peer)",
            Self::TransferLeaderHere => "Transfer leader here",
            Self::ConductorPause => "Conductor pause",
            Self::ConductorResume => "Conductor resume",
            Self::StartSequencer => "Start sequencer…",
            Self::StopSequencer => "Stop sequencer",
            Self::P2PToggle => {
                if is_p2p_isolated {
                    "P2P reconnect"
                } else {
                    "P2P isolate"
                }
            }
            Self::RestartContainers => "Restart containers",
        }
    }

    /// Returns whether the action makes sense given the current node state.
    ///
    /// Disabled items remain visible (greyed out) so operators always see the
    /// full menu and don't have to guess what's missing.
    pub fn enabled(self, node: &ConductorNodeStatus, _is_p2p_isolated: bool) -> bool {
        match self {
            Self::TransferLeaderHere => node.is_leader == Some(false),
            Self::ConductorPause => node.conductor_paused == Some(false),
            Self::ConductorResume => node.conductor_paused == Some(true),
            Self::StartSequencer => {
                node.is_leader == Some(true) && node.sequencer_active == Some(false)
            }
            Self::StopSequencer => node.sequencer_active == Some(true),
            Self::RestartContainers | Self::P2PToggle => !node.discovered,
            Self::TransferLeaderAny => true,
        }
    }
}

/// Yes / No selector inside the confirmation overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmButton {
    /// Confirm and execute the action.
    Yes,
    /// Cancel and return to the previous overlay.
    No,
}

/// A mutation queued behind a confirmation prompt.
#[derive(Debug, Clone)]
pub enum PendingAction {
    /// Transfer leadership to any healthy peer.
    TransferAny,
    /// Transfer leadership to the named node.
    TransferTo(String),
    /// Restart docker containers on the named node.
    RestartNode(String),
    /// Soft-isolate the named node by disconnecting every CL+EL peer.
    P2PIsolate(String),
    /// Reconnect a previously isolated node using its saved peers.
    P2PReconnect(String),
    /// Pause the conductor's control loop on the named node.
    ConductorPause(String),
    /// Resume the conductor's control loop on the named node.
    ConductorResume(String),
    /// Pause the conductor's control loop on every node in the cluster.
    ///
    /// Carries the node count so the confirmation prompt can show it.
    ConductorPauseAll(usize),
    /// Resume the conductor's control loop on every node in the cluster.
    ConductorResumeAll(usize),
    /// Start the sequencer at the given unsafe head hash.
    StartSequencer {
        /// Target conductor / sequencer node name.
        node: String,
        /// Unsafe head hash to start from. Server rejects [`B256::ZERO`].
        hash: B256,
    },
    /// Stop the sequencer on the named node.
    StopSequencer(String),
}

impl PendingAction {
    /// Human-readable description shown inside the confirmation overlay.
    pub fn description(&self) -> String {
        match self {
            Self::TransferAny => "Transfer leadership to any healthy peer?".to_string(),
            Self::TransferTo(name) => format!("Transfer leadership to {name}?"),
            Self::RestartNode(name) => format!("Restart EL/CL/conductor containers on {name}?"),
            Self::P2PIsolate(name) => {
                format!("Disconnect every CL+EL peer on {name}? (soft pause)")
            }
            Self::P2PReconnect(name) => format!("Reconnect saved peers on {name}?"),
            Self::ConductorPause(name) => format!("Pause conductor control loop on {name}?"),
            Self::ConductorResume(name) => format!("Resume conductor control loop on {name}?"),
            Self::ConductorPauseAll(count) => {
                format!("Pause conductor control loop on ALL {count} nodes?")
            }
            Self::ConductorResumeAll(count) => {
                format!("Resume conductor control loop on ALL {count} nodes?")
            }
            Self::StartSequencer { node, hash } => {
                format!("Start sequencer on {node} at {hash}?")
            }
            Self::StopSequencer(name) => format!("Stop sequencer on {name}?"),
        }
    }

    /// Whether the action is destructive enough to warrant a red confirm button.
    pub const fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::TransferAny
                | Self::TransferTo(_)
                | Self::RestartNode(_)
                | Self::P2PIsolate(_)
                | Self::ConductorPause(_)
                | Self::ConductorPauseAll(_)
                | Self::StopSequencer(_)
        )
    }
}

/// Modal overlay state for [`ConductorView`].
///
/// Only one overlay can be active at a time. While active, key handling and
/// rendering route through the overlay's branch instead of the underlying
/// status table.
#[derive(Debug, Default)]
pub enum Overlay {
    /// No overlay; status table is interactive.
    #[default]
    None,
    /// Per-node action menu, with `cursor` indexing into [`MENU_ITEMS`].
    ActionMenu {
        /// Currently-highlighted menu item index.
        cursor: usize,
    },
    /// Yes/No confirmation prompt for a pending action.
    Confirm {
        /// The action to execute on `Yes`.
        action: PendingAction,
        /// Currently-highlighted confirm button.
        button: ConfirmButton,
    },
    /// Free-text hex input used for `admin_startSequencer`.
    HashInput {
        /// Target node name (carried so we can spawn the mutation directly).
        node: String,
        /// Current input buffer (without the leading `0x`).
        input: String,
        /// Cursor offset within `input`.
        cursor: usize,
        /// True when the buffer was prefilled from a poll snapshot.
        prefilled: bool,
    },
}

/// HA conductor cluster status view with per-node action overlay.
///
/// Renders a fixed grid with one column per conductor node and rows for
/// role, conductor state (paused / stopped / healthy / sequencer-active),
/// CL block heads, and EL block heads. The user navigates columns with
/// `←`/`→` and opens a per-node action menu with `Enter`. Mutating actions
/// are gated behind a `Yes` / `No` confirmation overlay; `Start sequencer`
/// additionally prompts for an unsafe head hash, prefilled from the latest
/// poll snapshot.
#[derive(Debug, Default)]
pub struct ConductorView {
    selected: usize,
    overlay: Overlay,
    op_pending: bool,
    /// In-flight result channel for any mutation returning `Result<String, String>`
    /// (transfer, restart, conductor pause/resume, sequencer start/stop).
    op_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// In-flight result channel for the soft P2P-isolate operation.
    /// Carries `(node_name, result)` where `Ok` includes the saved peers.
    pause_rx: PauseRx,
    /// In-flight result channel for the soft P2P-reconnect operation.
    unpause_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// Name of the node currently being reconnected, if any. Used to remove the saved
    /// peer list from `paused_node_peers` only after a successful reconnect, so a
    /// failed RPC leaves the saved peers intact for retry.
    reconnecting_node: Option<String>,
    /// Saved peer lists for each soft-isolated node, keyed by node name.
    /// Presence in this map means the node is currently P2P-isolated.
    paused_node_peers: HashMap<String, PausedPeers>,
}

impl ConductorView {
    /// Creates a new conductor view with no overlay open.
    pub fn new() -> Self {
        Self::default()
    }

    const fn is_overlay_open(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    fn selected_node<'a>(
        &self,
        nodes: &'a [ConductorNodeStatus],
    ) -> Option<&'a ConductorNodeStatus> {
        if nodes.is_empty() { None } else { Some(&nodes[self.selected.min(nodes.len() - 1)]) }
    }

    fn open_action_menu(&mut self) {
        self.overlay = Overlay::ActionMenu { cursor: 0 };
    }

    /// Resolves a menu item into an overlay transition (or a no-op).
    fn select_menu_item(
        &mut self,
        item: ActionMenuItem,
        node: &ConductorNodeStatus,
        is_p2p_isolated: bool,
    ) {
        if !item.enabled(node, is_p2p_isolated) {
            return;
        }
        let name = node.name.clone();
        let action = match item {
            ActionMenuItem::TransferLeaderAny => Some(PendingAction::TransferAny),
            ActionMenuItem::TransferLeaderHere => Some(PendingAction::TransferTo(name)),
            ActionMenuItem::ConductorPause => Some(PendingAction::ConductorPause(name)),
            ActionMenuItem::ConductorResume => Some(PendingAction::ConductorResume(name)),
            ActionMenuItem::StopSequencer => Some(PendingAction::StopSequencer(name)),
            ActionMenuItem::RestartContainers => Some(PendingAction::RestartNode(name)),
            ActionMenuItem::P2PToggle => Some(if is_p2p_isolated {
                PendingAction::P2PReconnect(name)
            } else {
                PendingAction::P2PIsolate(name)
            }),
            ActionMenuItem::StartSequencer => {
                let (input, prefilled) = node
                    .unsafe_l2_hash
                    .map_or_else(|| (String::new(), false), |h| (format!("{h:x}"), true));
                let cursor = input.len();
                self.overlay = Overlay::HashInput { node: name, input, cursor, prefilled };
                None
            }
        };
        if let Some(action) = action {
            self.overlay = Overlay::Confirm { action, button: ConfirmButton::No };
        }
    }

    /// Spawns the mutation behind a confirmed action and switches to single-flight.
    fn execute(&mut self, action: PendingAction, resources: &Resources) {
        let nodes_cfg = resources.conductor.nodes_config();
        if nodes_cfg.is_empty() {
            return;
        }
        self.op_pending = true;
        self.close_overlay();

        match action {
            PendingAction::TransferAny => {
                let (tx, rx) = mpsc::channel(1);
                self.op_rx = Some(rx);
                tokio::spawn(transfer_conductor_leader(nodes_cfg.to_vec(), None, tx));
            }
            PendingAction::TransferTo(target) => {
                let (tx, rx) = mpsc::channel(1);
                self.op_rx = Some(rx);
                tokio::spawn(transfer_conductor_leader(nodes_cfg.to_vec(), Some(target), tx));
            }
            PendingAction::RestartNode(name) => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.op_rx = Some(rx);
                    tokio::spawn(restart_conductor_node(node, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::P2PIsolate(name) => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.pause_rx = Some((node.name.clone(), rx));
                    tokio::spawn(pause_sequencer_node(node, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::P2PReconnect(name) => {
                let node = nodes_cfg.iter().find(|n| n.name == name).cloned();
                let peers = self.paused_node_peers.get(&name).cloned();
                if let (Some(node), Some(peers)) = (node, peers) {
                    let (tx, rx) = mpsc::channel(1);
                    self.unpause_rx = Some(rx);
                    self.reconnecting_node = Some(name);
                    tokio::spawn(unpause_sequencer_node(node, peers, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::ConductorPause(name) => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.op_rx = Some(rx);
                    tokio::spawn(conductor_pause_node(node, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::ConductorResume(name) => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.op_rx = Some(rx);
                    tokio::spawn(conductor_resume_node(node, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::ConductorPauseAll(_) => {
                let (tx, rx) = mpsc::channel(1);
                self.op_rx = Some(rx);
                tokio::spawn(conductor_pause_all_nodes(nodes_cfg.to_vec(), tx));
            }
            PendingAction::ConductorResumeAll(_) => {
                let (tx, rx) = mpsc::channel(1);
                self.op_rx = Some(rx);
                tokio::spawn(conductor_resume_all_nodes(nodes_cfg.to_vec(), tx));
            }
            PendingAction::StartSequencer { node: name, hash } => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.op_rx = Some(rx);
                    tokio::spawn(start_sequencer_node(node, hash, tx));
                } else {
                    self.op_pending = false;
                }
            }
            PendingAction::StopSequencer(name) => {
                if let Some(node) = nodes_cfg.iter().find(|n| n.name == name).cloned() {
                    let (tx, rx) = mpsc::channel(1);
                    self.op_rx = Some(rx);
                    tokio::spawn(stop_sequencer_node(node, tx));
                } else {
                    self.op_pending = false;
                }
            }
        }
    }
}

impl View for ConductorView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn consumes_esc(&self) -> bool {
        self.is_overlay_open()
    }

    fn consumes_quit(&self) -> bool {
        self.is_overlay_open()
    }

    fn captures_char_input(&self) -> bool {
        // Any open overlay handles its own keys (including Confirm's y/n shortcuts and
        // ActionMenu's j/k navigation), so the framework should not intercept Char keys
        // for its own bindings while an overlay is open.
        self.is_overlay_open()
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if let Some(ref mut rx) = self.op_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            self.op_rx = None;
            match result {
                Ok(msg) => resources.toasts.push(Toast::info(msg)),
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
        }

        if let Some((ref node_name, ref mut rx)) = self.pause_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            match result {
                Ok((msg, peers)) => {
                    self.paused_node_peers.insert(node_name.clone(), peers);
                    resources.toasts.push(Toast::info(msg));
                }
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
            self.pause_rx = None;
        }

        if let Some(ref mut rx) = self.unpause_rx
            && let Ok(result) = rx.try_recv()
        {
            self.op_pending = false;
            self.unpause_rx = None;
            let node_name = self.reconnecting_node.take();
            match result {
                Ok(msg) => {
                    if let Some(name) = node_name {
                        self.paused_node_peers.remove(&name);
                    }
                    resources.toasts.push(Toast::info(msg));
                }
                Err(msg) => resources.toasts.push(Toast::warning(msg)),
            }
        }

        Action::None
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        let node_count = resources.conductor.nodes.len();

        match &mut self.overlay {
            Overlay::HashInput { node: target, input, cursor, prefilled } => {
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Backspace => {
                        if *cursor > 0 {
                            *cursor -= 1;
                            input.remove(*cursor);
                            *prefilled = false;
                        }
                    }
                    KeyCode::Left => {
                        if *cursor > 0 {
                            *cursor -= 1;
                        }
                    }
                    KeyCode::Right => {
                        if *cursor < input.len() {
                            *cursor += 1;
                        }
                    }
                    KeyCode::Home => *cursor = 0,
                    KeyCode::End => *cursor = input.len(),
                    KeyCode::F(5) => {
                        if let Some(hash) = resources
                            .conductor
                            .nodes
                            .iter()
                            .find(|n| n.name == *target)
                            .and_then(|n| n.unsafe_l2_hash)
                        {
                            *input = format!("{hash:x}");
                            *cursor = input.len();
                            *prefilled = true;
                        }
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if c.is_ascii_hexdigit() && input.len() < 64 {
                            input.insert(*cursor, c);
                            *cursor += 1;
                            *prefilled = false;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(hash) = parse_hex_hash(input) {
                            if hash == B256::ZERO {
                                resources
                                    .toasts
                                    .push(Toast::warning("Refusing to start at zero hash"));
                            } else {
                                let target_clone = target.clone();
                                self.overlay = Overlay::Confirm {
                                    action: PendingAction::StartSequencer {
                                        node: target_clone,
                                        hash,
                                    },
                                    button: ConfirmButton::No,
                                };
                            }
                        } else {
                            resources.toasts.push(Toast::warning(
                                "Invalid hash: need 64 hex chars (with or without 0x)".to_string(),
                            ));
                        }
                    }
                    _ => {}
                }
                return Action::None;
            }
            Overlay::Confirm { action, button } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('n' | 'N') => self.close_overlay(),
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        *button = match *button {
                            ConfirmButton::Yes => ConfirmButton::No,
                            ConfirmButton::No => ConfirmButton::Yes,
                        };
                    }
                    KeyCode::Char('y' | 'Y') => {
                        let action = action.clone();
                        self.execute(action, resources);
                    }
                    KeyCode::Enter => match button {
                        ConfirmButton::Yes => {
                            let action = action.clone();
                            self.execute(action, resources);
                        }
                        ConfirmButton::No => self.close_overlay(),
                    },
                    _ => {}
                }
                return Action::None;
            }
            Overlay::ActionMenu { cursor } => {
                match key.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = (*cursor + MENU_ITEMS.len() - 1) % MENU_ITEMS.len();
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1) % MENU_ITEMS.len();
                    }
                    KeyCode::Enter => {
                        let cursor_idx = *cursor;
                        if let Some(node) = self.selected_node(&resources.conductor.nodes).cloned()
                        {
                            let item = MENU_ITEMS[cursor_idx];
                            let is_p2p_isolated = self.paused_node_peers.contains_key(&node.name);
                            self.select_menu_item(item, &node, is_p2p_isolated);
                        }
                    }
                    _ => {}
                }
                return Action::None;
            }
            Overlay::None => {}
        }

        match key.code {
            KeyCode::Left | KeyCode::Char('h') if node_count > 0 => {
                self.selected = (self.selected + node_count - 1) % node_count;
            }
            KeyCode::Right | KeyCode::Char('l') if node_count > 0 => {
                self.selected = (self.selected + 1) % node_count;
            }
            KeyCode::Enter if !self.op_pending && node_count > 0 => {
                self.open_action_menu();
            }
            KeyCode::Char('t') if !self.op_pending => {
                self.overlay = Overlay::Confirm {
                    action: PendingAction::TransferAny,
                    button: ConfirmButton::No,
                };
            }
            KeyCode::Char('P') if !self.op_pending && node_count > 0 => {
                self.overlay = Overlay::Confirm {
                    action: PendingAction::ConductorPauseAll(node_count),
                    button: ConfirmButton::No,
                };
            }
            KeyCode::Char('R') if !self.op_pending && node_count > 0 => {
                self.overlay = Overlay::Confirm {
                    action: PendingAction::ConductorResumeAll(node_count),
                    button: ConfirmButton::No,
                };
            }
            _ => {}
        }

        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let content_area = chunks[0];
        let footer_area = chunks[1];

        let nodes = &resources.conductor.nodes;
        let validators = &resources.validators.nodes;

        if validators.is_empty() {
            if nodes.is_empty() {
                render_unconfigured(frame, content_area);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    content_area,
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                );
            }
        } else {
            let conductor_height = 25u16;
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(conductor_height), Constraint::Min(0)])
                .split(content_area);

            if nodes.is_empty() {
                render_unconfigured(frame, sections[0]);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    sections[0],
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                );
            }
            render_validator_table(frame, sections[1], validators);
        }

        render_footer(frame, footer_area, &self.overlay, self.op_pending);

        match &self.overlay {
            Overlay::None => {}
            Overlay::ActionMenu { cursor } => {
                if let Some(node) = self.selected_node(nodes) {
                    let is_p2p_isolated = self.paused_node_peers.contains_key(&node.name);
                    render_action_menu(frame, area, node, *cursor, is_p2p_isolated);
                }
            }
            Overlay::Confirm { action, button } => {
                render_confirm(frame, area, action, *button);
            }
            Overlay::HashInput { node, input, cursor, prefilled } => {
                render_hash_input(frame, area, node, input, *cursor, *prefilled);
            }
        }
    }
}

/// Parses a hex-encoded 32-byte hash, accepting both `0x`-prefixed and bare forms.
fn parse_hex_hash(input: &str) -> Option<B256> {
    B256::from_str(input).ok()
}

fn render_unconfigured(f: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .title(" HA Conductor ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let msg = Paragraph::new("Conductor monitoring requires a config with conductor endpoints.")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));

    f.render_widget(msg, chunks[1]);
}

fn render_footer(f: &mut Frame<'_>, area: Rect, overlay: &Overlay, op_pending: bool) {
    let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let sep = Span::styled("  │  ", desc_style);

    let mut spans: Vec<Span<'_>> = Vec::new();
    let push_pair = |spans: &mut Vec<Span<'_>>, key: &'static str, desc: &'static str| {
        spans.push(Span::styled(format!("[{key}]"), key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc, desc_style));
    };

    match overlay {
        Overlay::None => {
            push_pair(&mut spans, "Esc", "back");
            spans.push(sep.clone());
            push_pair(&mut spans, "←/→", "select node");
            spans.push(sep.clone());
            if op_pending {
                spans.push(Span::styled("working…", Style::default().fg(Color::Yellow)));
            } else {
                push_pair(&mut spans, "Enter", "actions");
                spans.push(sep.clone());
                push_pair(&mut spans, "t", "transfer (any)");
                spans.push(sep.clone());
                push_pair(&mut spans, "P", "pause all");
                spans.push(sep.clone());
                push_pair(&mut spans, "R", "resume all");
            }
        }
        Overlay::ActionMenu { .. } => {
            push_pair(&mut spans, "↑/↓", "move");
            spans.push(sep.clone());
            push_pair(&mut spans, "Enter", "select");
            spans.push(sep.clone());
            push_pair(&mut spans, "Esc", "cancel");
        }
        Overlay::Confirm { .. } => {
            push_pair(&mut spans, "←/→", "Yes / No");
            spans.push(sep.clone());
            push_pair(&mut spans, "Enter", "confirm");
            spans.push(sep.clone());
            push_pair(&mut spans, "y/n", "shortcut");
            spans.push(sep.clone());
            push_pair(&mut spans, "Esc", "cancel");
        }
        Overlay::HashInput { .. } => {
            push_pair(&mut spans, "0-9 a-f", "hex");
            spans.push(sep.clone());
            push_pair(&mut spans, "F5", "refresh prefill");
            spans.push(sep.clone());
            push_pair(&mut spans, "Enter", "confirm");
            spans.push(sep.clone());
            push_pair(&mut spans, "Esc", "cancel");
        }
    }

    spans.push(sep);
    push_pair(&mut spans, "?", "help");

    let footer = Paragraph::new(Line::from(spans));
    f.render_widget(footer, area);
}

fn render_action_menu(
    f: &mut Frame<'_>,
    area: Rect,
    node: &ConductorNodeStatus,
    cursor: usize,
    is_p2p_isolated: bool,
) {
    let popup_w = 44u16.min(area.width.saturating_sub(4));
    let popup_h = (MENU_ITEMS.len() as u16 + 5).min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(Clear, popup);

    let title = format!(" Actions: {} ", node.name);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(MENU_ITEMS.len() + 2);
    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let enabled = item.enabled(node, is_p2p_isolated);
        let label = item.label(node, is_p2p_isolated);
        let marker = if i == cursor { "› " } else { "  " };
        let style = match (i == cursor, enabled) {
            (true, true) => Style::default()
                .fg(COLOR_BASE_BLUE)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            (true, false) => Style::default().fg(Color::DarkGray).add_modifier(Modifier::REVERSED),
            (false, true) => Style::default().fg(Color::White),
            (false, false) => Style::default().fg(Color::DarkGray),
        };
        lines.push(Line::from(vec![Span::styled(format!("{marker}{label}"), style)]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "  ↑/↓ move   Enter select   Esc cancel",
        Style::default().fg(Color::DarkGray),
    )]));

    f.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm(f: &mut Frame<'_>, area: Rect, action: &PendingAction, button: ConfirmButton) {
    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = 8u16.min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let body = Paragraph::new(action.description())
        .style(Style::default().fg(Color::White))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(body, layout[0]);

    let yes_color = if action.is_destructive() { Color::Red } else { Color::Green };
    let yes_style = match button {
        ConfirmButton::Yes => {
            Style::default().fg(yes_color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        }
        ConfirmButton::No => Style::default().fg(yes_color),
    };
    let no_style = match button {
        ConfirmButton::No => {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD | Modifier::REVERSED)
        }
        ConfirmButton::Yes => Style::default().fg(Color::White),
    };

    let buttons = Line::from(vec![
        Span::styled("[ Yes ]", yes_style),
        Span::raw("    "),
        Span::styled("[ No ]", no_style),
    ]);
    f.render_widget(Paragraph::new(buttons).alignment(Alignment::Center), layout[2]);

    let hint = Line::from(vec![Span::styled(
        "←/→ select   Enter confirm   y/n shortcut   Esc cancel",
        Style::default().fg(Color::DarkGray),
    )]);
    f.render_widget(Paragraph::new(hint).alignment(Alignment::Center), layout[3]);
}

fn render_hash_input(
    f: &mut Frame<'_>,
    area: Rect,
    node: &str,
    input: &str,
    cursor: usize,
    prefilled: bool,
) {
    let popup_w = 76u16.min(area.width.saturating_sub(4));
    let popup_h = 9u16.min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(Clear, popup);

    let title = format!(" Start sequencer on {node} ");
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let prompt = Paragraph::new(Line::from(vec![Span::styled(
        "Unsafe head hash (64 hex chars; 0x shown automatically)",
        Style::default().fg(Color::White),
    )]));
    f.render_widget(prompt, layout[0]);

    let trimmed = trim_prefix(input);
    let display = format!("0x{trimmed}");
    let valid = parse_hex_hash(input).is_some_and(|h| h != B256::ZERO);
    let progress = format!("({} / 64 hex chars)", trimmed.len());
    let progress_color = if valid { Color::Green } else { Color::Yellow };

    let value_style =
        if valid { Style::default().fg(Color::Green) } else { Style::default().fg(Color::White) };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(display, value_style)])),
        layout[2],
    );

    // `cursor` is bounded by the input length, which the key handler caps at 64
    // hex chars, so the conversion never truncates in practice; the saturating
    // cast guards against future changes to that invariant.
    let cursor_col = inner.x + 2 + u16::try_from(cursor).unwrap_or(u16::MAX);
    let cursor_col = cursor_col.min(inner.x + inner.width.saturating_sub(1));
    f.set_cursor_position((cursor_col, layout[2].y));

    let progress_line = Paragraph::new(Line::from(vec![Span::styled(
        progress,
        Style::default().fg(progress_color),
    )]));
    f.render_widget(progress_line, layout[3]);

    if prefilled {
        let prefill_hint = Paragraph::new(Line::from(vec![Span::styled(
            "Prefilled from latest poll. F5 to refresh, edit to override.",
            Style::default().fg(Color::Cyan),
        )]));
        f.render_widget(prefill_hint, layout[5]);
    }

    let hint = Paragraph::new(Line::from(vec![Span::styled(
        "Enter confirm   F5 refresh   Esc cancel",
        Style::default().fg(Color::DarkGray),
    )]));
    f.render_widget(hint, layout[6]);
}

fn trim_prefix(input: &str) -> &str {
    input.strip_prefix("0x").or_else(|| input.strip_prefix("0X")).unwrap_or(input)
}

fn render_cluster_table(
    f: &mut Frame<'_>,
    area: Rect,
    nodes: &[ConductorNodeStatus],
    selected: usize,
    op_pending: bool,
    paused_nodes: &HashMap<String, PausedPeers>,
) {
    let title = if op_pending { " HA Conductor [working…] " } else { " HA Conductor " };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    render_pause_all_banner(f, inner_chunks[0], nodes, op_pending);
    let inner = inner_chunks[2];

    debug_assert!(!nodes.is_empty(), "render_cluster_table requires at least one node");
    let node_count = nodes.len();
    let label_pct = 15u16;
    let node_pct = (100u16 - label_pct) / node_count.max(1) as u16;

    let mut constraints = vec![Constraint::Percentage(label_pct)];
    for _ in 0..node_count {
        constraints.push(Constraint::Percentage(node_pct));
    }

    // ── Fork detection: find leader's unsafe and safe hashes ──────────────
    let leader_unsafe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.unsafe_l2_block.zip(n.unsafe_l2_hash) } else { None }
    });
    let leader_safe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.safe_l2_block.zip(n.safe_l2_hash) } else { None }
    });

    // ── Header row: node names ─────────────────────────────────────────────
    let mut header_cells = vec![Cell::from("")];
    for (i, node) in nodes.iter().enumerate() {
        let is_selected = i == selected;
        let role_color = match node.is_leader {
            Some(true) => Color::Yellow,
            Some(false) => Color::DarkGray,
            None => Color::Red,
        };
        let mut mods = Modifier::BOLD;
        if is_selected {
            mods |= Modifier::UNDERLINED;
        }
        let style = Style::default().fg(role_color).add_modifier(mods);
        let label = if node.discovered { format!("{} (d)", node.name) } else { node.name.clone() };
        header_cells.push(Cell::from(label).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .height(1);

    // ── Role row ───────────────────────────────────────────────────────────
    let mut role_cells = vec![
        Cell::from("  Role").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = if paused_nodes.contains_key(&node.name) {
            ("⏸  isolated", Style::default().fg(Color::Cyan))
        } else {
            match node.is_leader {
                Some(true) => {
                    ("★  LEADER", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                }
                Some(false) => ("   follower", Style::default().fg(Color::DarkGray)),
                None => ("   offline", Style::default().fg(Color::Red)),
            }
        };
        role_cells.push(Cell::from(label).style(style));
    }
    let role_row = Row::new(role_cells).height(1);

    // ── Active row (conductor) ─────────────────────────────────────────────
    let mut active_cells = vec![
        Cell::from("  Active")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = if paused_nodes.contains_key(&node.name) {
            ("   isolated", Style::default().fg(Color::Cyan))
        } else {
            match (node.is_leader, node.conductor_active) {
                (Some(true), Some(true)) => ("   yes", Style::default().fg(Color::Green)),
                (Some(true), Some(false)) => ("   no", Style::default().fg(Color::Red)),
                (Some(false), Some(false)) => ("   no", Style::default().fg(Color::DarkGray)),
                (Some(false), Some(true)) => ("   yes", Style::default().fg(Color::Yellow)),
                _ => ("   ?", Style::default().fg(Color::DarkGray)),
            }
        };
        active_cells.push(Cell::from(label).style(style));
    }
    let active_row = Row::new(active_cells).height(1);

    let paused_row = bool_row(
        "  Paused",
        nodes,
        |n| n.conductor_paused,
        Color::Cyan,
        Color::Green,
        ("   yes", "   no"),
    );

    let stopped_row = bool_row(
        "  Stopped",
        nodes,
        |n| n.conductor_stopped,
        Color::Red,
        Color::Green,
        ("   yes", "   no"),
    );

    let healthy_row = bool_row(
        "  Healthy",
        nodes,
        |n| n.sequencer_healthy,
        Color::Green,
        Color::Red,
        ("   yes", "   no"),
    );

    // ── Seq active row (admin RPC) ─────────────────────────────────────────
    let mut seq_active_cells = vec![
        Cell::from("  Seq active")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = if paused_nodes.contains_key(&node.name) {
            ("   isolated", Style::default().fg(Color::Cyan))
        } else {
            match (node.is_leader, node.sequencer_active) {
                (Some(true), Some(true)) => ("   yes", Style::default().fg(Color::Green)),
                (Some(true), Some(false)) => ("   no", Style::default().fg(Color::Red)),
                (Some(false), Some(false)) => ("   no", Style::default().fg(Color::DarkGray)),
                (Some(false), Some(true)) => ("   yes", Style::default().fg(Color::Yellow)),
                _ => ("   ?", Style::default().fg(Color::DarkGray)),
            }
        };
        seq_active_cells.push(Cell::from(label).style(style));
    }
    let seq_active_row = Row::new(seq_active_cells).height(1);

    // ── Unsafe L2 row ──────────────────────────────────────────────────────
    let mut l2_cells = vec![
        Cell::from("  Unsafe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.unsafe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    // ── Unsafe L2 hash row ────────────────────────────────────────────────
    let mut l2_hash_cells = vec![
        Cell::from("  Unsafe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.unsafe_l2_hash {
            Some(h) if node.is_leader == Some(true) => {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::Yellow))
            }
            Some(h) => {
                let hex = format!("{h:x}");
                let is_fork = leader_unsafe
                    .is_some_and(|(lnum, lhash)| node.unsafe_l2_block == Some(lnum) && h != lhash);
                if is_fork {
                    (format!("   ⚠ 0x{}…", &hex[..8]), Style::default().fg(Color::Red))
                } else {
                    (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
                }
            }
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l2_hash_cells.push(Cell::from(label).style(style));
    }
    let l2_hash_row = Row::new(l2_hash_cells).height(1);

    // ── Safe L2 row ────────────────────────────────────────────────────────
    let mut safe_l2_cells = vec![
        Cell::from("  Safe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.safe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    // ── Safe L2 hash row ──────────────────────────────────────────────────
    let mut safe_hash_cells = vec![
        Cell::from("  Safe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.safe_l2_hash {
            Some(h) if node.is_leader == Some(true) => {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::Yellow))
            }
            Some(h) => {
                let hex = format!("{h:x}");
                let is_fork = leader_safe
                    .is_some_and(|(lnum, lhash)| node.safe_l2_block == Some(lnum) && h != lhash);
                if is_fork {
                    (format!("   ⚠ 0x{}…", &hex[..8]), Style::default().fg(Color::Red))
                } else {
                    (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
                }
            }
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        safe_hash_cells.push(Cell::from(label).style(style));
    }
    let safe_hash_row = Row::new(safe_hash_cells).height(1);

    // ── Finalized L2 row ───────────────────────────────────────────────────
    let mut finalized_l2_cells = vec![
        Cell::from("  Finalized L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.finalized_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    // ── L1 derivation row ──────────────────────────────────────────────────
    let mut l1_cells = vec![
        Cell::from("  L1 Derived")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   #{cur} / #{head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    // ── CL peer count row ──────────────────────────────────────────────────
    let mut cl_peers_cells = vec![
        Cell::from("  CL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    // ── EL block row ───────────────────────────────────────────────────────
    let mut el_block_cells = vec![
        Cell::from("  Block").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_block {
            Some(n) if node.is_leader == Some(true) => {
                (format!("   #{n}"), Style::default().fg(Color::Yellow))
            }
            Some(n) => (format!("   #{n}"), Style::default().fg(Color::White)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    // ── EL syncing row ─────────────────────────────────────────────────────
    let mut el_syncing_cells = vec![
        Cell::from("  Syncing")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    // ── EL peer count row ──────────────────────────────────────────────────
    let mut el_peers_cells = vec![
        Cell::from("  EL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_peers_cells.push(Cell::from(label).style(style));
    }
    let el_peers_row = Row::new(el_peers_cells).height(1);

    let cl_section = section_row("CL", node_count);
    let el_section = section_row("EL", node_count);
    let spacer = Row::new(vec![Cell::from("")]).height(1);

    let rows = vec![
        // ── Conductor ────────────────────────────────────────────────────
        role_row,
        active_row,
        paused_row,
        stopped_row,
        healthy_row,
        seq_active_row,
        // ── CL ───────────────────────────────────────────────────────────
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
        // ── EL ───────────────────────────────────────────────────────────
        spacer,
        el_section,
        el_block_row,
        el_syncing_row,
        el_peers_row,
    ];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}

/// Renders the cluster-wide control-loop status and the always-visible
/// `[ P ] Pause all` / `[ R ] Resume all` button strip.
///
/// The status segment summarises `conductor_paused` across every node so the
/// affordance for "pause all" is obvious without remembering a shortcut.
fn render_pause_all_banner(
    f: &mut Frame<'_>,
    area: Rect,
    nodes: &[ConductorNodeStatus],
    op_pending: bool,
) {
    let total = nodes.len();
    let paused = nodes.iter().filter(|n| n.conductor_paused == Some(true)).count();
    let known = nodes.iter().filter(|n| n.conductor_paused.is_some()).count();

    let active = known - paused;
    let (status_label, status_color) = if known == 0 {
        ("control loop: status unknown".to_string(), Color::DarkGray)
    } else if known < total {
        (
            format!(
                "control loop: PARTIAL REPORT ({paused} paused, {active} active, {} unknown of {total})",
                total - known
            ),
            Color::DarkGray,
        )
    } else if paused == total {
        (format!("control loop: ALL PAUSED ({paused}/{total})"), Color::Cyan)
    } else if paused == 0 {
        (format!("control loop: ALL ACTIVE ({total}/{total})"), Color::Green)
    } else {
        (format!("control loop: MIXED ({paused}/{total} paused)"), Color::Yellow)
    };

    let key_style =
        Style::default().fg(Color::Black).bg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let working = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'_>> = vec![
        Span::styled(status_label, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled("   ·   ", dim),
    ];

    if op_pending {
        spans.push(Span::styled("[ working… ]", working));
    } else {
        spans.push(Span::styled(" P ", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("Pause all", label_style));
        spans.push(Span::styled("    ", dim));
        spans.push(Span::styled(" R ", key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("Resume all", label_style));
    }

    f.render_widget(Paragraph::new(Line::from(spans)).alignment(Alignment::Center), area);
}

/// Builds a row that renders a tri-state `Option<bool>` per node.
///
/// `true_color` and `false_color` style the corresponding labels;
/// `None` always renders as a grey `?`.
fn bool_row<'a>(
    label: &'static str,
    nodes: &[ConductorNodeStatus],
    extract: impl Fn(&ConductorNodeStatus) -> Option<bool>,
    true_color: Color,
    false_color: Color,
    labels: (&'static str, &'static str),
) -> Row<'a> {
    let mut cells = vec![
        Cell::from(label).style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (text, style) = match extract(node) {
            Some(true) => (labels.0, Style::default().fg(true_color)),
            Some(false) => (labels.1, Style::default().fg(false_color)),
            None => ("   ?", Style::default().fg(Color::DarkGray)),
        };
        cells.push(Cell::from(text).style(style));
    }
    Row::new(cells).height(1)
}

fn render_validator_table(f: &mut Frame<'_>, area: Rect, nodes: &[ValidatorNodeStatus]) {
    let block = Block::default()
        .title(" Validators ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    debug_assert!(!nodes.is_empty(), "render_validator_table requires at least one node");
    let node_count = nodes.len();
    let label_pct = 15u16;
    let node_pct = (100u16 - label_pct) / node_count.max(1) as u16;

    let mut constraints = vec![Constraint::Percentage(label_pct)];
    for _ in 0..node_count {
        constraints.push(Constraint::Percentage(node_pct));
    }

    // ── Header row: node names ─────────────────────────────────────────────
    let mut header_cells = vec![Cell::from("")];
    for node in nodes {
        let style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        header_cells.push(Cell::from(node.name.as_str()).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .height(1);

    // ── Binary row ────────────────────────────────────────────────────────
    let mut binary_cells = vec![
        Cell::from("  Binary")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.binary.as_ref().map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |binary| (format!("   {binary}"), Style::default().fg(Color::Cyan)),
        );
        binary_cells.push(Cell::from(label).style(style));
    }
    let binary_row = Row::new(binary_cells).height(1);

    // ── CL section header ──────────────────────────────────────────────────
    let cl_section = section_row("CL", node_count);

    // ── Unsafe L2 row ──────────────────────────────────────────────────────
    let mut l2_cells = vec![
        Cell::from("  Unsafe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.unsafe_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    // ── Unsafe L2 hash row ────────────────────────────────────────────────
    let mut l2_hash_cells = vec![
        Cell::from("  Unsafe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.unsafe_l2_hash.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |h| {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
            },
        );
        l2_hash_cells.push(Cell::from(label).style(style));
    }
    let l2_hash_row = Row::new(l2_hash_cells).height(1);

    // ── Safe L2 row ────────────────────────────────────────────────────────
    let mut safe_l2_cells = vec![
        Cell::from("  Safe L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.safe_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    // ── Safe L2 hash row ──────────────────────────────────────────────────
    let mut safe_hash_cells = vec![
        Cell::from("  Safe Hash")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.safe_l2_hash.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |h| {
                let hex = format!("{h:x}");
                (format!("   0x{}…", &hex[..8]), Style::default().fg(Color::White))
            },
        );
        safe_hash_cells.push(Cell::from(label).style(style));
    }
    let safe_hash_row = Row::new(safe_hash_cells).height(1);

    // ── Finalized L2 row ───────────────────────────────────────────────────
    let mut finalized_l2_cells = vec![
        Cell::from("  Finalized L2")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.finalized_l2_block.map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    // ── L1 derivation row ──────────────────────────────────────────────────
    let mut l1_cells = vec![
        Cell::from("  L1 Derived")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   #{cur} / #{head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    // ── CL peer count row ──────────────────────────────────────────────────
    let mut cl_peers_cells = vec![
        Cell::from("  CL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    // ── EL section header ──────────────────────────────────────────────────
    let el_section = section_row("EL", node_count);

    // ── EL block row ───────────────────────────────────────────────────────
    let mut el_block_cells = vec![
        Cell::from("  Block").style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = node.el_block.map_or_else(
            || ("   -".to_string(), Style::default().fg(Color::DarkGray)),
            |n| (format!("   #{n}"), Style::default().fg(Color::White)),
        );
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    // ── EL syncing row ─────────────────────────────────────────────────────
    let mut el_syncing_cells = vec![
        Cell::from("  Syncing")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    // ── EL peer count row ──────────────────────────────────────────────────
    let mut el_peers_cells = vec![
        Cell::from("  EL Peers")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for node in nodes {
        let (label, style) = match node.el_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   -".to_string(), Style::default().fg(Color::DarkGray)),
        };
        el_peers_cells.push(Cell::from(label).style(style));
    }
    let el_peers_row = Row::new(el_peers_cells).height(1);

    let spacer = Row::new(vec![Cell::from("")]).height(1);

    let rows = vec![
        binary_row,
        // ── CL ───────────────────────────────────────────────────────────
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
        // ── EL ───────────────────────────────────────────────────────────
        spacer,
        el_section,
        el_block_row,
        el_syncing_row,
        el_peers_row,
    ];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}

/// Creates a styled section-separator row for the cluster table.
///
/// Renders as `── LABEL ──────────────` in the label column and `──────────────`
/// in every data column, extending the visual divider fully across all columns.
fn section_row(label: &str, node_count: usize) -> Row<'static> {
    let sep_style = Style::default().fg(Color::DarkGray);
    let heading = format!("── {label} ──────────────");
    let mut cells = vec![Cell::from(heading).style(sep_style)];
    for _ in 0..node_count {
        cells.push(Cell::from("──────────────").style(sep_style));
    }
    Row::new(cells).height(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_bare_hex_hash() {
        let parsed = parse_hex_hash(SAMPLE_HEX).expect("bare 64-char hex parses");
        assert_eq!(parsed, B256::from_str(SAMPLE_HEX).unwrap());
    }

    #[test]
    fn parses_0x_prefixed_hex_hash() {
        let prefixed = format!("0x{SAMPLE_HEX}");
        let parsed = parse_hex_hash(&prefixed).expect("0x-prefixed 64-char hex parses");
        assert_eq!(parsed, B256::from_str(SAMPLE_HEX).unwrap());
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(parse_hex_hash("dead").is_none());
        assert!(parse_hex_hash(&format!("0x{SAMPLE_HEX}ff")).is_none());
    }

    #[test]
    fn rejects_non_hex() {
        let bad = "g".repeat(64);
        assert!(parse_hex_hash(&bad).is_none());
    }
}
