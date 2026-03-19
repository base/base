//! Contains upgrade logging wrapper type.

use base_alloy_rpc_types_engine::OpPayloadAttributes;
use base_consensus_genesis::RollupConfig;

// TODO(refcell): Move this into a crate where it can be re-used.

/// A wrapper type that can be used to log hardfork/upgrade activation
/// when building the first block of a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeActivations;

impl UpgradeActivations {
    /// Logs hardfork activation when building the first block of a fork.
    pub fn log(config: &RollupConfig, attributes: &OpPayloadAttributes) {
        let timestamp = attributes.payload_attributes.timestamp;
        if config.is_first_ecotone_block(timestamp) {
            info!(target: "sequencer", "Sequencing ecotone upgrade block");
        } else if config.is_first_fjord_block(timestamp) {
            info!(target: "sequencer", "Sequencing fjord upgrade block");
        } else if config.is_first_granite_block(timestamp) {
            info!(target: "sequencer", "Sequencing granite upgrade block");
        } else if config.is_first_holocene_block(timestamp) {
            info!(target: "sequencer", "Sequencing holocene upgrade block");
        } else if config.is_first_isthmus_block(timestamp) {
            info!(target: "sequencer", "Sequencing isthmus upgrade block");
        } else if config.is_first_jovian_block(timestamp) {
            info!(target: "sequencer", "Sequencing jovian upgrade block");
        } else if config.is_first_base_v1_block(timestamp) {
            info!(target: "sequencer", "Sequencing base v1 upgrade block");
        }
    }
}
