//! Enclave configuration types for TEE proof generation.

use alloy_primitives::{B256, U256};
use base_enclave::{BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig};

use crate::error::ProposerError;

/// Convert a [`RollupConfig`] (from RPC) to [`PerChainConfig`].
///
/// This is useful when you have a rollup config from an op-node RPC and need
/// to create a [`PerChainConfig`] for the prover.
///
/// # Errors
///
/// Returns an error if the deposit contract address or L1 system config address
/// cannot be parsed from the hex string.
pub fn rollup_config_to_per_chain_config(
    cfg: &RollupConfig,
) -> Result<PerChainConfig, ProposerError> {
    let deposit_contract_address = cfg.deposit_contract_address;
    let l1_system_config_address = cfg.l1_system_config_address;

    let sc = cfg.genesis.system_config.as_ref().ok_or_else(|| {
        ProposerError::Config("rollup config missing genesis system_config".into())
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
        deposit_contract_address,
        l1_system_config_address,
    })
}
