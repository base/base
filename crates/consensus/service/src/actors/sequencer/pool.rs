//! Contains a small type that identifies if the transaction pool should be enabled.

use std::sync::Arc;

use base_alloy_rpc_types_engine::OpPayloadAttributes;
use base_consensus_genesis::RollupConfig;
use base_protocol::BlockInfo;

use super::metrics::{inc_drift_empty_block, inc_recovery_mode_block};

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
        attributes: &OpPayloadAttributes,
    ) -> bool {
        if recovery_mode {
            warn!(target: "sequencer", "Sequencer is in recovery mode, producing empty block");
            inc_recovery_mode_block();
            return false;
        }

        // If the next L2 block is beyond the sequencer drift threshold, we must produce an empty
        // block.
        if attributes.payload_attributes.timestamp
            > l1_origin.timestamp + self.rollup_config.max_sequencer_drift(l1_origin.timestamp)
        {
            warn!(
                target: "sequencer",
                l2_timestamp = attributes.payload_attributes.timestamp,
                l1_timestamp = l1_origin.timestamp,
                "L2 timestamp beyond sequencer drift, producing empty block"
            );
            inc_drift_empty_block();
            return false;
        }

        // Do not include transactions in the first Ecotone block.
        if self.rollup_config.is_first_ecotone_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Do not include transactions in the first Fjord block.
        if self.rollup_config.is_first_fjord_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Do not include transactions in the first Granite block.
        if self.rollup_config.is_first_granite_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Do not include transactions in the first Holocene block.
        if self.rollup_config.is_first_holocene_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Do not include transactions in the first Isthmus block.
        if self.rollup_config.is_first_isthmus_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Do not include transactions in the first Jovian block.
        // See: `<https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/jovian/derivation.md#activation-block-rules>`
        if self.rollup_config.is_first_jovian_block(attributes.payload_attributes.timestamp) {
            return false;
        }

        // Transaction pool transactions are enabled if none of the reasons to disable are satisfied
        // above.
        true
    }
}
