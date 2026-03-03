use alloy_primitives::{B256, Bytes};
use async_trait::async_trait;
use base_enclave::ExecutionWitness;
use base_proof_rpc::{L2Client, RpcResult};

/// Extension trait for proposer-specific L2 RPC methods.
///
/// These methods are only needed by the proposer for generating proofs,
/// not by generic L2 consumers such as the challenger.
#[async_trait]
pub trait ProverL2Client: L2Client {
    /// Gets the execution witness for a block via `debug_executionWitness`.
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness>;

    /// Gets a raw DB value by key via `debug_dbGet`.
    async fn db_get(&self, key: B256) -> RpcResult<Bytes>;
}

// Blanket implementation for Box<dyn ProverL2Client> to support dynamic dispatch.
#[async_trait]
impl ProverL2Client for Box<dyn ProverL2Client> {
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness> {
        (**self).execution_witness(block_number).await
    }

    async fn db_get(&self, key: B256) -> RpcResult<Bytes> {
        (**self).db_get(key).await
    }
}

// Delegate L2Client supertrait methods for Box<dyn ProverL2Client>.
// Rust does not auto-delegate supertrait methods through trait objects,
// so this must stay in sync with the L2Client trait in base-proof-rpc.
#[async_trait]
impl L2Client for Box<dyn ProverL2Client> {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        (**self).chain_config().await
    }

    async fn get_proof(
        &self,
        address: alloy_primitives::Address,
        block_hash: B256,
    ) -> RpcResult<base_enclave::AccountResult> {
        (**self).get_proof(address, block_hash).await
    }

    async fn header_by_number(
        &self,
        number: Option<u64>,
    ) -> RpcResult<alloy_rpc_types_eth::Header> {
        (**self).header_by_number(number).await
    }

    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<base_proof_rpc::OpBlock> {
        (**self).block_by_number(number).await
    }

    async fn block_by_hash(&self, hash: B256) -> RpcResult<base_proof_rpc::OpBlock> {
        (**self).block_by_hash(hash).await
    }
}
