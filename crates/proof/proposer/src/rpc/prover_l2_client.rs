use alloy_primitives::{B256, Bytes};
use async_trait::async_trait;
use base_enclave::ExecutionWitness;
use base_proof_rpc::{L2Provider, RpcResult};

/// Extension trait for proposer-specific L2 RPC methods.
///
/// These methods are only needed by the proposer for generating proofs,
/// not by generic L2 consumers such as the challenger.
#[async_trait]
pub trait ProverL2Provider: L2Provider {
    /// Gets the execution witness for a block via `debug_executionWitness`.
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness>;

    /// Gets a raw DB value by key via `debug_dbGet`.
    async fn db_get(&self, key: B256) -> RpcResult<Bytes>;
}
