use std::{sync::Arc, time::Instant};

use base_common_genesis::SystemConfig;
use base_consensus_rpc::ClusterMembership;
use tokio::sync::mpsc;
use url::Url;

use crate::{
    app::{DaTracker, LoadingState},
    config::{ConductorNodeConfig, MonitoringConfig},
    rpc::{
        BacklogFetchResult, BlockDaInfo, ConductorNodeStatus, ConductorPollUpdate, L1BlockInfo,
        L1ConnectionMode, PodsSnapshot, ProofsSnapshot, ValidatorNodeStatus,
    },
    tui::ToastState,
};

/// Origin label for the conductor cluster node list, surfaced in the TUI.
#[derive(Debug, Clone, Default)]
pub enum SourceLabel {
    /// Hand-configured node list (devnet, custom YAML).
    #[default]
    Static,
    /// Bootstrapped from a single conductor RPC and refreshed from raft membership.
    Discovered {
        /// Bootstrap conductor RPC URL.
        bootstrap: Url,
        /// Wall-clock time of the most recent successful membership refresh.
        last_refresh: Instant,
    },
}

/// State for HA conductor cluster monitoring.
#[derive(Debug, Default)]
pub struct ConductorState {
    /// Most recent status snapshot for each conductor node.
    pub nodes: Vec<ConductorNodeStatus>,
    /// Original per-node configs. In `Discover` mode this is rebuilt every time
    /// the poller emits a `NodeListRefreshed` update.
    nodes_config: Vec<ConductorNodeConfig>,
    rx: Option<mpsc::Receiver<ConductorPollUpdate>>,
    /// Most recent raft cluster membership snapshot. Shared by `Arc` with the
    /// poller so a membership change is a single allocation, not a deep copy.
    pub cluster_membership: Option<Arc<ClusterMembership>>,
    /// Whether the active node list comes from a static config or live discovery.
    pub source_label: SourceLabel,
}

impl ConductorState {
    /// Sets the channel for receiving conductor poll updates.
    pub fn set_channel(&mut self, rx: mpsc::Receiver<ConductorPollUpdate>) {
        self.rx = Some(rx);
    }

    /// Sets the source label (static vs discovered) for UI display.
    pub fn set_source_label(&mut self, label: SourceLabel) {
        self.source_label = label;
    }

    /// Returns the active per-node configs. In `Static` mode this is the
    /// configured list; in `Discover` mode it is the list synthesised from the
    /// last `clusterMembership` snapshot. The conductor view uses this to
    /// dispatch mutations (pause, resume, transfer, …) without re-reading the
    /// stale `MonitoringConfig.conductors` list, which is `None` in `Discover`.
    pub fn nodes_config(&self) -> &[ConductorNodeConfig] {
        &self.nodes_config
    }

    /// Seeds the per-node configs directly (used in `Discover` mode so the view
    /// can dispatch mutations against the bootstrap node before the first
    /// `clusterMembership` snapshot arrives).
    pub fn set_nodes_config(&mut self, nodes_config: Vec<ConductorNodeConfig>) {
        self.nodes_config = nodes_config;
    }

    /// Drains all pending poll updates, keeping the most recent values.
    pub fn poll(&mut self) {
        let Some(ref mut rx) = self.rx else { return };
        while let Ok(update) = rx.try_recv() {
            match update {
                ConductorPollUpdate::Status(statuses) => self.nodes = statuses,
                ConductorPollUpdate::Membership(m) => {
                    self.cluster_membership = Some(m);
                    if let SourceLabel::Discovered { last_refresh, .. } = &mut self.source_label {
                        *last_refresh = Instant::now();
                    }
                }
                ConductorPollUpdate::NodeListRefreshed(nodes) => self.nodes_config = nodes,
            }
        }
    }

    /// Returns the safe L2 block number reported by the current Raft leader, if known.
    pub fn leader_safe_l2_block(&self) -> Option<u64> {
        self.nodes.iter().find(|n| n.is_leader == Some(true)).and_then(|n| n.safe_l2_block)
    }
}

/// State for validator node monitoring.
#[derive(Debug, Default)]
pub struct ValidatorState {
    /// Most recent status snapshot for each validator node.
    pub nodes: Vec<ValidatorNodeStatus>,
    rx: Option<mpsc::Receiver<Vec<ValidatorNodeStatus>>>,
}

impl ValidatorState {
    /// Sets the channel for receiving validator status updates.
    pub fn set_channel(&mut self, rx: mpsc::Receiver<Vec<ValidatorNodeStatus>>) {
        self.rx = Some(rx);
    }

    /// Drains the latest status snapshot from the background poller.
    pub fn poll(&mut self) {
        let Some(ref mut rx) = self.rx else { return };
        while let Ok(statuses) = rx.try_recv() {
            self.nodes = statuses;
        }
    }
}

