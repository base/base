//! Testing gossiping of transactions.
use std::sync::Arc;

use alloy_primitives::{Signature, U256};
use base_common_types_chain::TxLegacy;
use base_execution_txpool::{
    AddedTransactionOutcome, TransactionPool, test_utils::TransactionGenerator,
};
use futures::StreamExt;
use reth_network::{
    NetworkEvent, NetworkEventListenerProvider, Peers,
    test_utils::{NetworkEventStream, Testnet},
    transactions::config::{
        TransactionIngressPolicy, TransactionPropagationKind, TransactionsManagerConfig,
    },
};
use reth_network_api::{PeerKind, PeersInfo, events::PeerEvent};
use base_execution_state_provider::test_utils::{ExtendedAccount, MockEthProvider};
use tokio::join;

#[tokio::test(flavor = "multi_thread")]
async fn test_tx_gossip() {
    base_common_observability_tracing::init_test_tracing();

    let provider = MockEthProvider::default().with_genesis_block();
    let net = Testnet::create_with(2, provider.clone()).await;

    // install request handlers
    let net = net.with_eth_pool();
    let handle = net.spawn();
    // connect all the peers
    handle.connect_peers().await;

    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];

    let peer0_pool = peer0.pool().unwrap();
    let mut peer0_tx_listener = peer0.pool().unwrap().pending_transactions_listener();
    let mut peer1_tx_listener = peer1.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = reth_network::test_utils::NetworkTestData::transaction(tx_gen.gen_eip1559_pooled());

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert pending tx in peer0's pool
    let AddedTransactionOutcome { hash, .. } =
        peer0_pool.add_external_transaction(tx).await.unwrap();

    let inserted = peer0_tx_listener.recv().await.unwrap();
    assert_eq!(inserted, hash);

    // ensure tx is gossiped to peer1
    let received = peer1_tx_listener.recv().await.unwrap();
    assert_eq!(received, hash);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tx_propagation_policy_trusted_only() {
    base_common_observability_tracing::init_test_tracing();

    let provider = MockEthProvider::default().with_genesis_block();

    let policy = TransactionPropagationKind::Trusted;
    let net = Testnet::create_with(2, provider.clone()).await;
    let net = net.with_eth_pool_config_and_policy(Default::default(), policy);

    let handle = net.spawn();

    // connect all the peers
    handle.connect_peers().await;

    let peer_0_handle = &handle.peers()[0];
    let peer_1_handle = &handle.peers()[1];

    let mut peer0_tx_listener = peer_0_handle.pool().unwrap().pending_transactions_listener();
    let mut peer1_tx_listener = peer_1_handle.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = reth_network::test_utils::NetworkTestData::transaction(tx_gen.gen_eip1559_pooled());

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert the tx in peer0's pool
    let outcome_0 = peer_0_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();
    let inserted = peer0_tx_listener.recv().await.unwrap();

    assert_eq!(inserted, outcome_0.hash);

    // ensure tx is not gossiped to peer1
    peer1_tx_listener.try_recv().expect_err("Empty");

    let mut event_stream_0 = NetworkEventStream::new(peer_0_handle.network().event_listener());
    let mut event_stream_1 = NetworkEventStream::new(peer_1_handle.network().event_listener());

    // disconnect peer1 from peer0
    peer_0_handle.network().remove_peer(*peer_1_handle.peer_id(), PeerKind::Static);
    join!(event_stream_0.next_session_closed(), event_stream_1.next_session_closed());

    // re register peer1 as trusted
    peer_0_handle.network().add_trusted_peer(*peer_1_handle.peer_id(), peer_1_handle.local_addr());
    join!(event_stream_0.next_session_established(), event_stream_1.next_session_established());

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = reth_network::test_utils::NetworkTestData::transaction(tx_gen.gen_eip1559_pooled());

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert pending tx in peer0's pool
    let outcome_1 = peer_0_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();
    let inserted = peer0_tx_listener.recv().await.unwrap();
    assert_eq!(inserted, outcome_1.hash);

    // ensure peer1 now receives the pending txs from peer0
    let mut buff = Vec::with_capacity(2);
    buff.push(peer1_tx_listener.recv().await.unwrap());
    buff.push(peer1_tx_listener.recv().await.unwrap());

    assert!(buff.contains(&outcome_1.hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tx_ingress_policy_trusted_only() {
    base_common_observability_tracing::init_test_tracing();

    let provider = MockEthProvider::default().with_genesis_block();

    let tx_manager_config = TransactionsManagerConfig {
        ingress_policy: TransactionIngressPolicy::Trusted,
        ..Default::default()
    };

    let net = Testnet::create_with(2, provider.clone()).await;
    let net = net.with_eth_pool_config(tx_manager_config);

    let handle = net.spawn();

    // connect all the peers
    handle.connect_peers().await;

    let peer_0_handle = &handle.peers()[0];
    let peer_1_handle = &handle.peers()[1];

    let mut peer0_tx_listener = peer_0_handle.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = reth_network::test_utils::NetworkTestData::transaction(tx_gen.gen_eip1559_pooled());

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert the tx in peer1's pool
    let outcome_0 = peer_1_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();

    // ensure tx is not accepted by peer0
    peer0_tx_listener.try_recv().expect_err("Empty");

    let mut event_stream_0 = NetworkEventStream::new(peer_0_handle.network().event_listener());
    let mut event_stream_1 = NetworkEventStream::new(peer_1_handle.network().event_listener());

    // disconnect peer1 from peer0
    peer_0_handle.network().remove_peer(*peer_1_handle.peer_id(), PeerKind::Static);
    join!(event_stream_0.next_session_closed(), event_stream_1.next_session_closed());

    // re register peer1 as trusted
    peer_0_handle.network().add_trusted_peer(*peer_1_handle.peer_id(), peer_1_handle.local_addr());
    join!(event_stream_0.next_session_established(), event_stream_1.next_session_established());

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = reth_network::test_utils::NetworkTestData::transaction(tx_gen.gen_eip1559_pooled());

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert pending tx in peer1's pool
    let outcome_1 = peer_1_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();

    // ensure peer0 now receives both pending txs from peer1 (the blocked one and the new one)
    let mut buff = Vec::with_capacity(2);
    buff.push(peer0_tx_listener.recv().await.unwrap());
    buff.push(peer0_tx_listener.recv().await.unwrap());

    assert!(buff.contains(&outcome_0.hash));
    assert!(buff.contains(&outcome_1.hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_blob_transaction_gossip() {
    let provider = MockEthProvider::default().with_genesis_block();
    let net = Testnet::create_with(2, provider).await.with_eth_pool();
    let handle = net.spawn();
    handle.connect_peers().await;
    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];
    let mut events = peer1.network().event_listener();

    // Bypass the Base send API to simulate an incompatible peer's raw wire message.
    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let blob = tx_gen.gen_eip4844();
    let encoded = alloy_rlp::encode(base_execution_network_wire::Transactions(vec![blob]));
    peer0.network().send_eth_message(
        *peer1.peer_id(),
        reth_network::message::PeerMessage::Other(
            base_execution_network_wire::RawCapabilityMessage::eth(
                base_execution_network_wire::EthMessageID::Transactions,
                encoded.into(),
            ),
        ),
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.next().await {
            if let NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) = event {
                assert_eq!(peer_id, *peer0.peer_id());
                assert!(peer1.pool().unwrap().is_empty());
                return;
            }
        }
        panic!("network event stream ended before the incompatible peer disconnected");
    })
    .await
    .expect("blob gossip should disconnect the incompatible peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sending_invalid_transactions() {
    base_common_observability_tracing::init_test_tracing();
    let provider = MockEthProvider::default().with_genesis_block();
    let net = Testnet::create_with(2, provider.clone()).await;
    // install request handlers
    let net = net.with_eth_pool();

    let handle = net.spawn();

    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];

    // connect all the peers
    handle.connect_peers().await;

    assert_eq!(peer0.network().num_connected_peers(), 1);
    let mut peer1_events = peer1.network().event_listener();
    let mut tx_listener = peer1.pool().unwrap().new_transactions_listener();

    for idx in 0..10 {
        // send invalid txs to peer1
        let tx = TxLegacy {
            chain_id: None,
            nonce: idx,
            gas_price: 0,
            gas_limit: 0,
            to: Default::default(),
            value: Default::default(),
            input: Default::default(),
        };
        let tx = base_common_types_chain::BaseTxEnvelope::new_unhashed(
            tx.into(),
            Signature::test_signature(),
        );
        peer0.network().send_transactions(*peer1.peer_id(), vec![Arc::new(tx)]);
    }

    // await disconnect for bad tx spam
    if let Some(ev) = peer1_events.next().await {
        match ev {
            NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) => {
                assert_eq!(peer_id, *peer0.peer_id());
            }
            NetworkEvent::ActivePeerSession { .. }
            | NetworkEvent::Peer(PeerEvent::SessionEstablished { .. }) => {
                panic!("unexpected SessionEstablished event")
            }
            NetworkEvent::Peer(PeerEvent::PeerAdded(_)) => {
                panic!("unexpected PeerAdded event")
            }
            NetworkEvent::Peer(PeerEvent::PeerRemoved(_)) => {
                panic!("unexpected PeerRemoved event")
            }
        }
    }

    // ensure txs never made it to the pool
    assert!(tx_listener.try_recv().is_err());
}
