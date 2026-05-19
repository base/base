//! Canonical derivation-side construction of hardfork upgrade transactions.
//!
//! Centralises the `parent_timestamp -> next_timestamp` fork-boundary logic so
//! both the derivation pipeline and the harness sequencer produce the same
//! set of system deposits at each hardfork transition.

use alloc::vec::Vec;

use alloy_primitives::Bytes;
use base_common_genesis::RollupConfig;
use base_consensus_upgrades::{Upgrade, Upgrades};

/// Canonical derivation-side construction of hardfork upgrade transactions.
#[derive(Debug, Default, Clone, Copy)]
pub struct UpgradeTransactions;

impl UpgradeTransactions {
    /// Returns the encoded hardfork upgrade transactions that must execute in
    /// the first L2 block whose timestamp enters a new hardfork regime.
    pub fn for_transition(
        rollup_config: &RollupConfig,
        parent_timestamp: u64,
        next_timestamp: u64,
    ) -> Vec<Bytes> {
        let mut upgrade_transactions = Vec::new();

        if rollup_config.is_ecotone_active(next_timestamp)
            && !rollup_config.is_ecotone_active(parent_timestamp)
        {
            upgrade_transactions.extend(Upgrades::ECOTONE.txs());
        }
        if rollup_config.is_fjord_active(next_timestamp)
            && !rollup_config.is_fjord_active(parent_timestamp)
        {
            upgrade_transactions.extend(Upgrades::FJORD.txs());
        }
        if rollup_config.is_isthmus_active(next_timestamp)
            && !rollup_config.is_isthmus_active(parent_timestamp)
        {
            upgrade_transactions.extend(Upgrades::ISTHMUS.txs());
        }
        if rollup_config.is_jovian_active(next_timestamp)
            && !rollup_config.is_jovian_active(parent_timestamp)
        {
            upgrade_transactions.extend(Upgrades::JOVIAN.txs());
        }
        if rollup_config.is_base_azul_active(next_timestamp)
            && !rollup_config.is_base_azul_active(parent_timestamp)
        {
            upgrade_transactions.extend(Upgrades::BASE_V1.txs());
        }

        upgrade_transactions
    }
}
