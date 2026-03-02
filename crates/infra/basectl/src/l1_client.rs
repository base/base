use std::time::Duration;

use alloy_primitives::Address;
use alloy_provider::{ProviderBuilder, layers::CallBatchLayer};
use alloy_sol_types::sol;
use anyhow::Result;
use base_consensus_genesis::SystemConfig;

sol! {
    #[sol(rpc)]
    interface ISystemConfig {
        function gasLimit() external view returns (uint64);
        function eip1559Elasticity() external view returns (uint32);
        function eip1559Denominator() external view returns (uint32);
        function batcherHash() external view returns (bytes32);
        function overhead() external view returns (uint256);
        function scalar() external view returns (uint256);
        function basefeeScalar() external view returns (uint32);
        function blobbasefeeScalar() external view returns (uint32);
    }
}

/// Fetch all available `SystemConfig` values from the L1 contract.
///
/// Uses Multicall3 via `CallBatchLayer` to batch all calls into a single RPC request.
/// Fields that are absent on older contract versions fall back to their defaults.
pub(crate) async fn fetch_full_system_config(
    l1_rpc_url: &str,
    system_config_address: Address,
) -> Result<SystemConfig> {
    // Use CallBatchLayer to batch all concurrent calls into a single Multicall3 call
    let provider = ProviderBuilder::new()
        .layer(CallBatchLayer::new().wait(Duration::from_millis(10)))
        .connect(l1_rpc_url)
        .await?;
    let contract = ISystemConfig::new(system_config_address, provider);

    // Create call builders first to avoid temporary borrow issues
    let gas_limit_call = contract.gasLimit();
    let eip1559_elasticity_call = contract.eip1559Elasticity();
    let eip1559_denominator_call = contract.eip1559Denominator();
    let batcher_hash_call = contract.batcherHash();
    let overhead_call = contract.overhead();
    let scalar_call = contract.scalar();
    let basefee_scalar_call = contract.basefeeScalar();
    let blobbasefee_scalar_call = contract.blobbasefeeScalar();

    // Fetch all values concurrently - each may fail on older versions
    let (
        gas_limit,
        eip1559_elasticity,
        eip1559_denominator,
        batcher_hash,
        overhead,
        scalar,
        basefee_scalar,
        blobbasefee_scalar,
    ) = tokio::join!(
        gas_limit_call.call(),
        eip1559_elasticity_call.call(),
        eip1559_denominator_call.call(),
        batcher_hash_call.call(),
        overhead_call.call(),
        scalar_call.call(),
        basefee_scalar_call.call(),
        blobbasefee_scalar_call.call(),
    );

    Ok(SystemConfig {
        batcher_address: batcher_hash
            .ok()
            .map(|h| Address::from_slice(&h.0[12..]))
            .unwrap_or_default(),
        overhead: overhead.ok().unwrap_or_default(),
        scalar: scalar.ok().unwrap_or_default(),
        gas_limit: gas_limit.ok().unwrap_or_default(),
        eip1559_elasticity: eip1559_elasticity.ok(),
        eip1559_denominator: eip1559_denominator.ok(),
        base_fee_scalar: basefee_scalar.ok().map(|v| v as u64),
        blob_base_fee_scalar: blobbasefee_scalar.ok().map(|v| v as u64),
        ..Default::default()
    })
}
