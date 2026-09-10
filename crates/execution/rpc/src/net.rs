use alloy_primitives::U64;
use base_execution_network_service::PeersInfo;
use jsonrpsee::core::RpcResult as Result;

use crate::{BaseEthApi, NetApiServer};

/// `Net` API implementation.
///
/// This type provides the functionality for handling `net` related requests.
pub struct NetApi<Net> {
    /// An interface to interact with the network
    network: Net,
    /// The implementation of `eth` API
    eth: BaseEthApi,
}

// === impl NetApi ===

impl<Net> NetApi<Net> {
    /// Returns a new instance with the given network and eth interface implementations
    pub const fn new(network: Net, eth: BaseEthApi) -> Self {
        Self { network, eth }
    }
}

/// Net rpc implementation
impl<Net> NetApiServer for NetApi<Net>
where
    Net: PeersInfo + 'static,
{
    /// Handler for `net_version`
    fn version(&self) -> Result<String> {
        // Note: net_version is numeric: <https://github.com/paradigmxyz/reth/issues/5569>
        Ok(self.eth.chain_id().to::<u64>().to_string())
    }

    /// Handler for `net_peerCount`
    fn peer_count(&self) -> Result<U64> {
        Ok(U64::from(self.network.num_connected_peers()))
    }

    /// Handler for `net_listening`
    fn is_listening(&self) -> Result<bool> {
        Ok(true)
    }
}

impl<Net> std::fmt::Debug for NetApi<Net> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetApi").finish_non_exhaustive()
    }
}
