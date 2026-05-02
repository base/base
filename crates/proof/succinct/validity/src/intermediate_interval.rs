//! Resolve the canonical `INTERMEDIATE_BLOCK_INTERVAL` from on-chain.
//!
//! Single source of truth: the `AggregateVerifier` implementation registered in the
//! `DisputeGameFactory` for [`OP_SUCCINCT_VALIDITY_GAME_TYPE`]. Mirrors the read the TEE
//! proposer/challenger perform so the ZK proposer can never disagree with the contracts it
//! submits to.

use alloy_primitives::Address;
use anyhow::Result;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient,
};
use base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL;
use reqwest::Url;

/// Succinct validity dispute game type (see `Proposer` tests / `initBonds`).
pub const OP_SUCCINCT_VALIDITY_GAME_TYPE: u32 = 6;

/// Resolve the interval the proposer must use when sampling intermediate output roots.
///
/// Behaviour:
/// - `dgf_address == 0` (dev/test): warn and fall back to [`DEFAULT_INTERMEDIATE_ROOT_INTERVAL`].
///   This is the only case in which a fallback is acceptable — there is no contract to disagree
///   with.
/// - `dgf_address` set: read on-chain and return the value, or propagate the error. We refuse to
///   start with a guessed interval, because a wrong value silently produces proofs that fail
///   on-chain verification.
pub async fn resolve_intermediate_root_interval(l1_rpc: &Url, dgf_address: Address) -> Result<u64> {
    if dgf_address.is_zero() {
        tracing::warn!(
            fallback = DEFAULT_INTERMEDIATE_ROOT_INTERVAL,
            "DGF_ADDRESS not set; using DEFAULT_INTERMEDIATE_ROOT_INTERVAL"
        );
        return Ok(DEFAULT_INTERMEDIATE_ROOT_INTERVAL);
    }

    let interval = read_intermediate_block_interval(l1_rpc, dgf_address).await?;
    if interval == 0 {
        anyhow::bail!(
            "on-chain INTERMEDIATE_BLOCK_INTERVAL is 0 for game type {OP_SUCCINCT_VALIDITY_GAME_TYPE}; this is likely a contract misconfiguration"
        );
    }
    tracing::info!(
        interval,
        game_type = OP_SUCCINCT_VALIDITY_GAME_TYPE,
        "using INTERMEDIATE_BLOCK_INTERVAL from on-chain AggregateVerifier implementation"
    );
    Ok(interval)
}

async fn read_intermediate_block_interval(l1_rpc: &Url, dgf_address: Address) -> Result<u64> {
    let factory = DisputeGameFactoryContractClient::new(dgf_address, l1_rpc.clone())
        .map_err(|e| anyhow::anyhow!("DisputeGameFactory client: {e}"))?;

    let impl_address = factory
        .game_impls(OP_SUCCINCT_VALIDITY_GAME_TYPE)
        .await
        .map_err(|e| anyhow::anyhow!("game_impls({OP_SUCCINCT_VALIDITY_GAME_TYPE}): {e}"))?;

    if impl_address.is_zero() {
        anyhow::bail!(
            "no AggregateVerifier implementation registered for game type {OP_SUCCINCT_VALIDITY_GAME_TYPE}"
        );
    }

    let verifier = AggregateVerifierContractClient::new(l1_rpc.clone())
        .map_err(|e| anyhow::anyhow!("AggregateVerifier client: {e}"))?;

    verifier
        .read_intermediate_block_interval(impl_address)
        .await
        .map_err(|e| anyhow::anyhow!("INTERMEDIATE_BLOCK_INTERVAL: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn falls_back_to_default_when_dgf_zero() {
        let url = Url::parse("http://localhost:8545").unwrap();
        let interval = resolve_intermediate_root_interval(&url, Address::ZERO).await.unwrap();
        assert_eq!(interval, DEFAULT_INTERMEDIATE_ROOT_INTERVAL);
    }
}
