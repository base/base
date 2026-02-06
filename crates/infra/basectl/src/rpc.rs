use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use anyhow::Result;

use crate::{config::ChainConfig, l1_client::fetch_system_config_params};

const DEFAULT_ELASTICITY: u64 = 6;

/// Chain parameters needed for flashblocks display
#[derive(Debug, Clone, Copy)]
pub struct ChainParams {
    pub gas_limit: u64,
    pub elasticity: u64,
}

/// Fetch chain parameters, trying L1 first and falling back to L2.
///
/// This fetches `gas_limit` and elasticity from the L1 `SystemConfig` contract.
/// If elasticity is not available on L1 (older `SystemConfig`), it falls back
/// to fetching from L2 `extraData`.
pub async fn fetch_chain_params(config: &ChainConfig) -> Result<ChainParams> {
    let l1_params =
        fetch_system_config_params(config.l1_rpc.as_str(), config.system_config).await?;

    let elasticity = match l1_params.elasticity {
        Some(e) => e,
        None => fetch_elasticity(config.rpc.as_str()).await?,
    };

    Ok(ChainParams { gas_limit: l1_params.gas_limit, elasticity })
}

/// Fetch the EIP-1559 elasticity multiplier from the L2 block extraData.
/// Falls back to default (6) if extraData is not in Holocene format.
pub async fn fetch_elasticity(rpc_url: &str) -> Result<u64> {
    let provider = ProviderBuilder::new().connect(rpc_url).await?;

    let block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No block found"))?;

    let extra_data = &block.header.extra_data;

    // Holocene format: version(1) + denominator(4) + elasticity(4) = 9 bytes
    if extra_data.len() >= 9 && extra_data[0] == 0 {
        let elasticity =
            u32::from_be_bytes([extra_data[5], extra_data[6], extra_data[7], extra_data[8]]);
        Ok(elasticity as u64)
    } else {
        // Pre-Holocene or invalid format, use default
        Ok(DEFAULT_ELASTICITY)
    }
}
