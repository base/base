//! Production payload construction includes queued transactions when their nonce gap closes.

use std::sync::Arc;

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, TxKind};
use base_common_chain_config::BaseChainSpecBuilder;
use base_common_client_ethereum::TxSignerSync;
use base_common_runtime_tasks::Runtime;
use base_common_types_chain::{SignableTransaction, Transaction, TxEip1559};
use base_node_core::NodeConfig;
use base_execution_state_database::test_utils::create_test_rw_db_with_path;
use reth_e2e_test_utils::{
    BaseNodeTestUtils, node::NodeTestContext, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_core::args::DatadirArgs;
use tokio::sync::Mutex;

#[tokio::test(flavor = "multi_thread")]
async fn test_queued_transaction_included_after_nonce_gap_closes() {
    base_common_observability_tracing::init_test_tracing();

    let genesis: Genesis = BaseNodeTestUtils::genesis();
    let chain_spec =
        Arc::new(BaseChainSpecBuilder::base_mainnet().genesis(genesis).ecotone_activated().build());

    // This wallet is going to send:
    // 1. L1 block info tx
    // 2. End-of-block custom tx
    let wallet = Arc::new(Mutex::new(Wallet::default().with_chain_id(chain_spec.chain().into())));

    // Configure and launch the node.
    let mut config =
        NodeConfig::new(chain_spec).with_unused_ports().with_datadir_args(DatadirArgs {
            datadir: base_execution_state_database::test_utils::tempdir_path().into(),
            ..Default::default()
        });
    config.network.discovery.discv5_port = Some(0);
    config.network.discovery.discv5_port_ipv6 = Some(0);
    let db = create_test_rw_db_with_path(
        config
            .datadir
            .datadir
            .unwrap_or_chain_default(config.chain.chain(), config.datadir.clone())
            .db(),
    );
    let runtime = Runtime::test();
    let node_handle = base_node_core::NodeLaunch::new(config.clone(), db, runtime.clone())
        .launch()
        .await
        .expect("Failed to launch node");

    let sender = Wallet::default().inner;
    let mut transfer = TxEip1559 {
        chain_id: config.chain.chain_id(),
        nonce: 1,
        gas_limit: 21_000,
        max_fee_per_gas: 20_000_000_000,
        to: TxKind::Call(Address::random()),
        ..Default::default()
    };
    let signature = sender.sign_transaction_sync(&mut transfer).unwrap();
    let transfer =
        base_common_types_chain::BaseTxEnvelope::Eip1559(transfer.into_signed(signature));
    let mut node = NodeTestContext::new(node_handle.node, BaseNodeTestUtils::payload_attributes)
        .await
        .unwrap();
    node.rpc.inject_tx(transfer.encoded_2718().into()).await.unwrap();
    let block_payloads = node
        .advance(1, |_| {
            let wallet = Arc::clone(&wallet);
            Box::pin(async move {
                let mut wallet = wallet.lock().await;
                let tx_fut = TransactionTestContext::optimism_l1_block_info_tx(
                    wallet.chain_id,
                    wallet.inner.clone(),
                    // This doesn't matter in the current test (because it's only one block),
                    // but make sure you're not reusing the nonce from end-of-block tx
                    // if they have the same signer.
                    wallet.inner_nonce * 2,
                );
                wallet.inner_nonce += 1;
                tx_fut.await
            })
        })
        .await
        .unwrap();
    assert_eq!(block_payloads.len(), 1);
    let block_payload = block_payloads.first().unwrap();
    let block = block_payload.block();
    assert_eq!(block.body().transactions.len(), 2); // L1 block info tx + end-of-block custom tx

    // Check that last transaction in the block looks like a transfer to a random address.
    let end_of_block_tx = block.body().transactions.last().unwrap();
    let Some(tx) = end_of_block_tx.as_eip1559() else {
        panic!("expected EIP-1559 transaction");
    };
    assert_eq!(tx.tx().nonce(), 1);
    assert_eq!(tx.tx().gas_limit(), 21_000);
    assert!(tx.tx().input().is_empty());
}
