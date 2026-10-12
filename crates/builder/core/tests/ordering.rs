#![allow(missing_docs)]

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS, RestingPredicateMode, ShadowValidityConfig,
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

/// Shadow injection adds only builder-local metadata: the original signed transaction executes
/// with the same hash, encoding, and state transition.
#[tokio::test]
async fn shadow_validity_injection_preserves_forwarded_transaction() -> eyre::Result<()> {
    let shadow =
        ShadowValidityConfig::enabled(MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS).expect("valid rate");
    let api_config = BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES)
        .with_shadow_validity(shadow)
        .with_noop_metering();
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(api_config)
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(1, ONE_ETH).await?;
    let recipient = Address::random();
    let value = 7;
    let recipient_balance_before = driver.provider().get_balance(recipient).await?;

    let signed = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_to(recipient)
        .with_value(value)
        .build()
        .await;
    let tx_hash = signed.tx_hash();
    let raw = signed.encoded_2718();
    let forwarded = ValidatedTransaction {
        sender: accounts[0].address(),
        raw: raw.clone().into(),
        metering: None,
        extensions: TransactionValidity::default(),
    };
    driver
        .provider()
        .raw_request::<_, ()>("base_insertValidatedTransaction".into(), (forwarded,))
        .await?;

    let block = driver.build_new_block().await?;
    let included = block
        .transactions
        .into_transactions()
        .find(|transaction| transaction.tx_hash() == tx_hash)
        .expect("forwarded transaction must be included");

    assert_eq!(
        included.inner.inner.encoded_2718(),
        raw,
        "validity metadata must not alter consensus bytes"
    );
    assert_eq!(
        driver.provider().get_balance(recipient).await?,
        recipient_balance_before + U256::from(value),
        "the original state transition must execute"
    );

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

/// Inserts a transaction from `signer` conditioned on `validity` through the builder API.
async fn insert_validity_transaction(
    driver: &ChainDriver,
    signer: &PrivateKeySigner,
    priority_fee: u128,
    validity: Vec<ValidityPredicate>,
    fill_immediately_or_kill: bool,
) -> eyre::Result<TxHash> {
    let tx = driver
        .create_transaction()
        .with_signer(signer)
        .with_to(Address::random())
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

/// A storage predicate that holds for any unwritten slot, standing in for a shared opportunity.
fn untouched_slot(address: Address, slot: u64) -> ValidityPredicate {
    ValidityPredicate::Storage {
        address,
        slot: U256::from(slot),
        mask: U256::MAX,
        op: ValidityOperator::Equal,
        value: U256::ZERO,
    }
}

/// Builds one block containing a higher-priority validity transaction and a lower-priority one
/// whose predicates read `searcher_slot`, and returns whether the lower-priority one landed.
async fn searcher_included_after_competitor(
    searcher_slot: u64,
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

    let competitor = insert_validity_transaction(
        &driver,
        &accounts[0],
        100,
        vec![untouched_slot(pool, 1)],
        false,
    )
    .await?;
    let searcher = insert_validity_transaction(
        &driver,
        &accounts[1],
        50,
        vec![untouched_slot(pool, searcher_slot)],
        fill_immediately_or_kill,
    )
    .await?;

    let block = driver.build_new_block().await?;
    let included = block.transactions.hashes().collect::<Vec<_>>();
    assert!(included.contains(&competitor), "the winning bid must land");
    Ok(included.contains(&searcher))
}

/// A fill-immediately-or-kill transaction is dropped once a validity transaction conditioned on
/// the same state commits ahead of it, even though its own predicates still hold. Without the
/// flag the same transaction lands and pays gas for an opportunity that was already taken.
#[tokio::test]
async fn fill_immediately_or_kill_is_dropped_after_competitor_claims_its_state() -> eyre::Result<()>
{
    assert!(searcher_included_after_competitor(1, false).await?);
    assert!(!searcher_included_after_competitor(1, true).await?);
    Ok(())
}

/// Another validity transaction only kills a fill-immediately-or-kill transaction when their
/// predicates read the same state.
#[tokio::test]
async fn fill_immediately_or_kill_survives_unrelated_validity_commit() -> eyre::Result<()> {
    assert!(searcher_included_after_competitor(2, true).await?);
    Ok(())
}

/// With no competing claim, a fill-immediately-or-kill transaction parked on unmet state is still
/// included once a commit satisfies it.
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

/// A competitor committed in an earlier flashblock of the block still claims its state. The killed
/// transaction is evicted rather than retried, so it does not land in the next block even though its
/// predicates still hold; it is not cached as rejected, so resubmitting it is admitted again.
#[tokio::test]
async fn fill_immediately_or_kill_evicts_after_competitor_in_earlier_flashblock() -> eyre::Result<()>
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

    insert_validity_transaction(&driver, &accounts[0], 100, vec![untouched_slot(pool, 1)], false)
        .await?;
    let config = instance.builder_config();
    let block_number = driver.latest().await?.header.number + 1;
    let block_bound = ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(block_number + 2),
    };
    let searcher_tx = driver
        .create_transaction()
        .with_signer(&accounts[1])
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(200)
        .build()
        .await;
    let searcher = searcher_tx.tx_hash();
    let insert = |validity: Vec<ValidityPredicate>| {
        driver.provider().raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[1].address(),
                raw: searcher_tx.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity,
                    validity_signature: None,
                    fill_immediately_or_kill: true,
                },
            },),
        )
    };
    // The flashblock bound delays the searcher until after the competitor's flashblock.
    insert(vec![
        ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(config.flashblocks_per_block() - 1),
        },
        block_bound.clone(),
        untouched_slot(pool, 1),
    ])
    .await?;

    let contended = driver.build_new_block().await?;
    assert!(!contended.transactions.hashes().any(|hash| hash == searcher));
    assert!(instance.pool().is_dropped(searcher), "a killed bid must be evicted");

    let next = driver.build_new_block().await?;
    assert!(!next.transactions.hashes().any(|hash| hash == searcher));

    insert(vec![block_bound, untouched_slot(pool, 1)]).await?;
    let resubmitted = driver.build_new_block().await?;
    assert!(resubmitted.transactions.hashes().any(|hash| hash == searcher));
    Ok(())
}

/// A validity transaction that is parked rather than committed claims nothing.
#[tokio::test]
async fn parked_validity_transaction_does_not_claim_state() -> eyre::Result<()> {
    let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
        .install_ext::<BuilderApiExtension>(
            BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
        )
        .build()
        .await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let pool = Address::random();
    let never_funded = ValidityPredicate::Balance {
        address: Address::random(),
        op: ValidityOperator::GreaterThan,
        value: U256::ZERO,
    };

    let parked = insert_validity_transaction(
        &driver,
        &accounts[0],
        100,
        vec![untouched_slot(pool, 1), never_funded],
        false,
    )
    .await?;
    let searcher =
        insert_validity_transaction(&driver, &accounts[1], 50, vec![untouched_slot(pool, 1)], true)
            .await?;

    let block = driver.build_new_block().await?;
    let included = block.transactions.hashes().collect::<Vec<_>>();
    assert!(!included.contains(&parked));
    assert!(included.contains(&searcher));
    Ok(())
}
