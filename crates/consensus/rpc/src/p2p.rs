//! RPC Module to serve the P2P API.

use std::{net::IpAddr, str::FromStr, time::Duration};

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use base_consensus_gossip::{Metrics, P2pRpcRequest, PeerCount, PeerDump, PeerInfo, PeerStats};
use ipnet::IpNet;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode, ErrorObject},
};

use crate::{BaseP2PApiServer, net::P2pRpc};

#[async_trait]
impl BaseP2PApiServer for P2pRpc {
    async fn opp2p_self(&self) -> RpcResult<PeerInfo> {
        Metrics::rpc_calls("opp2p_self").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::PeerInfo(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_peer_count(&self) -> RpcResult<PeerCount> {
        Metrics::rpc_calls("opp2p_peerCount").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::PeerCount(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        let (connected_discovery, connected_gossip) =
            rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        Ok(PeerCount { connected_discovery, connected_gossip })
    }

    async fn opp2p_peers(&self, connected: bool) -> RpcResult<PeerDump> {
        Metrics::rpc_calls("opp2p_peers").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::Peers { out: tx, connected })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        let dump = rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        Ok(dump)
    }

    async fn opp2p_peer_stats(&self) -> RpcResult<PeerStats> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::PeerStats(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        let stats = rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        Ok(stats)
    }

    async fn opp2p_discovery_table(&self) -> RpcResult<Vec<String>> {
        Metrics::rpc_calls("opp2p_discoveryTable").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::DiscoveryTable(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_block_peer(&self, peer_id: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_blockPeer").increment(1.0);
        let id = libp2p::PeerId::from_str(&peer_id)
            .map_err(|_| ErrorObject::from(ErrorCode::InvalidParams))?;
        self.sender
            .send(P2pRpcRequest::BlockPeer { id })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_unblock_peer(&self, peer_id: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_unblockPeer").increment(1.0);
        let id = libp2p::PeerId::from_str(&peer_id)
            .map_err(|_| ErrorObject::from(ErrorCode::InvalidParams))?;
        self.sender
            .send(P2pRpcRequest::UnblockPeer { id })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_list_blocked_peers(&self) -> RpcResult<Vec<String>> {
        Metrics::rpc_calls("opp2p_listBlockedPeers").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::ListBlockedPeers(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await
            .map(|peers| peers.iter().map(|p| p.to_string()).collect())
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_block_addr(&self, address: IpAddr) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_blockAddr").increment(1.0);
        self.sender
            .send(P2pRpcRequest::BlockAddr { address })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_unblock_addr(&self, address: IpAddr) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_unblockAddr").increment(1.0);
        self.sender
            .send(P2pRpcRequest::UnblockAddr { address })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_list_blocked_addrs(&self) -> RpcResult<Vec<IpAddr>> {
        Metrics::rpc_calls("opp2p_listBlockedAddrs").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::ListBlockedAddrs(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_block_subnet(&self, subnet: IpNet) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_blockSubnet").increment(1.0);
        self.sender
            .send(P2pRpcRequest::BlockSubnet { address: subnet })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_unblock_subnet(&self, subnet: IpNet) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_unblockSubnet").increment(1.0);

        self.sender
            .send(P2pRpcRequest::UnblockSubnet { address: subnet })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_list_blocked_subnets(&self) -> RpcResult<Vec<IpNet>> {
        Metrics::rpc_calls("opp2p_listBlockedSubnets").increment(1.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(P2pRpcRequest::ListBlockedSubnets(tx))
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        rx.await.map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_protect_peer(&self, id: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_protectPeer").increment(1.0);
        let peer_id = libp2p::PeerId::from_str(&id)
            .map_err(|_| ErrorObject::from(ErrorCode::InvalidParams))?;
        self.sender
            .send(P2pRpcRequest::ProtectPeer { peer_id })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_unprotect_peer(&self, id: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_unprotectPeer").increment(1.0);
        let peer_id = libp2p::PeerId::from_str(&id)
            .map_err(|_| ErrorObject::from(ErrorCode::InvalidParams))?;
        self.sender
            .send(P2pRpcRequest::UnprotectPeer { peer_id })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))
    }

    async fn opp2p_connect_peer(&self, _peer: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_connectPeer").increment(1.0);
        let ma = libp2p::Multiaddr::from_str(&_peer).map_err(|_| {
            ErrorObject::borrowed(ErrorCode::InvalidParams.code(), "Invalid multiaddr", None)
        })?;

        let peer_id = ma
            .iter()
            .find_map(|component| match component {
                libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
                _ => None,
            })
            .ok_or_else(|| {
                ErrorObject::borrowed(
                    ErrorCode::InvalidParams.code(),
                    "Impossible to extract peer ID from multiaddr",
                    None,
                )
            })?;

        self.sender.send(P2pRpcRequest::ConnectPeer { address: ma }).await.map_err(|_| {
            ErrorObject::borrowed(
                ErrorCode::InternalError.code(),
                "Failed to send connect peer request",
                None,
            )
        })?;

        // We need to wait until both peers are connected to each other to return from this method.
        // We try with an exponential backoff and return an error if we fail to connect to the peer.
        let is_connected = async || {
            let (tx, rx) = tokio::sync::oneshot::channel();

            self.sender
                .send(P2pRpcRequest::Peers { out: tx, connected: true })
                .await
                .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

            let peers = rx.await.map_err(|_| {
                ErrorObject::borrowed(ErrorCode::InternalError.code(), "Failed to get peers", None)
            })?;

            Ok::<bool, ErrorObject<'_>>(peers.peers.contains_key(&peer_id.to_string()))
        };

        if !is_connected
            .retry(ExponentialBuilder::default().with_total_delay(Some(Duration::from_secs(10))))
            .await?
        {
            return Err(ErrorObject::borrowed(
                ErrorCode::InvalidParams.code(),
                "Peer not connected",
                None,
            ));
        }

        Ok(())
    }

    async fn opp2p_disconnect_peer(&self, peer_id: String) -> RpcResult<()> {
        Metrics::rpc_calls("opp2p_disconnectPeer").increment(1.0);
        let peer_id = match peer_id.parse() {
            Ok(id) => id,
            Err(err) => {
                warn!(target: "rpc", ?err, ?peer_id, "Failed to parse peer ID");
                return Err(ErrorObject::from(ErrorCode::InvalidParams));
            }
        };

        self.sender
            .send(P2pRpcRequest::DisconnectPeer { peer_id })
            .await
            .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

        // We need to wait until both peers are fully disconnected to each other to return from this
        // method. We try with an exponential backoff and return an error if we fail to
        // disconnect from the peer.
        let is_not_connected = async || {
            let (tx, rx) = tokio::sync::oneshot::channel();

            self.sender
                .send(P2pRpcRequest::Peers { out: tx, connected: true })
                .await
                .map_err(|_| ErrorObject::from(ErrorCode::InternalError))?;

            let peers = rx.await.map_err(|_| {
                ErrorObject::borrowed(ErrorCode::InternalError.code(), "Failed to get peers", None)
            })?;

            Ok::<bool, ErrorObject<'_>>(!peers.peers.contains_key(&peer_id.to_string()))
        };

        if !is_not_connected
            .retry(ExponentialBuilder::default().with_total_delay(Some(Duration::from_secs(10))))
            .await?
        {
            return Err(ErrorObject::borrowed(
                ErrorCode::InvalidParams.code(),
                "Peers are still connected",
                None,
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    #[test]
    fn test_parse_multiaddr_string() {
        let ma = "/ip4/127.0.0.1/udt";
        let multiaddr = libp2p::Multiaddr::from_str(ma).unwrap();
        let components = multiaddr.iter().collect::<Vec<_>>();
        assert_eq!(
            components[0],
            libp2p::multiaddr::Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(components[1], libp2p::multiaddr::Protocol::Udt);
    }
}
