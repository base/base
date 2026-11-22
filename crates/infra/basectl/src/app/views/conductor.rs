use std::{collections::HashMap, str::FromStr};

use alloy_primitives::B256;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Flex, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::mpsc;

use super::conductor_gate::GateProposals;
use crate::{
    GateSettings,
    app::{Action, GatewayClusterView, Resources, View},
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
    Keybinding { key: "t", description: "Transfer leader (any); propose via gateway when set" },
    Keybinding { key: "p", description: "Approval gateway proposals (gateway mode)" },
    Keybinding { key: "P", description: "Pause conductor on all nodes" },
    Keybinding { key: "R", description: "Resume conductor on all nodes" },
    Keybinding { key: "Esc", description: "Back to home" },
    Keybinding { key: "?", description: "Toggle help" },
];

type PauseRx = Option<(String, mpsc::Receiver<Result<(String, PausedPeers), String>>)>;

const BLOCK_DELTA_RED_THRESHOLD: i128 = 100;

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

    /// Whether the action can run in `mode`; if not, how the menu shows and
    /// refuses it. This is the one place these rules live.
    ///
    /// With an approval gateway, a transfer needs a target, so "any peer" is
    /// refused and the menu closes. The gateway's cluster view only supports
    /// proposing a transfer to a node; everything else needs direct
    /// conductor RPC access and is greyed out.
    pub fn available(
        self,
        node: &ConductorNodeStatus,
        is_p2p_isolated: bool,
        mode: MenuMode,
    ) -> MenuAvailability {
        match (mode, self) {
            (MenuMode::Gateway | MenuMode::DirectWithGate, Self::TransferLeaderAny) => {
                MenuAvailability::Blocked {
                    toast: Some(ConductorView::NEEDS_TARGET),
                    close_menu: true,
                    greyed: mode == MenuMode::Gateway,
                }
            }
            (MenuMode::Gateway, item) if item != Self::TransferLeaderHere => {
                MenuAvailability::Blocked {
                    toast: Some(ConductorView::NEEDS_DIRECT_ACCESS),
                    close_menu: false,
                    greyed: true,
                }
            }
            _ if !self.enabled(node, is_p2p_isolated) => {
                MenuAvailability::Blocked { toast: None, close_menu: false, greyed: true }
            }
            _ => MenuAvailability::Allowed,
        }
    }
}

/// Where the conductor view's data comes from and where actions go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuMode {
    /// Direct conductor RPC, no approval gateway.
    Direct,
    /// The approval gateway's cluster view, with no conductor RPC access.
    Gateway,
    /// Direct conductor RPC for reads, with leader transfers proposed through
    /// the approval gateway.
    DirectWithGate,
}

impl MenuMode {
    /// The mode for the current config and data source.
    pub fn of(resources: &Resources) -> Self {
        if resources.conductor.gateway.is_some() {
            Self::Gateway
        } else if GateSettings::configured(&resources.config) {
            Self::DirectWithGate
        } else {
            Self::Direct
        }
    }
}

/// Whether an action menu item can run, from [`ActionMenuItem::available`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAvailability {
    /// The item can run.
    Allowed,
    /// The item can't run.
    Blocked {
        /// Shown when the item is chosen, saying why.
        toast: Option<&'static str>,
        /// Whether choosing the item closes the menu.
        close_menu: bool,
        /// Whether the menu shows the item greyed out.
        greyed: bool,
    },
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
    pub(super) selected: usize,
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
    /// Approval gateway proposals, their overlays and requests.
    pub(super) gate: GateProposals,
}

impl ConductorView {
    /// Shown when `t` is pressed in gateway mode before the cluster view arrives.
    pub const NO_GATEWAY_NODES: &str =
        "No nodes yet: waiting for the approval gateway's cluster view";
    /// Shown when a leader transfer is requested with nowhere to send it.
    pub const NO_CONDUCTOR_CONFIG: &str = "Leader transfer needs a conductor config (`conductors` or `--conductor-rpc`) or an approval gateway (`gate_url`)";
    /// Shown when "Transfer leader (any peer)" is chosen with an approval gateway.
    pub const NEEDS_TARGET: &str = "The approval gateway needs a target: use Transfer leader here";
    /// Shown when `t` is pressed with an approval gateway on a node that is the
    /// leader or whose role is unknown.
    pub const NEEDS_FOLLOWER: &str =
        "Leader transfer needs a selected node that is known not to be the leader";
    /// Shown when an action needs direct conductor RPC access in gateway mode.
    pub const NEEDS_DIRECT_ACCESS: &str =
        "Not available via the approval gateway: this action needs direct conductor RPC access";

    /// Creates a new conductor view with no overlay open.
    pub fn new() -> Self {
        Self::default()
    }

    const fn is_overlay_open(&self) -> bool {
        !matches!(self.overlay, Overlay::None) || self.gate.is_open()
    }

    pub(super) fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.gate.close();
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

