#![doc = include_str!("../README.md")]

mod app;
pub use app::{
    Action, ActionMenuItem, App, BLOB_SIZE, BlockContribution, CommandCenterView, ConductorState,
    ConductorView, ConfigView, ConfirmButton, DaMonitorView, DaState, DaTracker,
    EVENT_POLL_TIMEOUT, FlashState, FlashblockEntry, FlashblocksView, HomeView, L1_BLOCK_WINDOW,
    L1Block, L1BlockFilter, LoadingState, MAX_HISTORY, Overlay, PendingAction, PodsState, PodsView,
    ProofsState, ProofsView, RATE_WINDOW_2M, RATE_WINDOW_5M, RATE_WINDOW_30S, RateTracker,
    Resources, Router, SourceLabel, TransactionPane, UpgradesView, ValidatorState, View, ViewId,
    create_view, run_app, run_flashblocks_json, start_background_services,
};

mod output;
pub use output::{
    COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, COLOR_GAS_FILL, COLOR_GROWTH,
    COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, COLOR_TARGET, JsonOutput, KeyValueTable,
    L1BlocksTableParams, TimestampJson, backlog_size_color, block_color, block_color_bright,
    build_gas_bar, format_bytes, format_duration, format_gas, format_gwei, format_rate,
    format_unix_timestamp, render_da_backlog_bar, render_gas_usage_bar, render_l1_blocks_table,
    target_usage_color, time_diff_color, truncate_block_number,
};

mod config;
pub use config::{
    ConductorNodeConfig, ConductorSource, DiscoveryConfig, DiscoveryPorts, MonitoringConfig,
    PodGroupConfig, PodsConfig, ProofsConfig, ValidatorNodeConfig,
};

mod doctor;
pub use doctor::{
    Doctor, DoctorCheck, DoctorInputs, DoctorOptions, DoctorReport, DoctorStatus, DoctorSummary,
    DoctorThresholds, LayerEndpointSanity, RethLimitSection, RethLimits, RethToml,
};

mod errors;
pub use errors::{
    BlockRefParseError, ConductorCommandError, DoctorArgsError, NodeLookupError, P2pCommandError,
    P2pTargetError, SequencerCommandError, StateConvergenceTimeoutError, SyncStatusCommandError,
    TxpoolCommandError,
};

mod rpc;
pub use rpc::{
    BacklogBlock, BacklogFetchResult, BacklogProgress, BaseTxpoolContent, BaseTxpoolContentFrom,
    BlockDaInfo, ClInfoReport, ConductorClusterSnapshot, ConductorControl, ConductorFanoutReport,
    ConductorNodeFailure, ConductorNodeStatus, ConductorPollUpdate, DiscoveryInfo, ElInfoReport,
    InitialBacklog, L1BlockInfo, L1ConnectionMode, LatestProposal, NodeEndpoint, NodeInfoReport,
    PausedPeers, PeerListReport, PeerStatsReport, PeerSummary, PodGroupStatus, PodStatus,
    PodsPoller, PodsSnapshot, ProofsSnapshot, RawInfoReport, RawPeerCounts, RawPeersReport,
    SEQUENCER_ACTIVE_RPC_TIMEOUT, SyncStatusReport, TimestampedFlashblock, TxSummary, TxpoolClient,
    TxpoolCounts, TxpoolReport, TxpoolScope, TxpoolSenderSummary, TxpoolTransactionPool,
    TxpoolTransactionRow, ValidatorNodeStatus, add_peer, ban_peer, conductor_pause_all_nodes,
    conductor_pause_node, conductor_resume_all_nodes, conductor_resume_node, connect_peer,
    decode_flashblock_transactions, disconnect_peer, fetch_block, fetch_block_transactions,
    fetch_cl_info, fetch_connected_peers, fetch_el_info, fetch_full_system_config, fetch_info,
    fetch_initial_backlog_with_progress, fetch_l1_block_number, fetch_l2_block_number,
    fetch_l2_chain_id, fetch_raw_info, fetch_raw_peers, fetch_safe_and_latest,
    fetch_sequencer_active, fetch_sync_status, list_banned_peers, pause_sequencer_node,
    remove_peer, restart_conductor_node, run_block_fetcher, run_conductor_poller,
    run_flashblock_ws, run_flashblock_ws_timestamped, run_l1_blob_watcher, run_pods_poller,
    run_proofs_poller, run_safe_head_poller, run_validator_poller, start_sequencer,
    start_sequencer_node, stop_sequencer, stop_sequencer_node, transfer_conductor_leader,
    unban_peer, unpause_sequencer_node,
};

mod tui;
pub use tui::{
    AppFrame, AppLayout, Keybinding, Toast, ToastLevel, ToastState, restore_terminal,
    setup_terminal,
};
