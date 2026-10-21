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
