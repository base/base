use alloy_primitives::{B256, U256};
use base_enclave::{
    BlockId, ChainConfig, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig,
};

use crate::TeeProverError;

/// Configuration building utilities for TEE proving.
#[derive(Debug)]
pub struct ConfigBuilder;

impl ConfigBuilder {
    /// Builds the [`ChainConfig`] envelope expected by enclave RPC.
    ///
    /// The `name` parameter distinguishes callers (e.g. `"base-proposer"` vs
    /// `"base-challenger"`) and is the only field that varies.
    pub fn build_chain_config(rollup_config: &RollupConfig, name: &str) -> ChainConfig {
        let mut config = ChainConfig {
            name: name.to_string(),
            l1_chain_id: rollup_config.l1_chain_id,
            public_rpc: String::new(),
            sequencer_rpc: String::new(),
            explorer: String::new(),
            data_availability_type: "eth-da".to_string(),
            chain_id: rollup_config.l2_chain_id.id(),
            batch_inbox_addr: rollup_config.batch_inbox_address,
            block_time: rollup_config.block_time,
            seq_window_size: rollup_config.seq_window_size,
            max_sequencer_drift: rollup_config.max_sequencer_drift,
            gas_paying_token: None,
            protocol_versions_addr: Some(rollup_config.protocol_versions_address),
            hardfork_config: rollup_config.hardforks,
            optimism: Some(rollup_config.chain_op_config),
            genesis: rollup_config.genesis,
            roles: None,
            addresses: Some(Default::default()),
        };

        // These address fields are required by ChainConfig but are not consumed by
        // the enclave — it only reads deposit_contract_address and
        // l1_system_config_address from the PerChainConfig. The address_manager value
        // is mapped to protocol_versions_address solely to satisfy the struct; the
        // enclave discards it.
        if let Some(addresses) = config.addresses.as_mut() {
            addresses.address_manager = Some(rollup_config.protocol_versions_address);
            addresses.optimism_portal_proxy = Some(rollup_config.deposit_contract_address);
            addresses.system_config_proxy = Some(rollup_config.l1_system_config_address);
        }

        config
    }

    /// Converts a [`RollupConfig`] to a [`PerChainConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if the genesis `system_config` is missing.
    pub fn rollup_config_to_per_chain_config(
        cfg: &RollupConfig,
    ) -> Result<PerChainConfig, TeeProverError> {
        let sc = cfg.genesis.system_config.as_ref().ok_or_else(|| {
            TeeProverError::Config("rollup config missing genesis system_config".into())
        })?;
        let batcher_addr = sc.batcher_address;
        let scalar = B256::from(sc.scalar.to_be_bytes::<32>());
        let gas_limit = sc.gas_limit;

        Ok(PerChainConfig {
            chain_id: U256::from(cfg.l2_chain_id.id()),
            genesis: Genesis {
                l1: BlockId { hash: cfg.genesis.l1.hash, number: cfg.genesis.l1.number },
                l2: BlockId { hash: cfg.genesis.l2.hash, number: cfg.genesis.l2.number },
                l2_time: cfg.genesis.l2_time,
                system_config: GenesisSystemConfig {
                    batcher_addr,
                    overhead: B256::ZERO,
                    scalar,
                    gas_limit,
                },
            },
            block_time: cfg.block_time,
            deposit_contract_address: cfg.deposit_contract_address,
            l1_system_config_address: cfg.l1_system_config_address,
        })
    }

    /// Computes the canonical config hash from a [`RollupConfig`].
    ///
    /// Builds a [`PerChainConfig`], applies
    /// [`force_defaults()`](PerChainConfig::force_defaults) for deterministic
    /// values, then returns the keccak256 hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the genesis `system_config` is missing.
    pub fn compute_config_hash(rollup_config: &RollupConfig) -> Result<B256, TeeProverError> {
        let mut per_chain = Self::rollup_config_to_per_chain_config(rollup_config)?;
        per_chain.force_defaults();
        Ok(per_chain.hash())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_enclave::default_rollup_config;

    use super::*;

    #[test]
    fn test_build_chain_config_maps_rollup_fields() {
        let mut rollup = default_rollup_config();
        rollup.batch_inbox_address = address!("1111111111111111111111111111111111111111");
        rollup.protocol_versions_address = address!("2222222222222222222222222222222222222222");
        rollup.deposit_contract_address = address!("3333333333333333333333333333333333333333");
        rollup.l1_system_config_address = address!("4444444444444444444444444444444444444444");
        rollup.block_time = 2;
        rollup.l1_chain_id = 1;

        let config = ConfigBuilder::build_chain_config(&rollup, "test-name");

        assert_eq!(config.name, "test-name");
        assert_eq!(config.batch_inbox_addr, rollup.batch_inbox_address);
        assert_eq!(config.block_time, 2);
        assert_eq!(config.l1_chain_id, 1);
        assert_eq!(config.protocol_versions_addr, Some(rollup.protocol_versions_address));

        let addresses = config.addresses.expect("addresses should be populated");
        assert_eq!(addresses.address_manager, Some(rollup.protocol_versions_address));
        assert_eq!(addresses.optimism_portal_proxy, Some(rollup.deposit_contract_address));
        assert_eq!(addresses.system_config_proxy, Some(rollup.l1_system_config_address));
    }

    #[test]
    fn test_compute_config_hash_succeeds_with_valid_config() {
        let rollup = default_rollup_config();
        let result = ConfigBuilder::compute_config_hash(&rollup);
        assert!(result.is_ok());
    }

    #[test]
    fn test_rollup_config_to_per_chain_config_missing_system_config() {
        let mut rollup = default_rollup_config();
        rollup.genesis.system_config = None;

        let result = ConfigBuilder::rollup_config_to_per_chain_config(&rollup);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("system_config"));
    }
}
