// Re-exports from base-proof-rpc
pub use base_proof_rpc::{
    CacheMetrics, HttpProvider, L1Client, L1ClientConfig, L1ClientImpl, L2Client, L2ClientConfig,
    L2ClientImpl, L2HttpProvider, MeteredCache, OpBlock, ProofCacheKey, RollupClient,
    RollupClientConfig, RollupClientImpl, RpcError, RpcResult,
    // types
    GenesisL2BlockRef, L1BlockId, L1BlockRef, L2BlockRef, SyncStatus,
    // config
    RetryConfig,
};

mod prover_l2_client;
pub use prover_l2_client::ProverL2Client;

mod l2_client_ext;

mod reth_client;
pub use reth_client::RethL2Client;

mod types;
pub use types::RethExecutionWitness;

/// Creates an L2 client based on the configuration.
///
/// If `is_reth` is true, returns a [`RethL2Client`] that handles reth-specific
/// witness format conversion. Otherwise, returns a standard [`L2ClientImpl`].
pub fn create_l2_client(
    config: L2ClientConfig,
    is_reth: bool,
) -> RpcResult<Box<dyn ProverL2Client>> {
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
