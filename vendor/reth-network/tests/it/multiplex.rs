//! Native ETH transport remains usable when a peer config lists unsupported capabilities.

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use base_execution_network_wire::Capability;
use base_execution_network_wire::EthVersion;
use base_execution_network_wire::Protocol;
use reth_network::{
    BlockDownloaderProvider,
    test_utils::{PeerConfig, TestPool, Testnet},
};
use reth_network_api::Peers;
use reth_network_p2p::bodies::client::BodiesClient;
use base_execution_state_provider::test_utils::MockEthProvider;

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_protocols_are_not_announced_and_eth_requests_work_both_directions() {
    let provider = Arc::new(MockEthProvider::default());
    let mut net: Testnet<_, TestPool> = Testnet::default();
    let extra = Capability::new_static("extra", 1);
    for _ in 0..2 {
        net.add_peer_with_config(PeerConfig::with_protocols(
            provider.clone(),
            vec![EthVersion::Eth71.into(), Protocol::new(extra.clone(), 1)],
        ))
        .await
        .unwrap();
    }
    net.for_each_mut(|peer| {
        peer.install_request_handler();
    });
    let net = net.spawn();
    net.connect_peers().await;
    for peer in net.peers() {
        let peers = peer.network().get_all_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].capabilities.capabilities().iter().all(|cap| cap != &extra));
        let fetch = peer.network().fetch_client().await.unwrap();
        let bodies =
            tokio::time::timeout(Duration::from_secs(5), fetch.get_block_bodies(vec![B256::ZERO]))
                .await
                .expect("ETH request should not hang")
                .unwrap()
                .into_data();
        assert!(bodies.is_empty());
    }
}
