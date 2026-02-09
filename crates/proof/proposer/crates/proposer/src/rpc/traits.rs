//! Async trait definitions for RPC clients.

use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_eth::{Block, Header, TransactionReceipt};
use async_trait::async_trait;
use op_enclave_core::{AccountResult, executor::ExecutionWitness};

use super::{
    error::RpcResult,
    types::{RollupConfig, SyncStatus},
};

/// L1 RPC client trait for interacting with Ethereum.
#[async_trait]
pub trait L1Client: Send + Sync {
    /// Gets the latest block number.
    async fn block_number(&self) -> RpcResult<u64>;

    /// Gets a header by block number.
    /// If `number` is `None`, returns the latest header.
    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header>;

    /// Gets a header by block hash.
    async fn header_by_hash(&self, hash: B256) -> RpcResult<Header>;

    /// Gets block receipts by block hash.
    async fn block_receipts(&self, hash: B256) -> RpcResult<Vec<TransactionReceipt>>;

    /// Gets contract code at the given address.
    /// If `block_number` is `None`, uses the latest block.
    async fn code_at(&self, address: Address, block_number: Option<u64>) -> RpcResult<Bytes>;

    /// Executes a contract call without creating a transaction.
    /// If `block_number` is `None`, uses the latest block.
    async fn call_contract(
        &self,
        to: Address,
        data: Bytes,
        block_number: Option<u64>,
    ) -> RpcResult<Bytes>;
}

/// L2 RPC client trait for interacting with OP Stack chains.
#[async_trait]
pub trait L2Client: Send + Sync {
    /// Gets the chain configuration via `debug_chainConfig`.
    async fn chain_config(&self) -> RpcResult<serde_json::Value>;

    /// Gets an account proof via `eth_getProof`.
    async fn get_proof(&self, address: Address, block_hash: B256) -> RpcResult<AccountResult>;

    /// Gets a header by block number.
    /// If `number` is `None`, returns the latest header.
    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header>;

    /// Gets a block by number with full transactions.
    /// If `number` is `None`, returns the latest block.
    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<Block>;

    /// Gets a block by hash with full transactions.
    async fn block_by_hash(&self, hash: B256) -> RpcResult<Block>;

    /// Gets the execution witness for a block via `debug_executionWitness`.
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness>;
}

/// Rollup RPC client trait for interacting with OP Stack rollup nodes.
#[async_trait]
pub trait RollupClient: Send + Sync {
    /// Gets the rollup configuration via `optimism_rollupConfig`.
    async fn rollup_config(&self) -> RpcResult<RollupConfig>;

    /// Gets the sync status via `optimism_syncStatus`.
    async fn sync_status(&self) -> RpcResult<SyncStatus>;
}