/// State for proof system monitoring (dispute games, anchor state).
#[derive(Debug, Default)]
pub struct ProofsState {
    /// Most recent proof system snapshot.
    pub snapshot: Option<ProofsSnapshot>,
    rx: Option<mpsc::Receiver<ProofsSnapshot>>,
}

impl ProofsState {
    /// Sets the channel for receiving proof system snapshots.
    pub fn set_channel(&mut self, rx: mpsc::Receiver<ProofsSnapshot>) {
        self.rx = Some(rx);
    }

    /// Drains the latest snapshot from the background poller.
    pub fn poll(&mut self) {
        let Some(ref mut rx) = self.rx else { return };
        while let Ok(snapshot) = rx.try_recv() {
            self.snapshot = Some(snapshot);
        }
    }
}

/// State for Kubernetes pod monitoring.
#[derive(Debug, Default)]
pub struct PodsState {
    /// Most recent Kubernetes pods snapshot.
    pub snapshot: Option<PodsSnapshot>,
    rx: Option<mpsc::Receiver<PodsSnapshot>>,
}

impl PodsState {
    /// Sets the channel for receiving pods snapshots.
    pub fn set_channel(&mut self, rx: mpsc::Receiver<PodsSnapshot>) {
        self.rx = Some(rx);
    }

    /// Drains the latest pods snapshot from the background poller.
    pub fn poll(&mut self) {
        let Some(ref mut rx) = self.rx else { return };
        while let Ok(snapshot) = rx.try_recv() {
            self.snapshot = Some(snapshot);
        }
    }
}

/// Shared resources available to all TUI views.
#[derive(Debug)]
pub struct Resources {
    /// Active chain configuration.
    pub config: MonitoringConfig,
    /// Data availability monitoring state.
    pub da: DaState,
    /// Toast notification state.
    pub toasts: ToastState,
    /// HA conductor cluster monitoring state.
    pub conductor: ConductorState,
    /// Validator node monitoring state.
    pub validators: ValidatorState,
    /// Proof system monitoring state.
    pub proofs: ProofsState,
    /// Kubernetes pod monitoring state.
    pub pods: PodsState,
    /// L1 system config fetched from the contract.
    pub system_config: Option<SystemConfig>,
    sys_config_rx: Option<mpsc::Receiver<SystemConfig>>,
}

/// State for DA (data availability) monitoring.
#[derive(Debug)]
pub struct DaState {
    /// Tracks L2 block DA contributions and backlog.
    pub tracker: DaTracker,
    /// Current backlog loading progress, if still loading.
    pub loading: Option<LoadingState>,
    /// Whether the initial backlog has finished loading.
    pub loaded: bool,
    /// Current L1 connection mode (WebSocket or polling).
    pub l1_connection_mode: Option<L1ConnectionMode>,
    buffered_safe_heads: Vec<u64>,
    sync_rx: Option<mpsc::Receiver<u64>>,
    backlog_rx: Option<mpsc::Receiver<BacklogFetchResult>>,
    block_req_tx: Option<mpsc::Sender<u64>>,
    block_res_rx: Option<mpsc::Receiver<BlockDaInfo>>,
    l1_block_rx: Option<mpsc::Receiver<L1BlockInfo>>,
    l1_mode_rx: Option<mpsc::Receiver<L1ConnectionMode>>,
}

impl Resources {
    /// Creates new resources with the given chain configuration.
    pub fn new(config: MonitoringConfig) -> Self {
        Self {
            config,
            da: DaState::new(),
            toasts: ToastState::new(),
            conductor: ConductorState::default(),
            validators: ValidatorState::default(),
            proofs: ProofsState::default(),
            pods: PodsState::default(),
            system_config: None,
            sys_config_rx: None,
        }
    }

    /// Returns the configured chain name.
    pub fn chain_name(&self) -> &str {
        &self.config.name
    }

    /// Sets the channel for receiving L1 system config updates.
    pub fn set_sys_config_channel(&mut self, rx: mpsc::Receiver<SystemConfig>) {
        self.sys_config_rx = Some(rx);
    }

    /// Polls for a new system config from the background task.
    pub fn poll_sys_config(&mut self) {
        if let Some(ref mut rx) = self.sys_config_rx
            && let Ok(cfg) = rx.try_recv()
        {
            self.system_config = Some(cfg);
        }
    }
}

impl Default for DaState {
    fn default() -> Self {
        Self::new()
    }
}

