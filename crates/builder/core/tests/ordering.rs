#![allow(missing_docs)]

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    RestingPredicateMode,
    test_utils::{ChainDriver, ChainDriverExt, LocalInstanceBuilder, ONE_ETH, setup_test_instance},
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
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
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
        metering: None,
        extensions: TransactionValidity {
            validity: vec![ValidityPredicate::Balance {
                address: watched,
                op: ValidityOperator::Equal,
                value: U256::from_limbs([1, 0, 0, 0]),
            }],
            validity_signature: None,
            fill_immediately_or_kill: false,
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

/// A nonce-gated transaction wakes within the same block after the watched sender executes.
#[tokio::test]
async fn nonce_predicate_promotes_after_watched_sender_executes() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let watched_nonce = driver.provider().get_transaction_count(accounts[1].address()).await?;
    let gated = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let gated_hash = gated.tx_hash();
    let validated = ValidatedTransaction {
        sender: accounts[0].address(),
        raw: gated.encoded_2718().into(),
        metering: None,
        extensions: TransactionValidity {
            validity: vec![ValidityPredicate::Nonce {
                address: accounts[1].address(),
                op: ValidityOperator::Equal,
                value: U256::from(watched_nonce + 1),
            }],
            validity_signature: None,
            fill_immediately_or_kill: false,
        },
    };
    driver
        .provider()
        .raw_request::<_, ()>("base_insertValidatedTransaction".into(), (validated,))
        .await?;
    let trigger_hash = *driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [trigger_hash, gated_hash];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|tx| tracked.contains(&tx.tx_hash()).then(|| tx.tx_hash()))
        .collect::<Vec<_>>();
    assert_eq!(actual, tracked);
    Ok(())
}

/// Once a flashblock's validity-predicate evaluation time budget is exhausted, further
/// validity-gated transactions are deferred without evaluation rather than checked, even when
/// their predicate is already satisfied. An ordinary transaction is unaffected by the cutoff, so
/// it can be included in the same flashblock ahead of a higher-priority deferred transaction.
#[tokio::test]
async fn predicate_eval_hard_cutoff_defers_without_evaluating() -> eyre::Result<()> {
    let instance =
        LocalInstanceBuilder::new(BuilderConfig::for_tests().with_predicate_eval_hard_cutoff_ms(0))
            .install_ext::<BuilderApiExtension>(
                BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES)
                    .with_noop_metering(),
            )
            .build()
            .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;

    // Trivially satisfied for any real block number: proves deferral is due to the exhausted
    // budget, not an unsatisfied predicate.
    let always_satisfied = vec![ValidityPredicate::BlockNumber {
        op: ValidityOperator::GreaterThanOrEqual,
        value: U256::ZERO,
    }];

    let first = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let first_hash = first.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[0].address(),
                raw: first.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity: always_satisfied.clone(),
                    validity_signature: None,
                    fill_immediately_or_kill: false,
                },
            },),
        )
        .await?;

    let deferred = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(90)
        .build()
        .await;
    let deferred_hash = deferred.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[1].address(),
                raw: deferred.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity: always_satisfied,
                    validity_signature: None,
                    fill_immediately_or_kill: false,
                },
            },),
        )
        .await?;

    let ordinary_hash = *driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [first_hash, deferred_hash, ordinary_hash];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|transaction| {
            tracked.contains(&transaction.tx_hash()).then(|| transaction.tx_hash())
        })
        .collect::<Vec<_>>();

    // `first` is evaluated within budget and included immediately at its natural priority
    // position. `deferred` has higher priority than `ordinary` but is skipped by the exhausted
    // budget, so `ordinary` (unaffected by the cutoff) is included ahead of it; `deferred` is
    // still included overall, just in a later flashblock once the budget resets.
    assert_eq!(actual, [first_hash, ordinary_hash, deferred_hash]);

    Ok(())
}

