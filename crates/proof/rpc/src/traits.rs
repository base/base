//! Async trait definitions for RPC clients.

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header, TransactionReceipt};
use async_trait::async_trait;
use base_consensus_genesis::RollupConfig;

use super::{
    error::RpcResult,
    types::{OpBlock, OutputAtBlock, SyncStatus},
};

/// L1 RPC provider trait for interacting with Ethereum.
#[async_trait]
pub trait L1Provider: Send + Sync {
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

    /// Gets the ETH balance of an address at the latest block.
    async fn get_balance(&self, address: Address) -> RpcResult<U256>;
}

/// L2 RPC provider trait for interacting with Base.
#[async_trait]
pub trait L2Provider: Send + Sync {
    /// Gets the chain configuration via `debug_chainConfig`.
    async fn chain_config(&self) -> RpcResult<serde_json::Value>;

    /// Gets an account proof via `eth_getProof`.
    async fn get_proof(
        &self,
        address: Address,
        block_hash: B256,
    ) -> RpcResult<EIP1186AccountProofResponse>;

    /// Gets a header by block number.
    /// If `number` is `None`, returns the latest header.
    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header>;

    /// Gets a block by number with full transactions.
    /// If `number` is `None`, returns the latest block.
    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock>;

    /// Gets a block by hash with full transactions.
    async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock>;
}

/// Rollup RPC provider trait for interacting with Base rollup nodes.
#[async_trait]
pub trait RollupProvider: Send + Sync {
    /// Gets the rollup configuration via `optimism_rollupConfig`.
    async fn rollup_config(&self) -> RpcResult<RollupConfig>;

    /// Gets the sync status via `optimism_syncStatus`.
    async fn sync_status(&self) -> RpcResult<SyncStatus>;

    /// Gets the output root at a specific L2 block via `optimism_outputAtBlock`.
    async fn output_at_block(&self, block_number: u64) -> RpcResult<OutputAtBlock>;
}
