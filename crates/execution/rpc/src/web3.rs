use alloy_primitives::{B256, Bytes, keccak256};
use async_trait::async_trait;
use base_execution_network_service::{NetworkHandle, NetworkInfo};
use jsonrpsee::core::RpcResult;

use crate::Web3ApiServer;

/// `web3` API implementation.
///
/// This type provides the functionality for handling `web3` related requests.
pub struct Web3Api {
    /// An interface to interact with the network
    network: NetworkHandle,
}

impl Web3Api {
    /// Creates a new instance of `Web3Api`.
    pub const fn new(network: NetworkHandle) -> Self {
        Self { network }
    }
}

#[async_trait]
impl Web3ApiServer for Web3Api {
    /// Handler for `web3_clientVersion`
    async fn client_version(&self) -> RpcResult<String> {
        let status = self
            .network
            .network_status()
            .await
            .map_err(|err| crate::RpcErrorFactory::internal(err.to_string()))?;
        Ok(status.client_version)
    }

    /// Handler for `web3_sha3`
    fn sha3(&self, input: Bytes) -> RpcResult<B256> {
        Ok(keccak256(input))
    }
}

impl std::fmt::Debug for Web3Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Web3Api").finish_non_exhaustive()
    }
}
