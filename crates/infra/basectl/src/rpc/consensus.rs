//! Consensus-node JSON-RPC client.

use std::{fmt, future::Future, time::Duration};

use alloy_primitives::B256;
use base_common_genesis::RollupConfig;
use base_consensus_gossip::{PeerDump, PeerInfo, PeerStats};
use base_consensus_rpc::{AdminApiClient, BaseApiClient, BaseP2PApiClient, RollupNodeApiClient};
use base_protocol::SyncStatus;
use base_upgrade_signal::UpgradeReadiness;
use jsonrpsee::{
    core::client::Error as JsonRpcClientError,
    http_client::{HttpClient, HttpClientBuilder},
    types::error::METHOD_NOT_FOUND_CODE,
};
use tracing::warn;
use url::Url;

use crate::ConsensusRpcError;

/// Client for one consensus node's JSON-RPC endpoint.
///
/// Calls fail after the client's request timeout, except [`Self::stop_sequencer`] and
/// [`Self::upgrade_readiness`], which always allow their own longer bounds.
#[derive(Clone)]
pub struct ConsensusClient {
    /// Endpoint origin shown in errors and logs, without credentials, path, or query.
    origin: String,
    client: HttpClient,
    timeout: Duration,
}

impl fmt::Debug for ConsensusClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The inner client holds the full URL and a basic-auth header built from its credentials.
        f.debug_struct("ConsensusClient")
            .field("origin", &self.origin)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ConsensusClient {
    /// Default request timeout for one-shot reads and p2p peer management.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Request timeout for `admin_sequencerActive` polls and `admin_startSequencer`.
    pub const SEQUENCER_ADMIN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Timeout of `admin_stopSequencer`, which may defer its response until the seal pipeline
    /// finishes and the final unsafe head is known. Also the longest timeout any call can use.
    pub const STOP_SEQUENCER_TIMEOUT: Duration = Duration::from_secs(60);

    /// Timeout of `base_upgradeReadiness`.
    pub const UPGRADE_READINESS_TIMEOUT: Duration = Duration::from_secs(30);

    /// Builds a client for `rpc` with the [`Self::REQUEST_TIMEOUT`] request timeout.
    pub fn connect(rpc: &Url) -> Result<Self, ConsensusRpcError> {
        let origin = rpc.origin().ascii_serialization();
        let client = HttpClientBuilder::default()
            .request_timeout(Self::STOP_SEQUENCER_TIMEOUT)
            .build(rpc.as_str())
            .map_err(|source| ConsensusRpcError::BuildClient { rpc: origin.clone(), source })?;
        Ok(Self { origin, client, timeout: Self::REQUEST_TIMEOUT })
    }

    /// Sets the request timeout, capped at [`Self::STOP_SEQUENCER_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.min(Self::STOP_SEQUENCER_TIMEOUT);
        self
    }

    /// Returns whether the sequencer is active, via `admin_sequencerActive`.
    pub async fn sequencer_active(&self) -> Result<bool, ConsensusRpcError> {
        self.call("admin_sequencerActive", AdminApiClient::admin_sequencer_active(&self.client))
            .await
    }

    /// Starts the sequencer at `unsafe_head` via `admin_startSequencer`.
    ///
    /// [`B256::ZERO`] is rejected without a request. The node rejects a hash that does not match
    /// its current engine unsafe head, and when op-conductor is enabled, any call to a node that
    /// is not the Raft leader.
    pub async fn start_sequencer(&self, unsafe_head: B256) -> Result<(), ConsensusRpcError> {
        if unsafe_head == B256::ZERO {
            return Err(ConsensusRpcError::ZeroUnsafeHead);
        }
        self.call(
            "admin_startSequencer",
            AdminApiClient::admin_start_sequencer(&self.client, unsafe_head),
        )
        .await
    }

    /// Stops the sequencer via `admin_stopSequencer` and returns the unsafe head captured when it
    /// stopped.
    ///
    /// A returned [`B256::ZERO`] means the sequencer stopped but the captured head is unavailable;
    /// it is not a valid restart point. The stop has already taken effect, so this is a warning
    /// rather than an error and callers must not retry.
    pub async fn stop_sequencer(&self) -> Result<B256, ConsensusRpcError> {
        let unsafe_head = self
            .call_within(
                Self::STOP_SEQUENCER_TIMEOUT,
                "admin_stopSequencer",
                AdminApiClient::admin_stop_sequencer(&self.client),
            )
            .await?;
        if unsafe_head == B256::ZERO {
            warn!(
                cl_rpc = %self.origin,
                "admin_stopSequencer returned a zero unsafe head; sequencer stopped but the captured head is unavailable"
            );
        }
        Ok(unsafe_head)
    }

    /// Returns the rollup node's L1 and L2 heads via `optimism_syncStatus`.
    pub async fn sync_status(&self) -> Result<SyncStatus, ConsensusRpcError> {
        self.call("optimism_syncStatus", RollupNodeApiClient::sync_status(&self.client)).await
    }

    /// Returns the live rollup config via `optimism_rollupConfig`.
    pub async fn rollup_config(&self) -> Result<RollupConfig, ConsensusRpcError> {
        self.call("optimism_rollupConfig", RollupNodeApiClient::rollup_config(&self.client)).await
    }

    /// Returns the node's upgrade readiness via `base_upgradeReadiness`.
    ///
    /// `target_version`, when set, is an announced `major.minor.patch[-rc.N]` version to judge
    /// readiness against before the upgrade is scheduled on L1; the node validates it.
    pub async fn upgrade_readiness(
        &self,
        target_version: Option<String>,
    ) -> Result<UpgradeReadiness, ConsensusRpcError> {
        self.call_within(
            Self::UPGRADE_READINESS_TIMEOUT,
            "base_upgradeReadiness",
            BaseApiClient::upgrade_readiness(&self.client, target_version),
        )
        .await
    }

    /// Returns the node's own advertised peer record via `opp2p_self`.
    pub async fn self_info(&self) -> Result<PeerInfo, ConsensusRpcError> {
        self.call("opp2p_self", BaseP2PApiClient::opp2p_self(&self.client)).await
    }

    /// Returns connected peer statistics via `opp2p_peerStats`.
    pub async fn peer_stats(&self) -> Result<PeerStats, ConsensusRpcError> {
        self.call("opp2p_peerStats", BaseP2PApiClient::opp2p_peer_stats(&self.client)).await
    }

    /// Returns known peers via `opp2p_peers`, restricted to connected peers when `connected`.
    pub async fn peers(&self, connected: bool) -> Result<PeerDump, ConsensusRpcError> {
        self.call("opp2p_peers", BaseP2PApiClient::opp2p_peers(&self.client, connected)).await
    }

    /// Connects the peer at `multiaddr` via `opp2p_connectPeer`.
    pub async fn connect_peer(&self, multiaddr: &str) -> Result<(), ConsensusRpcError> {
        self.call(
            "opp2p_connectPeer",
            BaseP2PApiClient::opp2p_connect_peer(&self.client, multiaddr.to_string()),
        )
        .await
    }

    /// Disconnects the peer with libp2p `peer_id` via `opp2p_disconnectPeer`.
    pub async fn disconnect_peer(&self, peer_id: &str) -> Result<(), ConsensusRpcError> {
        self.call(
            "opp2p_disconnectPeer",
            BaseP2PApiClient::opp2p_disconnect_peer(&self.client, peer_id.to_string()),
        )
        .await
    }

    /// Bans the peer with libp2p `peer_id` via `opp2p_blockPeer`.
    pub async fn ban_peer(&self, peer_id: &str) -> Result<(), ConsensusRpcError> {
        self.call(
            "opp2p_blockPeer",
            BaseP2PApiClient::opp2p_block_peer(&self.client, peer_id.to_string()),
        )
        .await
    }

    /// Unbans the peer with libp2p `peer_id` via `opp2p_unblockPeer`.
    pub async fn unban_peer(&self, peer_id: &str) -> Result<(), ConsensusRpcError> {
        self.call(
            "opp2p_unblockPeer",
            BaseP2PApiClient::opp2p_unblock_peer(&self.client, peer_id.to_string()),
        )
        .await
    }

    /// Lists banned peer IDs via `opp2p_listBlockedPeers`.
    pub async fn list_banned_peers(&self) -> Result<Vec<String>, ConsensusRpcError> {
        self.call(
            "opp2p_listBlockedPeers",
            BaseP2PApiClient::opp2p_list_blocked_peers(&self.client),
        )
        .await
    }

    async fn call<T>(
        &self,
        method: &'static str,
        request: impl Future<Output = Result<T, JsonRpcClientError>>,
    ) -> Result<T, ConsensusRpcError> {
        self.call_within(self.timeout, method, request).await
    }

    async fn call_within<T>(
        &self,
        timeout: Duration,
        method: &'static str,
        request: impl Future<Output = Result<T, JsonRpcClientError>>,
    ) -> Result<T, ConsensusRpcError> {
        let rpc = || self.origin.clone();
        match tokio::time::timeout(timeout, request).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(JsonRpcClientError::Call(payload)))
                if payload.code() == METHOD_NOT_FOUND_CODE =>
            {
                Err(ConsensusRpcError::MethodUnavailable { rpc: rpc(), method })
            }
            Ok(Err(JsonRpcClientError::RequestTimeout)) | Err(_) => {
                Err(ConsensusRpcError::Timeout { rpc: rpc(), method, timeout })
            }
            Ok(Err(source)) => Err(ConsensusRpcError::Rpc { rpc: rpc(), method, source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::{
        server::{RpcModule, Server},
        types::ErrorObjectOwned,
    };

    use super::*;

    /// A missing method is reported as unavailable so callers can degrade, a short client
    /// timeout bounds ordinary calls but not `admin_stopSequencer`, and neither errors nor
    /// `Debug` expose the endpoint's credentials.
    #[tokio::test]
    async fn client_classifies_failures_and_bounds_calls() {
        const DELAY: Duration = Duration::from_millis(200);
        const TIMEOUT: Duration = Duration::from_millis(50);
        let head = B256::repeat_byte(1);

        let mut module = RpcModule::new(());
        module
            .register_async_method("admin_sequencerActive", |_, _, _| async {
                tokio::time::sleep(DELAY).await;
                true
            })
            .unwrap();
        module
            .register_async_method("admin_stopSequencer", move |_, _, _| async move {
                tokio::time::sleep(DELAY).await;
                Ok::<_, ErrorObjectOwned>(head)
            })
            .unwrap();
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let handle = server.start(module);
        let rpc = Url::parse(&format!("http://operator:secret@{address}/cl?key=token")).unwrap();
        let client = ConsensusClient::connect(&rpc).unwrap().with_timeout(TIMEOUT);

        let debug = format!("{client:?}");
        assert!(
            !["operator", "secret", "/cl", "token"].iter().any(|s| debug.contains(s)),
            "{debug}"
        );

        let unavailable = client.peer_stats().await.unwrap_err();
        assert!(matches!(unavailable, ConsensusRpcError::MethodUnavailable { .. }));
        assert_eq!(
            unavailable.to_string(),
            format!(
                "consensus-node RPC method `opp2p_peerStats` is not exposed by http://{address}"
            )
        );

        assert!(matches!(
            client.sequencer_active().await,
            Err(ConsensusRpcError::Timeout {
                method: "admin_sequencerActive",
                timeout: TIMEOUT,
                ..
            })
        ));
        assert_eq!(client.stop_sequencer().await.unwrap(), head);

        handle.stop().unwrap();
    }
}
