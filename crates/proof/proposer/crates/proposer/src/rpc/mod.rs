//! RPC client implementations for L1, L2, and Rollup nodes.
//!
//! This module provides async RPC clients for interacting with:
//! - L1 Ethereum nodes
//! - L2 OP Stack nodes (standard and reth-specific)
//! - OP Stack rollup nodes
//!
//! All clients include LRU caching with metrics for observability.

use alloy::network::Ethereum;
use alloy::providers::RootProvider;

/// Shared type alias for the HTTP provider.
/// Uses `RootProvider` directly since these clients only perform read operations.
pub(crate) type HttpProvider = RootProvider<Ethereum>;

mod cache;
mod error;
mod l1_client;
mod l2_client;
mod reth_client;
mod rollup_client;
mod traits;
mod types;

// Re-export cache types
pub use cache::{CacheMetrics, MeteredCache};

// Re-export error types
pub use error::{RpcError, RpcResult};

// Re-export client configurations
pub use l1_client::{L1ClientConfig, L1ClientImpl};
pub use l2_client::{L2ClientConfig, L2ClientImpl, ProofCacheKey};
pub use reth_client::RethL2Client;
pub use rollup_client::{RollupClientConfig, RollupClientImpl};

// Re-export traits
pub use traits::{L1Client, L2Client, RollupClient};

// Re-export custom types
pub use types::{
    GenesisConfig, L1BlockRef, L2BlockRef, RethExecutionWitness, RollupConfig, SyncStatus,
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
