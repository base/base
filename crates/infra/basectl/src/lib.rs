#![doc = include_str!("../README.md")]

mod app;
pub use app::{
    Action, App, CommandCenterView, ConductorState, ConductorView, ConfigView, DaMonitorView,
    DaState, FlashState, FlashblocksView, HomeView, LoadTestTask, LoadTestView, ProofsState,
    ProofsView, Resources, Router, TransactionPane, UpgradesView, ValidatorState, View, ViewId,
    create_view, run_app, run_flashblocks_json, start_background_services,
};

mod commands;
pub use commands::{
    BLOB_SIZE, BlockContribution, COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, COLOR_GAS_FILL,
    COLOR_GROWTH, COLOR_ROW_HIGHLIGHTED, COLOR_ROW_SELECTED, COLOR_TARGET, DaTracker,
    EVENT_POLL_TIMEOUT, FlashblockEntry, L1_BLOCK_WINDOW, L1Block, L1BlockFilter,
    L1BlocksTableParams, LoadingState, MAX_HISTORY, RATE_WINDOW_2M, RATE_WINDOW_5M,
    RATE_WINDOW_30S, RateTracker, backlog_size_color, block_color, block_color_bright,
    build_gas_bar, format_bytes, format_duration, format_gas, format_gwei, format_rate,
    render_da_backlog_bar, render_gas_usage_bar, render_l1_blocks_table, target_usage_color,
    time_diff_color, truncate_block_number,
};

mod config;
pub use config::{ConductorNodeConfig, MonitoringConfig, ProofsConfig, ValidatorNodeConfig};

mod l1_client;
pub use l1_client::fetch_full_system_config;

mod rpc;
pub use rpc::{
    BacklogBlock, BacklogFetchResult, BacklogProgress, BlockDaInfo, ConductorNodeStatus,
    InitialBacklog, L1BlockInfo, L1ConnectionMode, LatestProposal, PausedPeers, ProofsSnapshot,
    TimestampedFlashblock, TxSummary, ValidatorNodeStatus, decode_flashblock_transactions,
    fetch_block_transactions, fetch_initial_backlog_with_progress, fetch_safe_and_latest,
    pause_sequencer_node, restart_conductor_node, run_block_fetcher, run_conductor_poller,
    run_flashblock_ws, run_flashblock_ws_timestamped, run_l1_blob_watcher, run_proofs_poller,
    run_safe_head_poller, run_validator_poller, transfer_conductor_leader, unpause_sequencer_node,
};

mod tui;
pub use tui::{
    AppFrame, AppLayout, Keybinding, Toast, ToastLevel, ToastState, restore_terminal,
    setup_terminal,
};
