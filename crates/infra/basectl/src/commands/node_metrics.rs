//! Shared per-node metrics projected from polled conductor node status.

use serde::Serialize;

use crate::ConductorNodeStatus;

/// Node metrics shared by conductor and sequencer JSON output, flattened into
/// each node entry so both commands report identical field names and shapes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetricsJson {
    /// Whether this node is leader.
    pub is_leader: Option<bool>,
    /// Whether sequencing is active.
    pub sequencer_active: Option<bool>,
    /// Whether the sequencer is healthy.
    pub sequencer_healthy: Option<bool>,
    /// Whether the conductor is paused.
    pub conductor_paused: Option<bool>,
    /// Unsafe L2 block number.
    pub unsafe_l2_block: Option<u64>,
    /// Unsafe L2 block hash.
    pub unsafe_l2_hash: Option<String>,
    /// Safe L2 block number.
    pub safe_l2_block: Option<u64>,
    /// Safe L2 block hash.
    pub safe_l2_hash: Option<String>,
    /// Finalized L2 block number.
    pub finalized_l2_block: Option<u64>,
    /// Current L1 block number.
    pub current_l1_block: Option<u64>,
    /// Head L1 block number.
    pub head_l1_block: Option<u64>,
    /// Consensus-layer peer count.
    pub cl_peer_count: Option<u32>,
    /// Execution-layer block number.
    pub el_block: Option<u64>,
    /// Whether the execution layer is syncing.
    pub el_syncing: Option<bool>,
    /// Execution-layer peer count.
    pub el_peer_count: Option<u32>,
}

impl NodeMetricsJson {
    /// Projects the shared metric fields from an optionally polled node status.
    pub fn from_status(status: Option<&ConductorNodeStatus>) -> Self {
        Self {
            is_leader: status.and_then(|status| status.is_leader),
            sequencer_active: status.and_then(|status| status.sequencer_active),
            sequencer_healthy: status.and_then(|status| status.sequencer_healthy),
            conductor_paused: status.and_then(|status| status.conductor_paused),
            unsafe_l2_block: status.and_then(|status| status.unsafe_l2_block),
            unsafe_l2_hash: status
                .and_then(|status| status.unsafe_l2_hash)
                .map(|hash| hash.to_string()),
            safe_l2_block: status.and_then(|status| status.safe_l2_block),
            safe_l2_hash: status
                .and_then(|status| status.safe_l2_hash)
                .map(|hash| hash.to_string()),
            finalized_l2_block: status.and_then(|status| status.finalized_l2_block),
            current_l1_block: status.and_then(|status| status.current_l1_block),
            head_l1_block: status.and_then(|status| status.head_l1_block),
            cl_peer_count: status.and_then(|status| status.cl_peer_count),
            el_block: status.and_then(|status| status.el_block),
            el_syncing: status.and_then(|status| status.el_syncing),
            el_peer_count: status.and_then(|status| status.el_peer_count),
        }
    }
}