/// With resting predicates enforced, a transaction whose predicate is unsatisfied is still
/// included in the block whose execution first satisfies it.
#[tokio::test]
async fn resting_transaction_is_included_once_its_blocking_state_changes() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(
        BuilderConfig::for_tests().with_resting_predicate_mode(RestingPredicateMode::Enforce),
    )
    .install_ext::<BuilderApiExtension>(
        BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
    )
    .build()
    .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let watched = Address::random();

    let resting = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let resting_hash = resting.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[0].address(),
                raw: resting.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity: vec![ValidityPredicate::Balance {
                        address: watched,
                        op: ValidityOperator::Equal,
                        value: U256::from(1),
                    }],
                    validity_signature: None,
                    fill_immediately_or_kill: false,
                },
            },),
        )
        .await?;

    let unsatisfied = driver.build_new_block().await?;
    assert!(!unsatisfied.transactions.hashes().any(|hash| hash == resting_hash));

    let trigger_hash = *driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(watched)
        .with_value(1)
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [trigger_hash, resting_hash];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|transaction| {
            tracked.contains(&transaction.tx_hash()).then(|| transaction.tx_hash())
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, tracked);

    Ok(())
}

/// Inserts a transaction from `signer` paying one wei to `to`, conditioned on `validity`, through
/// the builder API.
async fn insert_validity_transaction(
    driver: &ChainDriver,
    signer: &PrivateKeySigner,
    priority_fee: u128,
    to: Address,
    validity: Vec<ValidityPredicate>,
    fill_immediately_or_kill: bool,
) -> eyre::Result<TxHash> {
    let tx = driver
        .create_transaction()
        .with_signer(signer)
        .with_to(to)
        .with_value(1)
        .with_max_priority_fee_per_gas(priority_fee)
        .build()
        .await;
    let hash = tx.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: signer.address(),
                raw: tx.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity,
                    validity_signature: None,
                    fill_immediately_or_kill,
                },
            },),
        )
        .await?;
    Ok(hash)
}

/// A loose balance predicate that holds whatever is paid into `address`, standing in for a
/// searcher whose bounds do not capture the opportunity it is really after.
const fn loose_balance(address: Address) -> ValidityPredicate {
    ValidityPredicate::Balance { address, op: ValidityOperator::LessThan, value: U256::MAX }
}

/// Builds one block with a higher-priority competitor paying into `pool` and a lower-priority
/// searcher whose loose predicate reads `watched`, and returns whether the searcher landed.
async fn searcher_included_after_competitor(
    watches_pool: bool,
    fill_immediately_or_kill: bool,
) -> eyre::Result<bool> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let pool = Address::random();
    let watched = if watches_pool { pool } else { Address::random() };

    let competitor =
        insert_validity_transaction(&driver, &accounts[0], 100, pool, Vec::new(), false).await?;
    let searcher = insert_validity_transaction(
        &driver,
        &accounts[1],
        50,
        Address::random(),
        vec![loose_balance(watched)],
        fill_immediately_or_kill,
    )
    .await?;

    let block = driver.build_new_block().await?;
    let included = block.transactions.hashes().collect::<Vec<_>>();
    assert!(included.contains(&competitor), "the winning bid must land");
    Ok(included.contains(&searcher))
}

/// A fill-immediately-or-kill transaction whose predicates held at the start of the block is
/// dropped once an earlier transaction writes the state those predicates read, even though they
/// still hold. Without the flag the same transaction lands and pays gas for an opportunity that
/// was already taken.
#[tokio::test]
async fn fill_immediately_or_kill_is_dropped_once_its_predicate_state_is_written()
-> eyre::Result<()> {
    assert!(searcher_included_after_competitor(true, false).await?);
    assert!(!searcher_included_after_competitor(true, true).await?);
    Ok(())
}

/// Writes to state the predicates do not read never kill.
#[tokio::test]
async fn fill_immediately_or_kill_survives_unrelated_writes() -> eyre::Result<()> {
    assert!(searcher_included_after_competitor(false, true).await?);
    Ok(())
}

/// A transaction that reads the same state as a flagged transaction, but does not write it, is not
/// a conflict: kills are read-after-write only.
#[tokio::test]
async fn fill_immediately_or_kill_survives_a_competitor_that_only_reads_its_state()
-> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let pool = Address::random();

    let reader = insert_validity_transaction(
        &driver,
        &accounts[0],
        100,
        Address::random(),
        vec![loose_balance(pool)],
        false,
    )
    .await?;
    let searcher = insert_validity_transaction(
        &driver,
        &accounts[1],
        50,
        Address::random(),
        vec![loose_balance(pool)],
        true,
    )
    .await?;

    let block = driver.build_new_block().await?;
    let included = block.transactions.hashes().collect::<Vec<_>>();
    assert_eq!(included[included.len() - 2..], [reader, searcher]);
    Ok(())
}

