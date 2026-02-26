#![allow(missing_docs)]

use base_builder_core::{
    BuilderConfig,
    test_utils::{
        BlockTransactionsExt, ChainDriverExt, ONE_ETH, default_node_config,
        setup_test_instance_with_node_config,
    },
};
use base_execution_chainspec::OpChainSpec;
use reth_node_builder::NodeConfig;
use reth_node_core::args::TxPoolArgs;

#[tokio::test]
async fn pending_pool_limit() -> eyre::Result<()> {
    let node_config = NodeConfig::<OpChainSpec> {
        txpool: TxPoolArgs { pending_max_count: 50, ..Default::default() },
        ..default_node_config()
    };
    let builder_config = BuilderConfig::for_tests();
    let rbuilder = setup_test_instance_with_node_config(builder_config, node_config).await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(50, ONE_ETH).await?;

    // send 50 txs from different addrs
    let accs_no_priority = accounts.clone();
    let accs_with_priority = accounts.into_iter().take(10).collect::<Vec<_>>();

    for i in 0..50 {
        let acc_no_priority = accs_no_priority.get(i).unwrap();
        let _ = driver.create_transaction().with_signer(acc_no_priority).send().await?;
    }

    assert_eq!(
        rbuilder.pool().pending_count(),
        50,
        "Pending pool must contain at max 50 txs {:?}",
        rbuilder.pool().pending_count()
    );

    // Send 10 txs that should be included in the block
    let mut txs = Vec::new();
    for i in 0..10 {
        let acc_with_priority = accs_with_priority.get(i).unwrap();
        let tx = driver
            .create_transaction()
            .with_signer(acc_with_priority)
            .with_max_priority_fee_per_gas(10)
            .send()
            .await?;
        txs.push(*tx.tx_hash());
    }

    assert_eq!(
        rbuilder.pool().pending_count(),
        50,
        "Pending pool must contain at max 50 txs {:?}",
        rbuilder.pool().pending_count()
    );

    // After we try building block our reverting tx would be removed and other tx will move to queue pool
    let block = driver.build_new_block().await?;

    // Ensure that 10 extra txs got included
    assert!(block.includes(&txs));

    Ok(())
}
