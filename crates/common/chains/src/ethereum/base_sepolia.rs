//! Base Sepolia (OP Stack L2) chain configuration for L3 settlement.
//!
//! Base Sepolia acts as the settlement layer ("L1") for L3 chains. This config provides the
//! [`alloy_genesis::ChainConfig`] needed by the prover and derivation pipeline when an L3
//! chain reports `l1_chain_id: 84532`.

use alloc::string::ToString;

use alloy_genesis::{ChainConfig, EthashConfig};
use alloy_primitives::U256;
use alloy_serde::OtherFields;

/// Base Sepolia chain configuration builder.
///
/// Unlike Ethereum L1 configs, Base Sepolia is an OP Stack L2 with no `PoW` history.
/// Pre-merge hardfork blocks are all 0 (active at genesis), and the `"optimism"` extra
/// field is set so the derivation pipeline selects `L1TxFormat::Base` (calldata-only DA,
/// OP Stack transaction envelopes).
#[derive(Debug, Clone, Copy)]
pub struct BaseSepolia;

impl BaseSepolia {
    const TTD: u128 = 0;

    /// Returns the Base Sepolia [`ChainConfig`].
    pub fn l1_config() -> ChainConfig {
        let mut extra_fields = OtherFields::default();
        extra_fields.insert("optimism".to_string(), serde_json::Value::Object(Default::default()));

        ChainConfig {
            chain_id: 84532,
            // All pre-merge hardforks active at genesis (OP Stack L2, post-merge chain).
            homestead_block: Some(0),
            dao_fork_block: Some(0),
            dao_fork_support: true,
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            muir_glacier_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            arrow_glacier_block: Some(0),
            gray_glacier_block: Some(0),
            // Post-merge timestamp-based forks: Shanghai and Cancun active at genesis.
            // Prague/Osaka not relevant — Base Sepolia headers have excess_blob_gas = 0
            // (hardcoded by OP Stack), so blob base fee is always 1 regardless of params.
            shanghai_time: Some(0),
            cancun_time: Some(0),
            prague_time: None,
            osaka_time: None,
            amsterdam_time: None,
            bogota_time: None,
            bpo1_time: None,
            bpo2_time: None,
            bpo3_time: None,
            bpo4_time: None,
            bpo5_time: None,
            ethash: Some(EthashConfig {}),
            blob_schedule: super::BlobSchedule::schedule(),
            // OP Stack L2: no PoW, TTD = 0 (matches Hoodi pattern for post-merge genesis).
            terminal_total_difficulty: Some(U256::from(Self::TTD)),
            merge_netsplit_block: None,
            // No beacon deposit contract on an OP Stack L2.
            deposit_contract_address: None,
            clique: None,
            parlia: None,
            extra_fields,
            terminal_total_difficulty_passed: false,
            _non_exhaustive: (),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::L1TxFormat;

    use super::*;

    #[test]
    fn base_sepolia_chain_id() {
        let cfg = BaseSepolia::l1_config();
        assert_eq!(cfg.chain_id, 84532);
    }

    #[test]
    fn base_sepolia_l1_tx_format_is_base() {
        let cfg = BaseSepolia::l1_config();
        assert_eq!(L1TxFormat::from_l1_config(&cfg), L1TxFormat::Base);
    }

    #[test]
    fn base_sepolia_no_beacon_deposit_contract() {
        let cfg = BaseSepolia::l1_config();
        assert!(cfg.deposit_contract_address.is_none());
    }
}
