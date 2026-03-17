//! Reth-specific L2 client implementation.

use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::Header;
use async_trait::async_trait;
use base_enclave::AccountResult;
use base_proof_rpc::{L2Client, L2ClientConfig, L2Provider, OpBlock, RpcResult};

pub struct RethL2Client {
    inner: L2Client,
}

impl std::fmt::Debug for RethL2Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RethL2Client").field("inner", &self.inner).finish()
    }
}

impl RethL2Client {
    /// Creates a new Reth L2 client from the given configuration.
    pub fn new(config: L2ClientConfig) -> RpcResult<Self> {
        Ok(Self { inner: L2Client::new(config)? })
    }

    pub const fn as_l2_client(&self) -> &L2Client {
        &self.inner
    }
}

#[async_trait]
impl L2Provider for RethL2Client {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        self.as_l2_client().chain_config().await
    }

    async fn get_proof(&self, address: Address, block_hash: B256) -> RpcResult<AccountResult> {
        self.as_l2_client().get_proof(address, block_hash).await
    }

    async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header> {
        self.as_l2_client().header_by_number(number).await
    }

    async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
        self.as_l2_client().block_by_number(number).await
    }

    async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
        self.as_l2_client().block_by_hash(hash).await
    }
}
