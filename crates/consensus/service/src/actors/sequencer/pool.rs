//! Contains a small type that identifies if the transaction pool should be enabled.

use std::sync::Arc;

use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_protocol::BlockInfo;

use crate::Metrics;

/// The `PoolActivation` type identifies if the transaction pool should be enabled.
#[derive(Debug, Clone)]
pub struct PoolActivation {
    /// The rollup config used to determine if the transaction pool should be enabled.
    pub rollup_config: Arc<RollupConfig>,
}

impl PoolActivation {
    /// Constructs a new `PoolActivation` instance with the given rollup config.
    pub const fn new(rollup_config: Arc<RollupConfig>) -> Self {
        Self { rollup_config }
    }

    /// Determines, for the provided L1 origin block and payload attributes being constructed,
    /// if transaction pool transactions should be enabled.
    pub fn is_enabled(
        &self,
        recovery_mode: bool,
        l1_origin: BlockInfo,
        _parent_timestamp: u64,
        attributes: &BasePayloadAttributes,
    ) -> bool {
        if recovery_mode {
            warn!(target: "sequencer", "Sequencer is in recovery mode, producing empty block");
            Metrics::sequencer_recovery_mode_blocks_total().increment(1);
            return false;
        }

        // If the next L2 block is beyond the sequencer drift threshold, we must produce an empty
        // block.
        if attributes.payload_attributes.timestamp
            > l1_origin.timestamp + base_common_genesis::RollupConfig::FJORD_MAX_SEQUENCER_DRIFT
        {
            warn!(
                target: "sequencer",
                l2_timestamp = attributes.payload_attributes.timestamp,
                l1_timestamp = l1_origin.timestamp,
                "L2 timestamp beyond sequencer drift, producing empty block"
            );
            Metrics::sequencer_drift_empty_blocks_total().increment(1);
            return false;
        }

        // Transaction pool transactions are enabled if none of the reasons to disable are satisfied
        // above.
        true
    }
}
