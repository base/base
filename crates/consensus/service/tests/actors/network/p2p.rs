use base_consensus_gossip::ConnectionLimitsConfig;
use base_consensus_rpc::BaseP2PApiClient;

use crate::actors::network::mocks::builder::TestNetworkBuilder;

#[tokio::test(flavor = "multi_thread")]
async fn test_p2p_network_conn() -> anyhow::Result<()> {
    let mut builder = TestNetworkBuilder::new();
    let network_1 = builder.build(vec![]).await;
    let enr_1 = network_1.peer_enr().await?;

    let network_2 = builder.build(vec![enr_1]).await;

    network_2.is_connected_to_with_retries(&network_1).await?;

    network_1.is_connected_to_with_retries(&network_2).await?;

    for network in [&network_1, &network_2] {
        let local = network.peer_info().await?;
        let peers = network.peers().await?;
        // Check both the local RPC's advertised capabilities and the remote
        // capabilities learned over Identify, not just the connection count.
        for info in std::iter::once(&local).chain(peers.peers.values()) {
            let protocols = info.protocols.as_ref().expect("peer protocols available");
            assert!(protocols.iter().any(|protocol| protocol.starts_with("/meshsub/")));
            assert!(
                protocols
                    .iter()
                    .all(|protocol| !protocol.starts_with("/opstack/req/payload_by_number/")),
                "legacy sync must not be advertised: {protocols:?}"
            );
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_large_network_conn() -> anyhow::Result<()> {
    const NETWORKS: usize = 10;

    let mut builder = TestNetworkBuilder::new();

    let (mut networks, mut bootnodes) = (vec![], vec![]);

    for _ in 0..NETWORKS {
        let network = builder.build(bootnodes.clone()).await;
        let enr = network.peer_enr().await?;
        networks.push(network);
        bootnodes.push(enr);
    }

    for network in &networks {
        for other_network in &networks {
            if network.peer_id().await? == other_network.peer_id().await? {
                continue;
            }

            network.is_connected_to_with_retries(other_network).await?;
        }
    }

    Ok(())
}

/// A node at its connection limit refuses `opp2p_connectPeer` from another node unless it has
/// protected that node with `opp2p_protectPeer`.
#[tokio::test(flavor = "multi_thread")]
async fn test_protected_peer_bypasses_connection_limits() -> anyhow::Result<()> {
    let mut builder =
        TestNetworkBuilder::new().with_connection_limits_config(ConnectionLimitsConfig::new(1));
    let full_node = builder.build(vec![]).await;
    let filler = builder.build(vec![]).await;
    let peer = builder.build(vec![]).await;

    let (_full_node_server, full_node_rpc) = full_node.spawn_p2p_rpc().await?;
    let (_peer_server, peer_rpc) = peer.spawn_p2p_rpc().await?;
    let full_node_id = full_node.peer_id().await?;
    let full_node_addr = full_node.listen_multiaddr().await?;
    let peer_id = peer.peer_id().await?;

    // Take the full node's only connection slot.
    full_node_rpc.opp2p_connect_peer(filler.listen_multiaddr().await?).await?;

    // Protecting the full node on the dialer exempts it from the dialer's redial limit, so only
    // the full node's connection limit decides whether the connection succeeds.
    peer_rpc.opp2p_protect_peer(full_node_id.clone()).await?;
    peer_rpc
        .opp2p_connect_peer(full_node_addr.clone())
        .await
        .expect_err("full node must refuse an unprotected peer");

    full_node_rpc.opp2p_protect_peer(peer_id.clone()).await?;
    peer_rpc.opp2p_connect_peer(full_node_addr.clone()).await?;
    assert_eq!(full_node_rpc.opp2p_peers(true).await?.total_connected, 2);

    full_node_rpc.opp2p_unprotect_peer(peer_id.clone()).await?;
    full_node_rpc.opp2p_disconnect_peer(peer_id).await?;
    peer_rpc.opp2p_disconnect_peer(full_node_id).await?;
    peer_rpc
        .opp2p_connect_peer(full_node_addr)
        .await
        .expect_err("full node must refuse the peer once unprotected");

    Ok(())
}
