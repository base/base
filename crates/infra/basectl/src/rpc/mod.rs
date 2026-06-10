//! RPC clients, pollers, and data types used by basectl.

mod admin;
pub use admin::{
    pause_sequencer_node, start_sequencer_node, stop_sequencer_node, unpause_sequencer_node,
};

mod conductor;
pub use conductor::{
    ConductorNodeStatus, ConductorPollUpdate, PausedPeers, conductor_pause_all_nodes,
    conductor_pause_node, conductor_resume_all_nodes, conductor_resume_node,
    restart_conductor_node, run_conductor_poller, transfer_conductor_leader,
};

mod el;
pub use el::{
    BacklogBlock, BacklogFetchResult, BacklogProgress, BlockDaInfo, InitialBacklog, TxSummary,
    decode_flashblock_transactions, fetch_block, fetch_block_transactions,
    fetch_initial_backlog_with_progress, fetch_l2_block_number, fetch_l2_chain_id,
    run_block_fetcher,
};

mod flashblocks;
pub use flashblocks::{TimestampedFlashblock, run_flashblock_ws, run_flashblock_ws_timestamped};

mod l1;
pub use l1::{
    L1BlockInfo, L1ConnectionMode, fetch_full_system_config, fetch_l1_block_number,
    run_l1_blob_watcher,
};

mod p2p;
pub use p2p::{
    ClInfoReport, DiscoveryInfo, ElInfoReport, NodeEndpoint, NodeInfoReport, PeerListReport,
    PeerStatsReport, PeerSummary, RawInfoReport, RawPeerCounts, RawPeersReport, add_peer,
    connect_peer, disconnect_peer, fetch_cl_info, fetch_connected_peers, fetch_el_info, fetch_info,
    fetch_raw_info, fetch_raw_peers, remove_peer,
};

mod pods;
pub use pods::{PodGroupStatus, PodStatus, PodsPoller, PodsSnapshot, run_pods_poller};

mod rollup;
pub use rollup::{
    LatestProposal, ProofsSnapshot, SyncStatusReport, ValidatorNodeStatus, fetch_safe_and_latest,
    fetch_sync_status, run_proofs_poller, run_safe_head_poller, run_validator_poller,
};
