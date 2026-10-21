use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use base_consensus_gossip::{ConnectionLimitsConfig, PeerTargets};
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

/// A node at its high-tide connection count still accepts peers, such as an operator's sibling
/// node, until it reaches the hard connection cap.
#[tokio::test(flavor = "multi_thread")]
async fn test_node_accepts_peers_above_high_tide_up_to_hard_cap() -> anyhow::Result<()> {
    let mut builder = TestNetworkBuilder::new()
        .with_connection_limits_config(ConnectionLimitsConfig::new(2))
        .with_peer_targets(PeerTargets {
            low: 1,
            high: 1,
            // Long enough that nothing is trimmed during the test.
            grace_period: Duration::from_secs(60),
            trim_interval: Duration::from_secs(1),
        });
    let full_node = builder.build(vec![]).await;
    let filler = builder.build(vec![]).await;
    let sibling = builder.build(vec![]).await;
    let stranger = builder.build(vec![]).await;

    let (_full_node_server, full_node_rpc) = full_node.spawn_p2p_rpc().await?;
    let (_sibling_server, sibling_rpc) = sibling.spawn_p2p_rpc().await?;
    let (_stranger_server, stranger_rpc) = stranger.spawn_p2p_rpc().await?;
    let full_node_addr = full_node.listen_multiaddr().await?;

    full_node_rpc.opp2p_connect_peer(filler.listen_multiaddr().await?).await?;

    sibling_rpc.opp2p_connect_peer(full_node_addr.clone()).await?;
    assert_eq!(full_node_rpc.opp2p_peers(true).await?.total_connected, 2);

    stranger_rpc
        .opp2p_connect_peer(full_node_addr)
        .await
        .expect_err("full node must refuse peers at the hard cap");

    Ok(())
}

/// A node at its high-tide connection count trims unprotected peers back to its low-tide peer
/// count, keeps protected peers, and then has room to accept a new peer.
#[tokio::test(flavor = "multi_thread")]
async fn test_full_node_trims_unprotected_peers() -> anyhow::Result<()> {
    let mut builder = TestNetworkBuilder::new()
        .with_connection_limits_config(ConnectionLimitsConfig::new(2))
        .with_peer_targets(PeerTargets {
            low: 1,
            high: 2,
            // Long enough for `opp2p_connectPeer` to observe the connection before it is trimmed.
            grace_period: Duration::from_secs(5),
            trim_interval: Duration::from_secs(1),
        });
    let full_node = builder.build(vec![]).await;
    let protected = builder.build(vec![]).await;
    let unprotected = builder.build(vec![]).await;
    let newcomer = builder.build(vec![]).await;

    let (_full_node_server, full_node_rpc) = full_node.spawn_p2p_rpc().await?;
    let (_newcomer_server, newcomer_rpc) = newcomer.spawn_p2p_rpc().await?;
    let protected_id = protected.peer_id().await?;

    full_node_rpc.opp2p_protect_peer(protected_id.clone()).await?;
    full_node_rpc.opp2p_connect_peer(protected.listen_multiaddr().await?).await?;
    full_node_rpc.opp2p_connect_peer(unprotected.listen_multiaddr().await?).await?;

    let peers = (async || {
        let peers = full_node_rpc.opp2p_peers(true).await?;
        anyhow::ensure!(peers.total_connected == 1, "full node has not trimmed yet");
        Ok(peers)
    })
    .retry(
        ExponentialBuilder::default()
            .without_max_times()
            .with_total_delay(Some(Duration::from_secs(30))),
    )
    .await?;
    assert!(peers.peers.contains_key(&protected_id), "protected peer must survive trimming");

    newcomer_rpc.opp2p_connect_peer(full_node.listen_multiaddr().await?).await?;

    Ok(())
}
