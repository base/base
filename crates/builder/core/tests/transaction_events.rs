//! Integration tests for AT builder audit-event emission.

#![allow(missing_docs)]

use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    test_utils::{ChainDriverExt, LocalInstanceBuilder, ONE_ETH},
};
use base_execution_txpool::{
    TransactionValidity, ValidatedTransaction, ValidityOperator, ValidityPredicate,
};
use base_observability_events::{TransactionEventCapture, TransactionEventType};

fn validity_instance() -> LocalInstanceBuilder {
    LocalInstanceBuilder::new(BuilderConfig::for_tests()).install_ext::<BuilderApiExtension>(
        BuilderApiExtensionConfig::new(true, DEFAULT_MAX_VALIDITY_PREDICATES),
    )
}

#[tokio::test]
async fn recoverable_predicate_emits_builder_deferred() -> eyre::Result<()> {
    let capture = TransactionEventCapture::install();
    let instance = validity_instance().build().await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;
    let watched = Address::random();

    let gated = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(0)
        .with_to(Address::random())
        .with_max_priority_fee_per_gas(100)
        .build()
        .await;
    let gated_hash = gated.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[0].address(),
                raw: gated.encoded_2718().into(),
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
            },),
        )
        .await?;

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
    let included: Vec<_> = block
        .transactions
        .into_transactions()
        .filter_map(|transaction| {
            [gated_hash, trigger_hash]
                .contains(&transaction.tx_hash())
                .then(|| transaction.tx_hash())
        })
        .collect();
    assert_eq!(included, [trigger_hash, gated_hash]);

    let gated_types: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|event| event.tx_hash == Some(gated_hash))
        .map(|event| event.event_type)
        .collect();
    assert!(
        gated_types.contains(&TransactionEventType::BuilderDeferred),
        "recoverable predicate must emit BUILDER_DEFERRED, got {gated_types:?}"
    );
    assert!(
        !gated_types.contains(&TransactionEventType::BuilderRejected),
        "parked transactions must not be labeled BUILDER_REJECTED, got {gated_types:?}"
    );
    assert!(
        !gated_types.contains(&TransactionEventType::BuilderExpired),
        "recoverable predicates must not emit BUILDER_EXPIRED, got {gated_types:?}"
    );
    assert!(
        capture
            .events()
            .iter()
            .filter(|event| event.tx_hash == Some(gated_hash))
            .all(|event| !event.data.contains_key("validity_predicates")),
        "downstream builder events must not repeat validity_predicates"
    );

    Ok(())
}

#[tokio::test]
async fn expired_position_predicate_emits_builder_expired() -> eyre::Result<()> {
    let capture = TransactionEventCapture::install();
    let instance = validity_instance().build().await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(1, ONE_ETH).await?;

    let expired = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_nonce(0)
        .with_to(Address::random())
        .build()
        .await;
    let expired_hash = expired.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[0].address(),
                raw: expired.encoded_2718().into(),
                min_block_number: None,
                max_block_number: None,
                min_timestamp: None,
                max_timestamp: None,
                extensions: TransactionValidity {
                    validity: vec![ValidityPredicate::FlashblockIndex {
                        op: ValidityOperator::LessThan,
                        value: U256::ZERO,
                    }],
                },
            },),
        )
        .await?;

    let block = driver.build_new_block().await?;
    assert!(
        !block
            .transactions
            .into_transactions()
            .any(|transaction| transaction.tx_hash() == expired_hash),
        "expired position predicates must never be included"
    );

    let expired_types: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|event| event.tx_hash == Some(expired_hash))
        .map(|event| event.event_type)
        .collect();
    assert!(
        expired_types.contains(&TransactionEventType::BuilderExpired),
        "terminal position predicates must emit BUILDER_EXPIRED, got {expired_types:?}"
    );
    assert!(
        !expired_types.contains(&TransactionEventType::BuilderDeferred),
        "expired transactions must not be parked, got {expired_types:?}"
    );

    Ok(())
}
