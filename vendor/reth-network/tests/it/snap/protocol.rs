//! Native SNAP transport ignores unsupported satellite capabilities.

use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use base_execution_network_wire::Capability;
use base_execution_network_wire::EthVersion;
use base_execution_network_wire::GetAccountRangeMessage;
use base_execution_network_wire::Protocol;
use base_execution_network_wire::{SnapClient, SnapResponse};
use base_execution_state_provider::test_utils::MockEthProvider;
use reth_network::{
    BlockDownloaderProvider,
    eth_requests::SOFT_RESPONSE_LIMIT,
    test_utils::{PeerConfig, TestPool, Testnet},
};

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_satellite_does_not_disable_native_snap_requests() {
    base_common_observability_tracing::init_test_tracing();

    let les_protocol = Protocol::new(Capability::new_static("les", 1), 1);
    let protocols = vec![EthVersion::Eth71.into(), Protocol::snap_2(), les_protocol.clone()];

    let provider = Arc::new(MockEthProvider::default());
    let mut net: Testnet<_, TestPool> = Testnet::default();
    for _ in 0..2 {
        let peer = PeerConfig::with_protocols(provider.clone(), protocols.clone());
        net.add_peer_with_config(peer).await.unwrap();
    }
    net.for_each_mut(|peer| {
        peer.install_request_handler();
    });
    let net = net.spawn();
    net.connect_peers().await;
    let fetch = net.peers()[0].network().fetch_client().await.unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fetch.get_account_range(GetAccountRangeMessage {
            request_id: 51,
            root_hash: B256::ZERO,
            starting_hash: B256::ZERO,
            limit_hash: B256::repeat_byte(0xff),
            response_bytes: SOFT_RESPONSE_LIMIT as u64,
        }),
    )
    .await
    .expect("request should not hang");

    assert!(matches!(result.unwrap().into_data(), SnapResponse::AccountRange(_)));
}