/// With no competing write, a fill-immediately-or-kill transaction parked on unmet state is still
/// included once a commit satisfies it: the satisfying write is not a conflict.
#[tokio::test]
async fn fill_immediately_or_kill_fills_when_woken_without_competition() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let watched = Address::random();

    let gated = insert_validity_transaction(
        &driver,
        &accounts[0],
        100,
        Address::random(),
        vec![ValidityPredicate::Balance {
            address: watched,
            op: ValidityOperator::Equal,
            value: U256::from(1),
        }],
        true,
    )
    .await?;
    let trigger = *driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(watched)
        .with_value(1)
        .with_max_priority_fee_per_gas(50)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [trigger, gated];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|tx| tracked.contains(&tx.tx_hash()).then(|| tx.tx_hash()))
        .collect::<Vec<_>>();
    assert_eq!(actual, tracked);
    Ok(())
}

/// Two backrunners wake on the same trigger. The faster one pays into the watched balance again
/// before the flagged one executes, so the flagged one is dropped instead of landing late.
#[tokio::test]
async fn fill_immediately_or_kill_loses_a_backrun_race_without_paying() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(3, ONE_ETH).await?;
    let victim = Address::random();
    let paid = ValidityPredicate::Balance {
        address: victim,
        op: ValidityOperator::GreaterThanOrEqual,
        value: U256::from(1),
    };

    let fast =
        insert_validity_transaction(&driver, &accounts[0], 100, victim, vec![paid.clone()], false)
            .await?;
    let slow =
        insert_validity_transaction(&driver, &accounts[1], 50, Address::random(), vec![paid], true)
            .await?;
    let trigger = *driver
        .create_transaction()
        .with_signer(&accounts[2])
        .with_to(victim)
        .with_value(1)
        .with_max_priority_fee_per_gas(200)
        .send()
        .await?
        .tx_hash();

    let block = driver.build_new_block().await?;
    let tracked = [trigger, fast, slow];
    let actual = block
        .transactions
        .into_transactions()
        .filter_map(|tx| tracked.contains(&tx.tx_hash()).then(|| tx.tx_hash()))
        .collect::<Vec<_>>();
    assert_eq!(actual, [trigger, fast]);
    Ok(())
}

/// A flashblock bound delays when the predicates count as satisfied: a write before the bound is
/// not a conflict, so the transaction fills once the bound is reached.
#[tokio::test]
async fn fill_immediately_or_kill_counts_satisfaction_from_its_flashblock_bound() -> eyre::Result<()>
{
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let pool = Address::random();
    let config = instance.builder_config();

    let competitor =
        insert_validity_transaction(&driver, &accounts[0], 100, pool, Vec::new(), false).await?;
    let searcher = insert_validity_transaction(
        &driver,
        &accounts[1],
        200,
        Address::random(),
        vec![
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(config.flashblocks_per_block() - 1),
            },
            loose_balance(pool),
        ],
        true,
    )
    .await?;

    let block = driver.build_new_block().await?;
    let included = block.transactions.hashes().collect::<Vec<_>>();
    assert!(included.contains(&competitor));
    assert!(included.contains(&searcher));
    Ok(())
}

/// A killed transaction is evicted and cached as rejected, so neither the next block nor a mempool
/// re-forward of the same transaction lands it, even though its predicates still hold.
#[tokio::test]
async fn fill_immediately_or_kill_evicts_and_rejects_reforwards_of_a_killed_transaction()
-> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let pool = Address::random();

    insert_validity_transaction(&driver, &accounts[0], 100, pool, Vec::new(), false).await?;
    let block_number = driver.latest().await?.header.number + 1;
    let block_bound = ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(block_number + 2),
    };
    let searcher_tx = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(50)
        .build()
        .await;
    let searcher = searcher_tx.tx_hash();
    let insert = || {
        driver.provider().raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[1].address(),
                raw: searcher_tx.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity: vec![block_bound.clone(), loose_balance(pool)],
                    validity_signature: None,
                    fill_immediately_or_kill: true,
                },
            },),
        )
    };
    insert().await?;

    let contended = driver.build_new_block().await?;
    assert!(!contended.transactions.hashes().any(|hash| hash == searcher));
    assert!(instance.pool().is_dropped(searcher), "a killed bid must be evicted");

    let next = driver.build_new_block().await?;
    assert!(!next.transactions.hashes().any(|hash| hash == searcher));

    // A mempool node re-forwards transactions it still holds.
    insert().await?;
    let reforwarded = driver.build_new_block().await?;
    assert!(!reforwarded.transactions.hashes().any(|hash| hash == searcher));
    Ok(())
}
