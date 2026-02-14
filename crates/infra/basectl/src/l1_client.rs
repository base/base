use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{ProviderBuilder, layers::CallBatchLayer};
use alloy_sol_types::sol;
use anyhow::Result;

sol! {
    #[sol(rpc)]
    interface ISystemConfig {
        function gasLimit() external view returns (uint64);
        function eip1559Elasticity() external view returns (uint32);
        function eip1559Denominator() external view returns (uint32);
        function batcherHash() external view returns (bytes32);
        function overhead() external view returns (uint256);
        function scalar() external view returns (uint256);
        function unsafeBlockSigner() external view returns (address);
        function startBlock() external view returns (uint256);
        function basefeeScalar() external view returns (uint32);
        function blobbasefeeScalar() external view returns (uint32);
    }
}

/// Full system configuration with all available fields.
/// Fields are `Option` because not all contracts have all functions (version differences).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FullSystemConfig {
    /// Block gas limit.
    pub gas_limit: Option<u64>,
    /// EIP-1559 elasticity multiplier.
    pub eip1559_elasticity: Option<u32>,
    /// EIP-1559 denominator.
    pub eip1559_denominator: Option<u32>,
    /// Batcher hash (32 bytes).
    pub batcher_hash: Option<[u8; 32]>,
    /// L1 fee overhead.
    pub overhead: Option<U256>,
    /// L1 fee scalar.
    pub scalar: Option<U256>,
    /// Unsafe block signer address.
    pub unsafe_block_signer: Option<Address>,
    /// `SystemConfig` start block.
    pub start_block: Option<U256>,
    /// Base fee scalar for L1 cost computation.
    pub basefee_scalar: Option<u32>,
    /// Blob base fee scalar for L1 cost computation.
    pub blobbasefee_scalar: Option<u32>,
}

/// Fetch all available `SystemConfig` values from the L1 contract.
///
/// Uses Multicall3 via `CallBatchLayer` to batch all calls into a single RPC request.
/// Each field is wrapped in `Option` to handle version differences in the `SystemConfig` contract.
pub(crate) async fn fetch_full_system_config(
    l1_rpc_url: &str,
    system_config_address: Address,
) -> Result<FullSystemConfig> {
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
    let unsafe_block_signer_call = contract.unsafeBlockSigner();
    let start_block_call = contract.startBlock();
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
        unsafe_block_signer,
        start_block,
        basefee_scalar,
        blobbasefee_scalar,
    ) = tokio::join!(
        gas_limit_call.call(),
        eip1559_elasticity_call.call(),
        eip1559_denominator_call.call(),
        batcher_hash_call.call(),
        overhead_call.call(),
        scalar_call.call(),
        unsafe_block_signer_call.call(),
        start_block_call.call(),
        basefee_scalar_call.call(),
        blobbasefee_scalar_call.call(),
    );

    Ok(FullSystemConfig {
        gas_limit: gas_limit.ok(),
        eip1559_elasticity: eip1559_elasticity.ok(),
        eip1559_denominator: eip1559_denominator.ok(),
        batcher_hash: batcher_hash.ok().map(|h| h.0),
        overhead: overhead.ok(),
        scalar: scalar.ok(),
        unsafe_block_signer: unsafe_block_signer.ok(),
        start_block: start_block.ok(),
        basefee_scalar: basefee_scalar.ok(),
        blobbasefee_scalar: blobbasefee_scalar.ok(),
    })
}
