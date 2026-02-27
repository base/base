use alloy_network::Ethereum;
use alloy_provider::RootProvider;
use base_alloy_network::Base;

/// Shared type alias for the L1 HTTP provider.
/// Uses `RootProvider` directly since these clients only perform read operations.
pub type HttpProvider = RootProvider<Ethereum>;

/// L2-specific provider type using the Base network.
/// Required for deserializing OP Stack deposit transactions (type 0x7E).
pub type L2HttpProvider = RootProvider<Base>;

mod cache;
pub use cache::{CacheMetrics, MeteredCache};

mod error;
pub use error::{RpcError, RpcResult};

mod l1_client;
pub use l1_client::{L1ClientConfig, L1ClientImpl};

mod l2_client;
pub use l2_client::{L2ClientConfig, L2ClientImpl, ProofCacheKey};

mod reth_client;
pub use reth_client::RethL2Client;

mod rollup_client;
pub use rollup_client::{RollupClientConfig, RollupClientImpl};

mod traits;
pub use traits::{L1Client, L2Client, RollupClient};

mod types;
pub use types::{
    GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, OpBlock, RethExecutionWitness, SyncStatus,
};

/// Creates an L2 client based on the configuration.
///
/// If `is_reth` is true, returns a [`RethL2Client`] that handles reth-specific
/// witness format conversion. Otherwise, returns a standard [`L2ClientImpl`].
pub fn create_l2_client(config: L2ClientConfig, is_reth: bool) -> RpcResult<Box<dyn L2Client>> {
    if is_reth {
        Ok(Box::new(RethL2Client::new(config)?))
    } else {
        Ok(Box::new(L2ClientImpl::new(config)?))
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
