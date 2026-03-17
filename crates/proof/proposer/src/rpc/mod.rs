//! L2 client implementations for the proposer.

use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::Header;
use async_trait::async_trait;
use base_enclave::AccountResult;
use base_proof_rpc::{L2Client, L2ClientConfig, L2Provider, OpBlock, RpcResult};

mod reth_client;
pub use reth_client::RethL2Client;

/// Enum dispatch for L2 provider implementations.
#[derive(Debug)]
pub enum L2ClientKind {
    Standard(L2Client),
    Reth(RethL2Client),
}

impl L2ClientKind {
    /// Creates an L2 client based on the configuration.
    ///
    /// If `is_reth` is true, returns a [`RethL2Client`] that handles reth-specific
    /// witness format conversion. Otherwise, returns a standard [`L2Client`].
    pub fn new(config: L2ClientConfig, is_reth: bool) -> RpcResult<Self> {
        if is_reth {
            Ok(Self::Reth(RethL2Client::new(config)?))
        } else {
            Ok(Self::Standard(L2Client::new(config)?))
        }
    }

    /// Returns a reference to the underlying [`L2Client`].
    ///
    /// Both variants delegate all [`L2Provider`] methods to an [`L2Client`],
    /// so this helper eliminates per-method match arms for supertrait dispatch.
    const fn as_l2_client(&self) -> &L2Client {
        match self {
            Self::Standard(c) => c,
            Self::Reth(c) => c.as_l2_client(),
        }
    }
}

#[async_trait]
impl L2Provider for L2ClientKind {
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

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    #[test]
    fn test_create_l2_client_standard() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        let client = L2ClientKind::new(config, false);
        assert!(client.is_ok());
    }

    #[test]
    fn test_create_l2_client_reth() {
        let config = L2ClientConfig::new(Url::parse("http://localhost:8545").unwrap());
        let client = L2ClientKind::new(config, true);
        assert!(client.is_ok());
    }
}