impl DaState {
    /// Creates a new empty DA state.
    pub fn new() -> Self {
        Self {
            tracker: DaTracker::new(),
            loading: None,
            loaded: false,
            l1_connection_mode: None,
            buffered_safe_heads: Vec::new(),
            sync_rx: None,
            backlog_rx: None,
            block_req_tx: None,
            block_res_rx: None,
            l1_block_rx: None,
            l1_mode_rx: None,
        }
    }

    /// Sets the channels used for receiving DA monitoring data.
    pub fn set_channels(
        &mut self,
        sync_rx: mpsc::Receiver<u64>,
        backlog_rx: mpsc::Receiver<BacklogFetchResult>,
        block_req_tx: mpsc::Sender<u64>,
        block_res_rx: mpsc::Receiver<BlockDaInfo>,
        l1_block_rx: mpsc::Receiver<L1BlockInfo>,
    ) {
        self.sync_rx = Some(sync_rx);
        self.backlog_rx = Some(backlog_rx);
        self.block_req_tx = Some(block_req_tx);
        self.block_res_rx = Some(block_res_rx);
        self.l1_block_rx = Some(l1_block_rx);
    }

    /// Sets the channel for receiving L1 connection mode updates.
    pub fn set_l1_mode_channel(&mut self, rx: mpsc::Receiver<L1ConnectionMode>) {
        self.l1_mode_rx = Some(rx);
    }

    /// Advances the safe head from the conductor leader's sync status.
    ///
    /// Called each tick when a conductor cluster is configured so the DA
    /// tracker does not have to wait for sequencer-0's EL to P2P-sync
    /// new blocks produced by whichever sequencer currently holds leadership.
    pub fn apply_conductor_safe_head(&mut self, safe_block: u64) {
        if self.loaded {
            self.tracker.update_safe_head(safe_block);
        } else {
            self.buffered_safe_heads.push(safe_block);
        }
    }

    /// Drains all pending messages from background channels and updates state.
    pub fn poll(&mut self) {
        let backlog_results: Vec<_> = self
            .backlog_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for result in backlog_results {
            match result {
                BacklogFetchResult::Progress(progress) => {
                    self.loading = Some(LoadingState {
                        current_block: progress.current_block,
                        total_blocks: progress.total_blocks,
                    });
                }
                BacklogFetchResult::Block(block) => {
                    self.tracker.add_backlog_block(
                        block.block_number,
                        block.da_bytes,
                        block.timestamp,
                    );
                }
                BacklogFetchResult::Complete(initial) => {
                    self.tracker.set_initial_backlog(initial.safe_block, initial.da_bytes);
                    self.flush_buffers();
                    self.loaded = true;
                }
                BacklogFetchResult::Error => {
                    self.flush_buffers();
                    self.loaded = true;
                }
            }
        }

        let block_infos: Vec<_> = self
            .block_res_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for info in block_infos {
            self.tracker.update_block_info(info.block_number, info.da_bytes, info.timestamp);
        }

        let safe_blocks: Vec<_> = self
            .sync_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for safe_block in safe_blocks {
            if self.loaded {
                self.tracker.update_safe_head(safe_block);
            } else {
                self.buffered_safe_heads.push(safe_block);
            }
        }

        let l1_blocks: Vec<_> = self
            .l1_block_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for l1_block in l1_blocks {
            self.tracker.record_l1_block(l1_block);
        }

        if let Some(mode) = self.l1_mode_rx.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.l1_connection_mode = Some(mode);
        }
    }

    fn flush_buffers(&mut self) {
        for safe_block in std::mem::take(&mut self.buffered_safe_heads) {
            self.tracker.update_safe_head(safe_block);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::DaState;
    use crate::rpc::{BacklogFetchResult, BlockDaInfo, L1BlockInfo};

    #[test]
    fn records_l1_blocks_before_backlog_load_completes() {
        let (_sync_tx, sync_rx) = mpsc::channel(1);
        let (_backlog_tx, backlog_rx) = mpsc::channel::<BacklogFetchResult>(1);
        let (block_req_tx, _block_req_rx) = mpsc::channel::<u64>(1);
        let (_block_res_tx, block_res_rx) = mpsc::channel::<BlockDaInfo>(1);
        let (l1_block_tx, l1_block_rx) = mpsc::channel(1);

        let mut state = DaState::new();
        state.set_channels(sync_rx, backlog_rx, block_req_tx, block_res_rx, l1_block_rx);

        l1_block_tx
            .try_send(L1BlockInfo {
                block_number: 123,
                timestamp: 456,
                total_blobs: 2,
                base_blobs: 1,
            })
            .unwrap();

        state.poll();

        assert!(!state.loaded);
        assert_eq!(state.tracker.l1_blocks.len(), 1);
        assert_eq!(state.tracker.l1_blocks.front().unwrap().block_number, 123);
    }
}