    /// Resolves an allowed menu item into an overlay transition. With an
    /// approval gateway, "Transfer leader here" opens the proposal form.
    fn select_menu_item(
        &mut self,
        item: ActionMenuItem,
        node: &ConductorNodeStatus,
        is_p2p_isolated: bool,
        mode: MenuMode,
    ) {
        let name = node.name.clone();
        if mode != MenuMode::Direct && item == ActionMenuItem::TransferLeaderHere {
            self.overlay = Overlay::None;
            self.gate.open_form(name);
            return;
        }
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
    fn execute(&mut self, action: PendingAction, resources: &mut Resources) {
        let nodes_cfg = resources.conductor.nodes_config();
        if nodes_cfg.is_empty() {
            self.close_overlay();
            let message = if resources.conductor.gateway.is_some() {
                Self::NEEDS_DIRECT_ACCESS
            } else {
                Self::NO_CONDUCTOR_CONFIG
            };
            resources.toasts.push(Toast::warning(message));
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
        self.gate.tick(resources);

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

        if self.gate.is_open() {
            self.gate.handle_key(key, resources);
            return Action::None;
        }

        match &mut self.overlay {
            Overlay::HashInput { node: target, input, cursor, prefilled } => {
                match key.code {
                    KeyCode::Esc => self.close_overlay(),
                    KeyCode::Backspace if *cursor > 0 => {
                        *cursor -= 1;
                        input.remove(*cursor);
                        *prefilled = false;
                    }
                    KeyCode::Left if *cursor > 0 => {
                        *cursor -= 1;
                    }
                    KeyCode::Right if *cursor < input.len() => {
                        *cursor += 1;
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
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && c.is_ascii_hexdigit()
                            && input.len() < 64 =>
                    {
                        input.insert(*cursor, c);
                        *cursor += 1;
                        *prefilled = false;
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
                            let mode = MenuMode::of(resources);
                            match item.available(&node, is_p2p_isolated, mode) {
                                MenuAvailability::Allowed => {
                                    self.select_menu_item(item, &node, is_p2p_isolated, mode);
                                }
                                MenuAvailability::Blocked { toast, close_menu, .. } => {
                                    if let Some(message) = toast {
                                        resources.toasts.push(Toast::warning(message));
                                    }
                                    if close_menu {
                                        self.close_overlay();
                                    }
                                }
                            }
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
                let mode = MenuMode::of(resources);
                let gate_enabled = mode != MenuMode::Direct;
                match self.selected_node(&resources.conductor.nodes) {
                    Some(node) if gate_enabled => {
                        let is_p2p_isolated = self.paused_node_peers.contains_key(&node.name);
                        match ActionMenuItem::TransferLeaderHere.available(
                            node,
                            is_p2p_isolated,
                            mode,
                        ) {
                            MenuAvailability::Allowed => self.gate.open_form(node.name.clone()),
                            MenuAvailability::Blocked { toast, .. } => resources
                                .toasts
                                .push(Toast::warning(toast.unwrap_or(Self::NEEDS_FOLLOWER))),
                        }
                    }
                    _ if gate_enabled => {
                        resources.toasts.push(Toast::warning(Self::NO_GATEWAY_NODES));
                    }
                    _ if resources.conductor.nodes_config().is_empty() => {
                        resources.toasts.push(Toast::warning(Self::NO_CONDUCTOR_CONFIG));
                    }
                    _ => {
                        self.overlay = Overlay::Confirm {
                            action: PendingAction::TransferAny,
                            button: ConfirmButton::No,
                        };
                    }
                }
            }
            KeyCode::Char('p') if GateSettings::configured(&resources.config) => {
                self.gate.open_list(resources);
            }
            KeyCode::Char('P' | 'R') if resources.conductor.gateway.is_some() => {
                resources.toasts.push(Toast::warning(Self::NEEDS_DIRECT_ACCESS));
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
        let gateway = resources.conductor.gateway.as_ref();

        if validators.is_empty() {
            if nodes.is_empty() {
                render_unconfigured(frame, content_area, gateway);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    content_area,
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                    gateway,
                );
            }
        } else {
            let conductor_height = 25u16;
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(conductor_height), Constraint::Min(0)])
                .split(content_area);

            if nodes.is_empty() {
                render_unconfigured(frame, sections[0], gateway);
            } else {
                let selected = self.selected.min(nodes.len().saturating_sub(1));
                render_cluster_table(
                    frame,
                    sections[0],
                    nodes,
                    selected,
                    self.op_pending,
                    &self.paused_node_peers,
                    gateway,
                );
            }
            render_validator_table(frame, sections[1], validators);
        }

        render_footer(
            frame,
            footer_area,
            &self.overlay,
            self.gate.footer_keys(),
            self.op_pending,
            gateway.is_some(),
        );

        match &self.overlay {
            Overlay::None => {}
            Overlay::ActionMenu { cursor } => {
                if let Some(node) = self.selected_node(nodes) {
                    let is_p2p_isolated = self.paused_node_peers.contains_key(&node.name);
                    let mode = MenuMode::of(resources);
                    render_action_menu(frame, area, node, *cursor, is_p2p_isolated, gateway, mode);
                }
            }
            Overlay::Confirm { action, button } => {
                render_confirm(
                    frame,
                    area,
                    &action.description(),
                    action.is_destructive(),
                    *button,
                );
            }
            Overlay::HashInput { node, input, cursor, prefilled } => {
                render_hash_input(frame, area, node, input, *cursor, *prefilled);
            }
        }
        self.gate.render(frame, area, &resources.toasts);
    }
}

/// Parses a hex-encoded 32-byte hash, accepting both `0x`-prefixed and bare forms.
fn parse_hex_hash(input: &str) -> Option<B256> {
    B256::from_str(input).ok()
}

fn render_unconfigured(f: &mut Frame<'_>, area: Rect, gateway: Option<&GatewayClusterView>) {
    let title = if gateway.is_some() { GATEWAY_TITLE } else { " HA Conductor " };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let msg = match gateway {
        Some(GatewayClusterView { notice: Some(notice), .. }) => {
            Paragraph::new(notice.escape_debug().to_string()).style(Style::default().fg(Color::Red))
        }
        Some(_) => Paragraph::new("Waiting for the approval gateway's cluster view…")
            .style(Style::default().fg(Color::DarkGray)),
        None => Paragraph::new("Conductor monitoring requires a config with conductor endpoints.")
            .style(Style::default().fg(Color::DarkGray)),
    }
    .alignment(Alignment::Center);

    f.render_widget(msg, chunks[1]);
}

/// Footer keys while a Yes/No confirmation is shown.
pub(super) const CONFIRM_KEYS: &[(&str, &str)] =
    &[("←/→", "Yes / No"), ("Enter", "confirm"), ("y/n", "shortcut"), ("Esc", "cancel")];

fn render_footer(
    f: &mut Frame<'_>,
    area: Rect,
    overlay: &Overlay,
    gate_keys: Option<&[(&'static str, &'static str)]>,
    op_pending: bool,
    gateway_feed: bool,
) {
    let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let sep = Span::styled("  │  ", desc_style);

    let mut spans: Vec<Span<'_>> = Vec::new();
    let push_pair = |spans: &mut Vec<Span<'_>>, key: &'static str, desc: &'static str| {
        spans.push(Span::styled(format!("[{key}]"), key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc, desc_style));
    };

    let push_pairs = |spans: &mut Vec<Span<'_>>, pairs: &[(&'static str, &'static str)]| {
        for (i, (key, desc)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(sep.clone());
            }
            push_pair(spans, key, desc);
        }
    };

    match overlay {
        _ if let Some(keys) = gate_keys => push_pairs(&mut spans, keys),
        Overlay::None => {
            push_pair(&mut spans, "Esc", "back");
            spans.push(sep.clone());
            push_pair(&mut spans, "←/→", "select node");
            spans.push(sep.clone());
            if op_pending {
                spans.push(Span::styled("working…", Style::default().fg(Color::Yellow)));
            } else if gateway_feed {
                push_pair(&mut spans, "Enter", "actions");
                spans.push(sep.clone());
                push_pair(&mut spans, "t", "propose transfer");
                spans.push(sep.clone());
                push_pair(&mut spans, "p", "proposals");
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
        Overlay::Confirm { .. } => push_pairs(&mut spans, CONFIRM_KEYS),
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
    gateway: Option<&GatewayClusterView>,
    mode: MenuMode,
) {
    // The table cell ellipsizes the gateway's error; the panel shows it in full.
    let error = gateway
        .and_then(|gateway| gateway.cluster.as_ref())
        .and_then(|cluster| cluster.error_for(&node.name))
        .map(|error| format!("Error: {}", error.escape_debug()));
    let popup_w = if error.is_some() { 64u16 } else { 44 }.min(area.width.saturating_sub(4));
    let error_rows = error.as_ref().map_or(0, |error| {
        // One extra row absorbs word-wrap slack.
        let text_w = usize::from(popup_w.saturating_sub(2)).max(1);
        error.chars().count().div_ceil(text_w) as u16 + 2
    });
    let popup_h = (MENU_ITEMS.len() as u16 + 5 + error_rows).min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(Clear, popup);

    let title = format!(" Actions: {} ", node.name.escape_debug());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(MENU_ITEMS.len() + 2);
    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let enabled = !matches!(
            item.available(node, is_p2p_isolated, mode),
            MenuAvailability::Blocked { greyed: true, .. }
        );
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
    if let Some(error) = error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(error, Style::default().fg(Color::Red)));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Shortens `text` to at most `width` display columns, ending with `…` when
/// anything was cut.
fn ellipsize(text: &str, width: usize) -> String {
    if Line::raw(text).width() <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 0;
    for c in text.chars() {
        let char_width = Span::raw(c.to_string()).width();
        if used + char_width + 1 > width {
            break;
        }
        used += char_width;
        kept.push(c);
    }
    if width > 0 {
        kept.push('…');
    }
    kept
}

/// Draws a centered Yes/No confirmation for `description`; a destructive
/// action gets a red Yes button.
pub(super) fn render_confirm(
    f: &mut Frame<'_>,
    area: Rect,
    description: &str,
    destructive: bool,
    button: ConfirmButton,
) {
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

    let body = Paragraph::new(description)
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

    let yes_color = if destructive { Color::Red } else { Color::Green };
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

fn latest_block<T>(nodes: &[T], extract: impl Copy + Fn(&T) -> Option<u64>) -> Option<u64> {
    nodes.iter().filter_map(extract).max()
}

fn block_label(block: u64, baseline: Option<u64>) -> Line<'static> {
    let Some(baseline) = baseline else { return Line::from(format!("   {block}")) };
    let delta = i128::from(block) - i128::from(baseline);
    if delta == 0 {
        return Line::from(format!("   {block}"));
    }

    let delta_style = if delta < -BLOCK_DELTA_RED_THRESHOLD {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::raw(format!("   {block} ")),
        Span::styled(format!("({delta:+})"), delta_style),
    ])
}

fn render_cluster_table(
    f: &mut Frame<'_>,
    area: Rect,
    nodes: &[ConductorNodeStatus],
    selected: usize,
    op_pending: bool,
    paused_nodes: &HashMap<String, PausedPeers>,
    gateway: Option<&GatewayClusterView>,
) {
    let title = match (gateway.is_some(), op_pending) {
        (true, true) => " HA Conductor · via approval gateway [working…] ",
        (true, false) => GATEWAY_TITLE,
        (false, true) => " HA Conductor [working…] ",
        (false, false) => " HA Conductor ",
    };

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
    render_pause_all_banner(f, inner_chunks[0], nodes, op_pending, gateway.is_none());
    if let Some(gateway) = gateway {
        render_gateway_status(f, inner_chunks[1], gateway);
    }
    let inner = inner_chunks[2];

    debug_assert!(!nodes.is_empty(), "render_cluster_table requires at least one node");
    let node_count = nodes.len();
    let label_pct = 15u16;
    let node_pct = (100u16 - label_pct) / node_count.max(1) as u16;

    let mut constraints = vec![Constraint::Percentage(label_pct)];
    for _ in 0..node_count {
        constraints.push(Constraint::Percentage(node_pct));
    }

    let leader_unsafe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.unsafe_l2_block.zip(n.unsafe_l2_hash) } else { None }
    });
    let leader_safe: Option<(u64, alloy_primitives::B256)> = nodes.iter().find_map(|n| {
        if n.is_leader == Some(true) { n.safe_l2_block.zip(n.safe_l2_hash) } else { None }
    });
    let unsafe_l2_baseline = latest_block(nodes, |node| node.unsafe_l2_block);
    let safe_l2_baseline = latest_block(nodes, |node| node.safe_l2_block);
    let finalized_l2_baseline = latest_block(nodes, |node| node.finalized_l2_block);
    let el_block_baseline = latest_block(nodes, |node| node.el_block);

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
        let name = node.name.escape_debug();
        let label = if node.discovered { format!("{name} (d)") } else { name.to_string() };
        header_cells.push(Cell::from(label).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .height(1);

    let label_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let mut role_cells = vec![Cell::from("  Role").style(label_style)];
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

    let mut active_cells = vec![Cell::from("  Active").style(label_style)];
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

    let mut seq_active_cells = vec![Cell::from("  Seq active").style(label_style)];
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

    let mut l2_cells = vec![Cell::from("  Unsafe L2").style(label_style)];
    for node in nodes {
        let (label, style) = match node.unsafe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (block_label(n, unsafe_l2_baseline), Style::default().fg(Color::Yellow))
            }
            Some(n) => (block_label(n, unsafe_l2_baseline), Style::default().fg(Color::White)),
            None => (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
        };
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    let mut l2_hash_cells = vec![Cell::from("  Unsafe Hash").style(label_style)];
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

    let mut safe_l2_cells = vec![Cell::from("  Safe L2").style(label_style)];
    for node in nodes {
        let (label, style) = match node.safe_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (block_label(n, safe_l2_baseline), Style::default().fg(Color::Yellow))
            }
            Some(n) => (block_label(n, safe_l2_baseline), Style::default().fg(Color::White)),
            None => (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
        };
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    let mut safe_hash_cells = vec![Cell::from("  Safe Hash").style(label_style)];
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

    let mut finalized_l2_cells = vec![Cell::from("  Finalized L2").style(label_style)];
    for node in nodes {
        let (label, style) = match node.finalized_l2_block {
            Some(n) if node.is_leader == Some(true) => {
                (block_label(n, finalized_l2_baseline), Style::default().fg(Color::Yellow))
            }
            Some(n) => (block_label(n, finalized_l2_baseline), Style::default().fg(Color::White)),
            None => (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
        };
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    let mut l1_cells = vec![Cell::from("  L1 Derived").style(label_style)];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   {cur} / {head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    let mut cl_peers_cells = vec![Cell::from("  CL Peers").style(label_style)];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    let mut el_block_cells = vec![Cell::from("  Block").style(label_style)];
    for node in nodes {
        let (label, style) = match node.el_block {
            Some(n) if node.is_leader == Some(true) => {
                (block_label(n, el_block_baseline), Style::default().fg(Color::Yellow))
            }
            Some(n) => (block_label(n, el_block_baseline), Style::default().fg(Color::White)),
            None => (Line::from("   -"), Style::default().fg(Color::DarkGray)),
        };
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    let mut el_syncing_cells = vec![Cell::from("  Syncing").style(label_style)];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    let mut el_peers_cells = vec![Cell::from("  EL Peers").style(label_style)];
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

    // The gateway serves conductor state, heads, the unsafe hash, L1
    // derivation and CL peers, but not the safe hash or anything EL, so
    // those rows (which would all read `?`) are hidden; its per-node errors
    // get their own row.
    if let Some(gateway) = gateway {
        // Same column layout as the table's, so long errors can be ellipsized
        // to their cell instead of being cut mid-word.
        let columns = Layout::horizontal(constraints.clone())
            .flex(Flex::Start)
            .spacing(TABLE_COLUMN_SPACING)
            .split(inner);
        let mut error_cells = vec![Cell::from("  Error").style(label_style)];
        for (i, node) in nodes.iter().enumerate() {
            let error = gateway.cluster.as_ref().and_then(|cluster| cluster.error_for(&node.name));
            let cell_width = columns.get(i + 1).map_or(0, |column| usize::from(column.width));
            error_cells.push(error.map_or_else(
                || Cell::from("   -").style(Style::default().fg(Color::DarkGray)),
                |error| {
                    let text = format!("   {}", error.escape_debug());
                    Cell::from(ellipsize(&text, cell_width)).style(Style::default().fg(Color::Red))
                },
            ));
        }
        let rows = vec![
            role_row,
            active_row,
            paused_row,
            stopped_row,
            healthy_row,
            seq_active_row,
            Row::new(error_cells).height(1),
            spacer,
            section_row("Heads", node_count),
            l2_row,
            l2_hash_row,
            safe_l2_row,
            finalized_l2_row,
            l1_row,
            cl_peers_row,
        ];
        let table = Table::new(rows, constraints)
            .header(header)
            .flex(Flex::Start)
            .column_spacing(TABLE_COLUMN_SPACING)
            .row_highlight_style(Style::default());
        f.render_stateful_widget(table, inner, &mut TableState::default());
        return;
    }

    let rows = vec![
        role_row,
        active_row,
        paused_row,
        stopped_row,
        healthy_row,
        seq_active_row,
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
        spacer,
        el_section,
        el_block_row,
        el_syncing_row,
        el_peers_row,
    ];
    let table = Table::new(rows, constraints).header(header).row_highlight_style(Style::default());

    f.render_stateful_widget(table, inner, &mut TableState::default());
}

/// Gap between cluster table columns (ratatui's default, set explicitly so the
/// error row's ellipsis width matches the table layout).
const TABLE_COLUMN_SPACING: u16 = 1;

/// Block title in gateway mode.
const GATEWAY_TITLE: &str = " HA Conductor · via approval gateway ";

/// Renders the gateway's refresh error, or where the data came from.
fn render_gateway_status(f: &mut Frame<'_>, area: Rect, gateway: &GatewayClusterView) {
    let line = gateway.notice.as_ref().map_or_else(
        || {
            let cluster = gateway.cluster.as_ref();
            let mut parts = Vec::new();
            if let Some(network) = cluster.and_then(|c| c.network.as_deref()) {
                parts.push(network.escape_debug().to_string());
            }
            if let Some(version) = cluster.and_then(|c| c.membership_version) {
                parts.push(format!("membership v{version}"));
            }
            if let Some(as_of) = cluster.and_then(|c| c.as_of) {
                parts.push(format!("as of {}", as_of.to_rfc3339()));
            }
            Line::styled(parts.join("  ·  "), Style::default().fg(Color::DarkGray))
        },
        |notice| Line::styled(notice.escape_debug().to_string(), Style::default().fg(Color::Red)),
    );
    f.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

/// Renders the cluster-wide control-loop status and the always-visible
/// `[ P ] Pause all` / `[ R ] Resume all` button strip.
///
/// The status segment summarises `conductor_paused` across every node so the
/// affordance for "pause all" is obvious without remembering a shortcut. The
/// buttons are hidden when `show_controls` is false (gateway mode, where
/// pausing needs direct conductor RPC access).
fn render_pause_all_banner(
    f: &mut Frame<'_>,
    area: Rect,
    nodes: &[ConductorNodeStatus],
    op_pending: bool,
    show_controls: bool,
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

    let mut spans: Vec<Span<'_>> = vec![Span::styled(
        status_label,
        Style::default().fg(status_color).add_modifier(Modifier::BOLD),
    )];

    if show_controls {
        spans.push(Span::styled("   ·   ", dim));
    }
    if show_controls && op_pending {
        spans.push(Span::styled("[ working… ]", working));
    } else if show_controls {
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
    let unsafe_l2_baseline = latest_block(nodes, |node| node.unsafe_l2_block);
    let safe_l2_baseline = latest_block(nodes, |node| node.safe_l2_block);
    let finalized_l2_baseline = latest_block(nodes, |node| node.finalized_l2_block);
    let el_block_baseline = latest_block(nodes, |node| node.el_block);

    let mut header_cells = vec![Cell::from("")];
    for node in nodes {
        let style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        header_cells.push(Cell::from(node.name.as_str()).style(style));
    }
    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .height(1);

    let label_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let mut binary_cells = vec![Cell::from("  Binary").style(label_style)];
    for node in nodes {
        let (label, style) = node.binary.as_ref().map_or_else(
            || ("   ?".to_string(), Style::default().fg(Color::DarkGray)),
            |binary| (format!("   {binary}"), Style::default().fg(Color::Cyan)),
        );
        binary_cells.push(Cell::from(label).style(style));
    }
    let binary_row = Row::new(binary_cells).height(1);

    let cl_section = section_row("CL", node_count);

    let mut l2_cells = vec![Cell::from("  Unsafe L2").style(label_style)];
    for node in nodes {
        let (label, style) = node.unsafe_l2_block.map_or_else(
            || (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
            |n| (block_label(n, unsafe_l2_baseline), Style::default().fg(Color::White)),
        );
        l2_cells.push(Cell::from(label).style(style));
    }
    let l2_row = Row::new(l2_cells).height(1);

    let mut l2_hash_cells = vec![Cell::from("  Unsafe Hash").style(label_style)];
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

    let mut safe_l2_cells = vec![Cell::from("  Safe L2").style(label_style)];
    for node in nodes {
        let (label, style) = node.safe_l2_block.map_or_else(
            || (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
            |n| (block_label(n, safe_l2_baseline), Style::default().fg(Color::White)),
        );
        safe_l2_cells.push(Cell::from(label).style(style));
    }
    let safe_l2_row = Row::new(safe_l2_cells).height(1);

    let mut safe_hash_cells = vec![Cell::from("  Safe Hash").style(label_style)];
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

    let mut finalized_l2_cells = vec![Cell::from("  Finalized L2").style(label_style)];
    for node in nodes {
        let (label, style) = node.finalized_l2_block.map_or_else(
            || (Line::from("   ?"), Style::default().fg(Color::DarkGray)),
            |n| (block_label(n, finalized_l2_baseline), Style::default().fg(Color::White)),
        );
        finalized_l2_cells.push(Cell::from(label).style(style));
    }
    let finalized_l2_row = Row::new(finalized_l2_cells).height(1);

    let mut l1_cells = vec![Cell::from("  L1 Derived").style(label_style)];
    for node in nodes {
        let (label, style) = match (node.current_l1_block, node.head_l1_block) {
            (Some(cur), Some(head)) => {
                let lag = head.saturating_sub(cur);
                let color = if lag > 10 { Color::Yellow } else { Color::Green };
                (format!("   {cur} / {head}"), Style::default().fg(color))
            }
            _ => ("   ? / ?".to_string(), Style::default().fg(Color::DarkGray)),
        };
        l1_cells.push(Cell::from(label).style(style));
    }
    let l1_row = Row::new(l1_cells).height(1);

    let mut cl_peers_cells = vec![Cell::from("  CL Peers").style(label_style)];
    for node in nodes {
        let (label, style) = match node.cl_peer_count {
            Some(0) => ("   0".to_string(), Style::default().fg(Color::Red)),
            Some(n) => (format!("   {n}"), Style::default().fg(Color::Green)),
            None => ("   ?".to_string(), Style::default().fg(Color::Red)),
        };
        cl_peers_cells.push(Cell::from(label).style(style));
    }
    let cl_peers_row = Row::new(cl_peers_cells).height(1);

    let el_section = section_row("EL", node_count);

    let mut el_block_cells = vec![Cell::from("  Block").style(label_style)];
    for node in nodes {
        let (label, style) = node.el_block.map_or_else(
            || (Line::from("   -"), Style::default().fg(Color::DarkGray)),
            |n| (block_label(n, el_block_baseline), Style::default().fg(Color::White)),
        );
        el_block_cells.push(Cell::from(label).style(style));
    }
    let el_block_row = Row::new(el_block_cells).height(1);

    let mut el_syncing_cells = vec![Cell::from("  Syncing").style(label_style)];
    for node in nodes {
        let (label, style) = match node.el_syncing {
            Some(true) => ("   yes", Style::default().fg(Color::Yellow)),
            Some(false) => ("   no", Style::default().fg(Color::Green)),
            None => ("   -", Style::default().fg(Color::DarkGray)),
        };
        el_syncing_cells.push(Cell::from(label).style(style));
    }
    let el_syncing_row = Row::new(el_syncing_cells).height(1);

    let mut el_peers_cells = vec![Cell::from("  EL Peers").style(label_style)];
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
        spacer.clone(),
        cl_section,
        l2_row,
        l2_hash_row,
        safe_l2_row,
        safe_hash_row,
        finalized_l2_row,
        l1_row,
        cl_peers_row,
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
    use crate::{
        app::views::{
            GateOverlay,
            conductor_test_utils::{draw, fg_of, gateway_resources, key, rows},
        },
        gate_cluster_fixtures::{CLUSTER_HEALTHY, CLUSTER_PARTIAL, CLUSTER_WITH_CL},
    };

    const SAMPLE_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn conductor_status(
        name: &str,
        is_leader: Option<bool>,
        unsafe_l2_block: Option<u64>,
    ) -> ConductorNodeStatus {
        ConductorNodeStatus {
            name: name.to_string(),
            is_leader,
            conductor_active: None,
            conductor_paused: None,
            conductor_stopped: None,
            sequencer_healthy: None,
            sequencer_active: None,
            unsafe_l2_block,
            unsafe_l2_hash: None,
            safe_l2_block: None,
            safe_l2_hash: None,
            finalized_l2_block: None,
            current_l1_block: None,
            head_l1_block: None,
            cl_peer_count: None,
            el_block: None,
            el_syncing: None,
            el_peer_count: None,
            suffrage: None,
            discovered: true,
        }
    }

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

    #[test]
    fn block_label_formats_signed_delta_from_baseline() {
        assert_eq!(block_label(7, Some(10)).to_string(), "   7 (-3)");
        assert_eq!(block_label(10, Some(10)).to_string(), "   10");
        assert_eq!(block_label(12, Some(10)).to_string(), "   12 (+2)");
        assert_eq!(block_label(7, None).to_string(), "   7");
    }

    #[test]
    fn block_label_colors_delta_red_when_more_than_100_blocks_behind() {
        let stale = block_label(899, Some(1000));
        assert_eq!(stale.to_string(), "   899 (-101)");
        assert_eq!(stale.spans[1].style.fg, Some(Color::Red));

        let boundary = block_label(900, Some(1000));
        assert_eq!(boundary.to_string(), "   900 (-100)");
        assert_eq!(boundary.spans[1].style.fg, None);
    }

    #[test]
    fn latest_block_uses_max_reported_block() {
        let nodes = vec![
            conductor_status("sequencer-0", Some(true), Some(10)),
            conductor_status("sequencer-1", Some(false), Some(7)),
            conductor_status("sequencer-2", Some(false), Some(12)),
        ];

        let baseline = latest_block(&nodes, |node| node.unsafe_l2_block);

        assert_eq!(baseline, Some(12));
    }

    #[test]
    fn latest_block_ignores_unknown_blocks() {
        let nodes = vec![
            conductor_status("sequencer-0", Some(false), None),
            conductor_status("sequencer-1", Some(false), Some(7)),
            conductor_status("sequencer-2", None, Some(12)),
        ];

        let baseline = latest_block(&nodes, |node| node.unsafe_l2_block);

        assert_eq!(baseline, Some(12));
    }

    #[tokio::test]
    async fn menu_blocks_the_same_items_with_the_same_toasts_in_each_mode() {
        let mut node = conductor_status("node-1", Some(false), None);
        node.conductor_paused = Some(false);
        node.sequencer_active = Some(true);
        node.discovered = false;
        let direct = ConductorView::NEEDS_DIRECT_ACCESS;
        let target = ConductorView::NEEDS_TARGET;
        // Items not listed for a mode are allowed; `None` is disabled for
        // this node (no toast).
        // Items not listed for a mode are allowed. Each entry is (item, toast,
        // closes the menu, greyed out).
        let disabled = (None, false, true);
        let needs_direct = (Some(direct), false, true);
        let blocked = [
            (
                MenuMode::Direct,
                vec![
                    (ActionMenuItem::ConductorResume, disabled),
                    (ActionMenuItem::StartSequencer, disabled),
                ],
            ),
            (
                MenuMode::DirectWithGate,
                vec![
                    (ActionMenuItem::TransferLeaderAny, (Some(target), true, false)),
                    (ActionMenuItem::ConductorResume, disabled),
                    (ActionMenuItem::StartSequencer, disabled),
                ],
            ),
            (
                MenuMode::Gateway,
                vec![
                    (ActionMenuItem::TransferLeaderAny, (Some(target), true, true)),
                    (ActionMenuItem::ConductorPause, needs_direct),
                    (ActionMenuItem::ConductorResume, needs_direct),
                    (ActionMenuItem::StartSequencer, needs_direct),
                    (ActionMenuItem::StopSequencer, needs_direct),
                    (ActionMenuItem::P2PToggle, needs_direct),
                    (ActionMenuItem::RestartContainers, needs_direct),
                ],
            ),
        ];

        for (mode, blocked) in blocked {
            for (index, item) in MENU_ITEMS.iter().enumerate() {
                let expected = blocked.iter().find(|(blocked, _)| blocked == item).map_or(
                    MenuAvailability::Allowed,
                    |(_, (toast, close_menu, greyed))| MenuAvailability::Blocked {
                        toast: *toast,
                        close_menu: *close_menu,
                        greyed: *greyed,
                    },
                );
                assert_eq!(item.available(&node, false, mode), expected, "{mode:?} {item:?}");

                // Enter on the item shows exactly that toast.
                let mut resources = match mode {
                    MenuMode::Gateway => gateway_resources(None),
                    MenuMode::DirectWithGate => {
                        let mut config = crate::MonitoringConfig::mainnet();
                        config.gate_url = Some(url::Url::parse("https://gate.example").unwrap());
                        Resources::new(config)
                    }
                    MenuMode::Direct => Resources::new(crate::MonitoringConfig::mainnet()),
                };
                resources.conductor.nodes = vec![node.clone()];
                assert_eq!(MenuMode::of(&resources), mode);
                let mut view = ConductorView::new();
                view.overlay = Overlay::ActionMenu { cursor: index };
                view.handle_key(key(KeyCode::Enter), &mut resources);
                let toast = resources.toasts.current().map(|toast| toast.message.clone());
                if let MenuAvailability::Blocked { toast: expected_toast, close_menu, .. } =
                    expected
                {
                    assert_eq!(toast.as_deref(), expected_toast, "{mode:?} {item:?}");
                    let menu_open = matches!(view.overlay, Overlay::ActionMenu { .. });
                    assert_eq!(menu_open, !close_menu, "{mode:?} {item:?}");
                } else {
                    assert_eq!(toast, None, "{mode:?} {item:?}");
                }
            }
        }
    }

    #[test]
    fn transfer_here_opens_proposal_form_in_gateway_mode() {
        let node = conductor_status("node-1", Some(false), None);
        let mut view = ConductorView::new();

        view.select_menu_item(
            ActionMenuItem::TransferLeaderHere,
            &node,
            false,
            MenuMode::DirectWithGate,
        );
        assert!(matches!(
            view.gate.overlay(),
            GateOverlay::ProposalForm { target, reason } if target == "node-1" && reason.is_empty()
        ));

        view.select_menu_item(ActionMenuItem::TransferLeaderHere, &node, false, MenuMode::Direct);
        assert!(matches!(
            &view.overlay,
            Overlay::Confirm { action: PendingAction::TransferTo(target), .. } if target == "node-1"
        ));
    }

    #[tokio::test]
    async fn gateway_mode_renders_the_gateway_cluster_view() {
        let resources = gateway_resources(Some(CLUSTER_HEALTHY));
        let buffer = draw(&mut ConductorView::new(), &resources);
        let rows = rows(&buffer);
        let screen = rows.join("\n");

        assert!(rows[0].contains("HA Conductor · via approval gateway"), "{screen}");
        assert!(rows[1].contains("control loop: ALL ACTIVE (2/2)"), "{screen}");
        assert!(!screen.contains("Pause all"), "{screen}");
        assert!(
            rows[2].contains("base-mainnet  ·  membership v7  ·  as of 2026-11-10T12:00:00+00:00")
        );
        for shown in [
            "★  LEADER",
            "follower",
            "Seq active",
            "Unsafe L2",
            "1204310",
            "Safe L2",
            "1204250",
            "Finalized L2",
            "1203900",
        ] {
            assert!(screen.contains(shown), "{shown} missing:\n{screen}");
        }
        for hidden in ["── CL", "── EL", "EL Peers", "Safe Hash", "Syncing", "  Block "] {
            assert!(!screen.contains(hidden), "{hidden} shown:\n{screen}");
        }
        let error_row = rows.iter().find(|row| row.contains("  Error")).unwrap();
        assert_eq!(error_row.matches(" - ").count(), 2, "{error_row}");
        assert!(rows[33].contains("[t] propose transfer") && rows[33].contains("[p] proposals"));
    }

    #[tokio::test]
    async fn gateway_mode_shows_the_cl_rows_like_the_direct_view() {
        let row_of = |rows: &[String], label: &str| {
            rows.iter().find(|row| row.contains(label)).cloned().unwrap_or_default()
        };

        let resources = gateway_resources(Some(CLUSTER_WITH_CL));
        let buffer = draw(&mut ConductorView::new(), &resources);
        let shown = rows(&buffer);
        let screen = shown.join("\n");
        let hash = row_of(&shown, "  Unsafe Hash");
        assert!(hash.contains("   0xabcdef01…") && hash.contains("   ?"), "{screen}");
        let l1 = row_of(&shown, "  L1 Derived");
        assert!(l1.contains("   21000000 / 21000004") && l1.contains("   ? / ?"), "{screen}");
        let peers = row_of(&shown, "  CL Peers");
        assert!(peers.contains("   12") && peers.contains("   ?"), "{screen}");
        assert_eq!(fg_of(&buffer, "0xabcdef01…"), Color::Yellow);
        assert_eq!(fg_of(&buffer, "21000000 / 21000004"), Color::Green);
        assert_eq!(fg_of(&buffer, "? / ?"), Color::DarkGray);
        assert_eq!(fg_of(&buffer, "   12 "), Color::Green);

        // An older gateway without the fields shows the direct view's placeholders.
        let resources = gateway_resources(Some(CLUSTER_HEALTHY));
        let older = rows(&draw(&mut ConductorView::new(), &resources));
        assert_eq!(row_of(&older, "  Unsafe Hash").matches("   ?").count(), 2);
        assert_eq!(row_of(&older, "  L1 Derived").matches("   ? / ?").count(), 2);
        assert_eq!(row_of(&older, "  CL Peers").matches("   ?").count(), 2);
    }

    #[tokio::test]
    async fn gateway_mode_shows_a_node_error_in_the_error_style() {
        let resources = gateway_resources(Some(CLUSTER_PARTIAL));
        let buffer = draw(&mut ConductorView::new(), &resources);
        let screen = rows(&buffer).join("\n");

        assert!(screen.contains("via approval gateway"), "{screen}");
        assert_eq!(fg_of(&buffer, "conductor rpc: context deadline exceeded"), Color::Red);
        assert_eq!(fg_of(&buffer, "offline"), Color::Red);
        assert!(!screen.contains("EL Peers"), "{screen}");
    }

    #[tokio::test]
    async fn long_node_error_is_ellipsized_in_the_table_and_full_in_the_actions_panel() {
        let full = "admin_sequencerActive: unreachable: dial tcp 10.0.0.12:7545: connect: connection refused after 3 attempts";
        let fixture = CLUSTER_PARTIAL.replace("conductor rpc: context deadline exceeded", full);
        let resources = gateway_resources(Some(&fixture));
        let mut view = ConductorView::new();

        let buffer = draw(&mut view, &resources);
        let table_rows = rows(&buffer);
        let error_row = table_rows.iter().find(|row| row.contains("  Error")).unwrap();
        let start = error_row.find("admin_sequencerActive").unwrap();
        let shown = error_row[start..].split('…').next().unwrap();
        assert!(error_row[start..].contains('…'), "{error_row}");
        assert!(full.starts_with(shown) && shown.len() < full.len(), "{error_row}");
        assert_eq!(fg_of(&buffer, "admin_sequencerActive"), Color::Red);
        assert_eq!(fg_of(&buffer, "…"), Color::Red);

        view.selected = 1;
        view.overlay = Overlay::ActionMenu { cursor: 0 };
        let buffer = draw(&mut view, &resources);
        let rows = rows(&buffer);
        let (first, row) =
            rows.iter().enumerate().find(|(_, row)| row.contains("Error: admin_")).unwrap();
        let left = row[..row.find("Error: admin_").unwrap()].chars().count();
        let panel_text = rows[first..]
            .iter()
            .map(|row| row.chars().skip(left).take_while(|c| *c != '│').collect::<String>())
            .map(|text| text.trim().to_string())
            .take_while(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(panel_text, format!("Error: {full}"), "{}", rows.join("\n"));
        assert_eq!(fg_of(&buffer, "Error: admin_"), Color::Red);
    }

    #[tokio::test]
    async fn gateway_node_names_render_escaped() {
        let fixture =
            CLUSTER_HEALTHY.replace(r#""id": "sequencer-1""#, r#""id": "evil\u001b[2Jx""#);
        let resources = gateway_resources(Some(&fixture));
        let mut view = ConductorView::new();
        view.selected = 1;
        view.overlay = Overlay::ActionMenu { cursor: 0 };
        let buffer = draw(&mut view, &resources);
        let rows = rows(&buffer);
        let screen = rows.join("\n");

        assert!(!screen.contains('\u{1b}'), "{screen}");
        assert!(rows.iter().any(|row| row.contains(" Actions: evil\\u{1b}[2Jx ")), "{screen}");
        assert!(
            rows.iter().any(|row| row.contains("evil\\u{1b}[2Jx") && !row.contains("Actions:")),
            "{screen}"
        );
    }

    #[tokio::test]
    async fn gateway_mode_enables_transfer_here_from_gateway_data() {
        let resources = gateway_resources(Some(CLUSTER_HEALTHY));
        let mut view = ConductorView::new();
        view.selected = 1;
        view.overlay = Overlay::ActionMenu { cursor: 0 };
        let buffer = draw(&mut view, &resources);

        assert_eq!(fg_of(&buffer, "Transfer leader here"), Color::White);
        assert_eq!(fg_of(&buffer, "Conductor pause"), Color::DarkGray);
        assert_eq!(fg_of(&buffer, "Restart containers"), Color::DarkGray);

        // The erroring node's role is unknown, so it can't be a target.
        let resources = gateway_resources(Some(CLUSTER_PARTIAL));
        let buffer = draw(&mut view, &resources);
        assert_eq!(fg_of(&buffer, "Transfer leader here"), Color::DarkGray);
    }

    #[tokio::test]
    async fn t_explains_why_it_cannot_transfer() {
        let mut resources = gateway_resources(None);
        let mut view = ConductorView::new();
        view.handle_key(key(KeyCode::Char('t')), &mut resources);
        assert!(matches!(view.overlay, Overlay::None));
        assert_eq!(resources.toasts.current().unwrap().message, ConductorView::NO_GATEWAY_NODES);

        let mut resources = Resources::new(crate::MonitoringConfig::mainnet());
        view.handle_key(key(KeyCode::Char('t')), &mut resources);
        assert!(matches!(view.overlay, Overlay::None));
        assert_eq!(resources.toasts.current().unwrap().message, ConductorView::NO_CONDUCTOR_CONFIG);
    }

    #[tokio::test]
    async fn t_with_a_gateway_proposes_only_to_a_known_follower() {
        for mode in [MenuMode::Gateway, MenuMode::DirectWithGate] {
            for (is_leader, opens_form) in [(Some(true), false), (None, false), (Some(false), true)]
            {
                let mut resources = if mode == MenuMode::Gateway {
                    gateway_resources(None)
                } else {
                    let mut config = crate::MonitoringConfig::mainnet();
                    config.gate_url = Some(url::Url::parse("https://gate.example").unwrap());
                    Resources::new(config)
                };
                resources.conductor.nodes = vec![conductor_status("node-1", is_leader, None)];
                assert_eq!(MenuMode::of(&resources), mode);
                let mut view = ConductorView::new();

                view.handle_key(key(KeyCode::Char('t')), &mut resources);

                let toast = resources.toasts.current().map(|toast| toast.message.clone());
                assert!(matches!(view.overlay, Overlay::None), "{mode:?} {is_leader:?}");
                if opens_form {
                    assert!(
                        matches!(
                            view.gate.overlay(),
                            GateOverlay::ProposalForm { target, .. } if target == "node-1"
                        ),
                        "{mode:?} {is_leader:?}"
                    );
                    assert_eq!(toast, None, "{mode:?} {is_leader:?}");
                } else {
                    assert!(
                        matches!(view.gate.overlay(), GateOverlay::None),
                        "{mode:?} {is_leader:?}"
                    );
                    assert_eq!(
                        toast.as_deref(),
                        Some(ConductorView::NEEDS_FOLLOWER),
                        "{mode:?} {is_leader:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn t_without_a_gateway_confirms_a_transfer_to_any_peer_whatever_is_selected() {
        for is_leader in [Some(true), None, Some(false)] {
            let mut resources = Resources::new(crate::MonitoringConfig::mainnet());
            resources.conductor.set_nodes_config(vec![crate::ConductorNodeConfig {
                name: "node-1".to_string(),
                conductor_rpc: url::Url::parse("http://127.0.0.1:6545").unwrap(),
                cl_rpc: url::Url::parse("http://127.0.0.1:7545").unwrap(),
                server_id: "node-1".to_string(),
                raft_addr: "node-1:5050".to_string(),
                el_rpc: None,
                docker_conductor: None,
                docker_el: None,
                docker_cl: None,
            }]);
            resources.conductor.nodes = vec![conductor_status("node-1", is_leader, None)];
            assert_eq!(MenuMode::of(&resources), MenuMode::Direct);
            let mut view = ConductorView::new();

            view.handle_key(key(KeyCode::Char('t')), &mut resources);

            assert!(
                matches!(view.overlay, Overlay::Confirm { action: PendingAction::TransferAny, .. }),
                "{is_leader:?}"
            );
            assert!(matches!(view.gate.overlay(), GateOverlay::None), "{is_leader:?}");
            assert!(resources.toasts.current().is_none(), "{is_leader:?}");
        }
    }

    #[tokio::test]
    async fn gateway_mode_refuses_direct_rpc_actions_with_a_message() {
        let mut resources = gateway_resources(Some(CLUSTER_HEALTHY));
        let mut view = ConductorView::new();

        view.handle_key(key(KeyCode::Char('P')), &mut resources);

        assert!(matches!(view.overlay, Overlay::None));
        assert_eq!(resources.toasts.current().unwrap().message, ConductorView::NEEDS_DIRECT_ACCESS);
    }
}
