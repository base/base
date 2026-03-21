//! Contains upgrade logging wrapper type.

use base_consensus_genesis::RollupConfig;

// TODO(refcell): Move this into a crate where it can be re-used.

/// A wrapper type that can be used to log hardfork/upgrade activation
/// when building the first block of a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeActivations;

impl UpgradeActivations {
    const BASE_V1_ACTIVATION_BANNER: &str = r#"######################################################
#                                                    #
#  BBBBB    AAA    SSSSS  EEEEE      V   V      111  #
#  B   B   A   A   S      E          V   V    1   1  #
#  BBBBB   AAAAA    SSS   EEEE       V   V        1  #
#  B   B   A   A       S  E           V V         1  #
#  BBBBB   A   A   SSSSS  EEEEE        V      11111  #
#                                                    #
#           ALL YOUR BASE ARE BELONG TO US           #
#                                                    #
######################################################"#;

    /// Logs hardfork activation when building or processing the first block of a fork.
    pub fn log(config: &RollupConfig, block_number: u64, timestamp: u64) {
        if config.is_first_ecotone_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating ecotone upgrade");
        } else if config.is_first_fjord_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating fjord upgrade");
        } else if config.is_first_granite_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating granite upgrade");
        } else if config.is_first_holocene_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating holocene upgrade");
        } else if config.is_first_isthmus_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating isthmus upgrade");
        } else if config.is_first_jovian_block(timestamp) {
            info!(target: "upgrades", block_number, "Activating jovian upgrade");
        } else if config.is_first_base_v1_block(timestamp) {
            for line in Self::BASE_V1_ACTIVATION_BANNER.lines() {
                info!(target: "upgrades", "{line}");
            }
            info!(target: "upgrades", block_number, "Activating base v1 upgrade");
        }
    }
}
