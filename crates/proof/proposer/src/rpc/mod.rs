use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_eth::Header;
use async_trait::async_trait;
use base_enclave::{AccountResult, ExecutionWitness};
// Re-exports from base-proof-rpc
pub use base_proof_rpc::{
    CacheMetrics, GenesisL2BlockRef, HttpProvider, L1BlockId, L1BlockRef, L1Client, L1ClientConfig,
    L1ClientImpl, L2BlockRef, L2Client, L2ClientConfig, L2ClientImpl, L2HttpProvider, MeteredCache,
    OpBlock, ProofCacheKey, RetryConfig, RollupClient, RollupClientConfig, RollupClientImpl,
    RpcError, RpcResult, SyncStatus,
};

mod prover_l2_client;
pub use prover_l2_client::ProverL2Client;

// Impl-only: provides ProverL2Client impl for L2ClientImpl.
mod l2_client_ext;

mod reth_client;
pub use reth_client::RethL2Client;

mod types;
pub use types::RethExecutionWitness;

/// Enum dispatch for [`ProverL2Client`], replacing `Box<dyn ProverL2Client>`.
///
/// Since there are only two concrete implementations, enum dispatch avoids
/// trait-object boilerplate and the fragile supertrait delegation it requires.
#[derive(Debug)]
pub enum L2ClientKind {
    /// Standard geth-compatible L2 client.
    Standard(L2ClientImpl),
    /// Reth-specific L2 client with witness format conversion.
    Reth(RethL2Client),
}

impl L2ClientKind {
    /// Returns a reference to the underlying [`L2ClientImpl`].
    ///
    /// Both variants delegate all [`L2Client`] methods to an [`L2ClientImpl`],
    /// so this helper eliminates per-method match arms for supertrait dispatch.
    const fn as_l2_client(&self) -> &L2ClientImpl {
        match self {
            Self::Standard(c) => c,
            Self::Reth(c) => c.as_l2_client(),
        }
    }
}

#[async_trait]
impl L2Client for L2ClientKind {
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

#[async_trait]
impl ProverL2Client for L2ClientKind {
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness> {
        match self {
            Self::Standard(c) => c.execution_witness(block_number).await,
            Self::Reth(c) => c.execution_witness(block_number).await,
        }
    }

    async fn db_get(&self, key: B256) -> RpcResult<Bytes> {
        match self {
            Self::Standard(c) => c.db_get(key).await,
            Self::Reth(c) => c.db_get(key).await,
        }
    }
}

/// Creates an L2 client based on the configuration.
///
/// If `is_reth` is true, returns a [`RethL2Client`] that handles reth-specific
/// witness format conversion. Otherwise, returns a standard [`L2ClientImpl`].
pub fn create_l2_client(config: L2ClientConfig, is_reth: bool) -> RpcResult<L2ClientKind> {
    if is_reth {
        Ok(L2ClientKind::Reth(RethL2Client::new(config)?))
    } else {
        Ok(L2ClientKind::Standard(L2ClientImpl::new(config)?))
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    #[test]
    fn test_create_l2_client_standard() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        let client = create_l2_client(config, false);
        assert!(client.is_ok());
    }

    #[test]
    fn test_create_l2_client_reth() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        let client = create_l2_client(config, true);
        assert!(client.is_ok());
    }
}
