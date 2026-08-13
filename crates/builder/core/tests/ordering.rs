#![allow(missing_docs)]

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderApiExtension, BuilderConfig,
    test_utils::{ChainDriverExt, LocalInstanceBuilder, ONE_ETH, setup_test_instance},
};
use base_execution_txpool::{
    TransactionValidity, ValidatedTransaction, ValidityOperator, ValidityPredicate,
};
use futures::{StreamExt, future::join_all, stream};

/// This test ensures that the transactions are ordered by fee priority within each flashblock.
/// We expect breaks in global ordering that align with flashblock boundaries.
#[tokio::test]
async fn fee_priority_ordering() -> eyre::Result<()> {
    let rbuilder = setup_test_instance().await?;
    let driver = rbuilder.driver().await?;
    let accounts = driver.fund_accounts(10, ONE_ETH).await?;

    let latest_block = driver.latest().await?;
    let base_fee = latest_block
        .header
        .base_fee_per_gas
        .expect("Base fee should be present in the latest block");

    // generate transactions with randomized tips
    let txs = join_all(accounts.iter().map(|signer| {
        driver
            .create_transaction()
            .with_signer(signer)
            .with_max_priority_fee_per_gas(rand::random_range(1..50))
            .send()
    }))
    .await
    .into_iter()
    .collect::<eyre::Result<Vec<_>>>()?
    .into_iter()
    .map(|tx| *tx.tx_hash())
    .collect::<Vec<_>>();

    driver.build_new_block().await?;

    // verify all transactions are included in the block
    assert!(
        stream::iter(txs.iter())
            .all(|tx_hash| async {
                driver
                    .latest_full()
                    .await
                    .expect("Failed to fetch latest block")
                    .transactions
                    .hashes()
                    .any(|hash| hash == *tx_hash)
            })
            .await,
        "not all transactions included in the block"
    );

    let config = rbuilder.builder_config();
    let flashblocks_per_block = config.flashblocks_per_block();

    // verify user transactions are fee-ordered within each flashblock boundary
    let tips_in_block_order = driver
        .latest_full()
        .await?
        .into_transactions_vec()
        .into_iter()
        .filter_map(|tx| {
            if txs.contains(&tx.tx_hash()) {
                Some(tx.effective_tip_per_gas(base_fee as u64))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let breaks = tips_in_block_order.windows(2).filter(|pair| pair[0] < pair[1]).count();

    assert!(
        (breaks as u64) <= flashblocks_per_block,
        "Observed more ordering resets than flashblocks_per_block (breaks={breaks}, flashblocks_per_block={flashblocks_per_block})"
    );

    Ok(())
}

/// A high-priority lane may execute after the lower-priority transaction that satisfies its
/// predicate, while retaining priority over lower-priority work once it becomes valid.
#[tokio::test]
async fn predicates_delay_priority_without_blocking_nonce_descendants() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(true)
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(4, ONE_ETH).await?;
    let watched = Address::random();

    let parent = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let parent_hash = parent.tx_hash();
    let validated_parent = ValidatedTransaction {
        sender: accounts[0].address(),
        raw: parent.encoded_2718().into(),
        min_block_number: None,
        max_block_number: None,
        min_timestamp: None,
        max_timestamp: None,
        extensions: TransactionValidity {
            validity: vec![ValidityPredicate::Balance {
                address: watched,
                op: ValidityOperator::Equal,
                value: U256::from_limbs([1, 0, 0, 0]),
            }],
        },
    };
    driver
        .provider()
        .raw_request::<_, ()>("base_insertValidatedTransaction".into(), (validated_parent,))
        .await?;

    let child_hash = *driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(1)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(90)
        .send()
        .await?
        .tx_hash();
    let trigger_hash = *driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(watched)
        .with_value(1)
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?
        .tx_hash();
    let low_hash = *driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(1)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [trigger_hash, parent_hash, child_hash, low_hash];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|transaction| {
            tracked.contains(&transaction.tx_hash()).then(|| transaction.tx_hash())
        })
        .collect::<Vec<_>>();

    assert_eq!(actual, tracked);
    assert!(
        [50u128, 100, 90, 1].windows(2).any(|fees| fees[0] < fees[1]),
        "predicate-delayed order must intentionally violate descending priority fee order"
    );

    Ok(())
}
