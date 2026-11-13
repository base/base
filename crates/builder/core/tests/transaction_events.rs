//! Integration tests for AT builder audit-event emission.

#![allow(missing_docs)]

use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, U256};
use alloy_provider::Provider;
use base_builder_core::{
    BuilderApiExtension, BuilderApiExtensionConfig, BuilderConfig, DEFAULT_MAX_VALIDITY_PREDICATES,
    test_utils::{
        ChainDriverExt, LocalInstanceBuilder, ONE_ETH, chain_spec_with_upgrade,
        default_node_config, node_config_with_chain_spec,
    },
};
use base_common_evm::BaseUpgrade;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    TransactionValidity, ValidatedTransaction, ValidityOperator, ValidityPredicate,
};
use base_observability_events::{TransactionEventCapture, TransactionEventType};
use reth_node_builder::NodeConfig;

fn validity_instance() -> LocalInstanceBuilder {
    LocalInstanceBuilder::new(BuilderConfig::for_tests()).install_ext::<BuilderApiExtension>(
        BuilderApiExtensionConfig::new(DEFAULT_MAX_VALIDITY_PREDICATES).with_noop_metering(),
    )
}

#[tokio::test]
async fn expired_position_predicate_emits_builder_expired() -> eyre::Result<()> {
    let capture = TransactionEventCapture::install();
    let instance = validity_instance().build().await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(1, ONE_ETH).await?;

    // A block-number upper bound on the already-mined head is admitted, then terminal on the
    // next payload build.
    let latest = driver.latest().await?;
    let expired = driver
        .create_transaction()
        .with_signer(&accounts[0])
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
                metering: None,
                extensions: TransactionValidity {
                    validity: vec![ValidityPredicate::BlockNumber {
                        op: ValidityOperator::LessThanOrEqual,
                        value: U256::from(latest.header.number),
                    }],
                    validity_signature: None,
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
        !expired_types.contains(&TransactionEventType::BuilderRejected),
        "expired transactions must not be labeled BUILDER_REJECTED, got {expired_types:?}"
    );

    Ok(())
}

/// Metered gas of a plain value transfer.
const TRANSFER_GAS: u64 = 21_000;
/// A transfer declaring far more gas than it meters.
const OVER_PROVISIONED_TRANSFER_GAS_LIMIT: u64 = 100_000;

/// The `tx_gas_limit`, `gas_used`, `metered_gas_used` and `payer_auth_gas_reserved` fields of
/// the `BUILDER_ACCEPTED` event for an over-provisioned transfer included by a builder running
/// `node_config`. The native builder journals accepted events only for validity-predicate
/// transactions, so the transfer carries a predicate that is already satisfied.
async fn accepted_transfer_gas(
    node_config: NodeConfig<BaseChainSpec>,
) -> eyre::Result<(u64, u64, u64, serde_json::Value)> {
    let capture = TransactionEventCapture::install();
    let instance = validity_instance().with_node_config(node_config).build().await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(1, ONE_ETH).await?;

    let tx = driver
        .create_transaction()
        .with_signer(&accounts[0])
        .with_to(Address::random())
        .with_gas_limit(OVER_PROVISIONED_TRANSFER_GAS_LIMIT)
        .build()
        .await;
    let tx_hash = tx.tx_hash();
    driver
        .provider()
        .raw_request::<_, ()>(
            "base_insertValidatedTransaction".into(),
            (ValidatedTransaction {
                sender: accounts[0].address(),
                raw: tx.encoded_2718().into(),
                metering: None,
                extensions: TransactionValidity {
                    validity: vec![ValidityPredicate::Balance {
                        address: accounts[0].address(),
                        op: ValidityOperator::GreaterThanOrEqual,
                        value: U256::ZERO,
                    }],
                    validity_signature: None,
                },
            },),
        )
        .await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    assert!(block.transactions.hashes().any(|hash| hash == tx_hash), "transfer must be included");

    let events = capture.events();
    let accepted = events
        .iter()
        .find(|event| {
            event.tx_hash == Some(tx_hash)
                && event.event_type == TransactionEventType::BuilderAccepted
        })
        .expect("an included validity-predicate transaction emits BUILDER_ACCEPTED");
    let gas_field =
        |name: &str| accepted.data[name].as_u64().unwrap_or_else(|| panic!("{name} must be a u64"));
    Ok((
        gas_field("tx_gas_limit"),
        gas_field("gas_used"),
        gas_field("metered_gas_used"),
        accepted.data["payer_auth_gas_reserved"].clone(),
    ))
}

/// Before gas-limit billing the accepted event's billed and metered gas are the same metered
/// figure, giving the data team a continuous `metered_gas_used` column across the activation.
#[tokio::test]
async fn builder_accepted_reports_metered_gas_equal_to_billed_gas_before_gas_limit_billing()
-> eyre::Result<()> {
    let (tx_gas_limit, gas_used, metered_gas_used, payer_auth) =
        accepted_transfer_gas(default_node_config()).await?;
    assert_eq!(tx_gas_limit, OVER_PROVISIONED_TRANSFER_GAS_LIMIT);
    assert_eq!(gas_used, TRANSFER_GAS);
    assert_eq!(metered_gas_used, TRANSFER_GAS);
    assert!(payer_auth.is_null(), "only EIP-8130 transactions report payer_auth_gas_reserved");
    Ok(())
}

/// Under gas-limit billing (`BaseFeature::GasLimitBilling`, enabled by Everest) the accepted
/// event bills the declared limit as `gas_used` while `metered_gas_used` keeps the gas the
/// transfer consumed, so one row relates limit, billed and metered gas.
#[tokio::test]
async fn builder_accepted_reports_metered_gas_below_billed_gas_under_gas_limit_billing()
-> eyre::Result<()> {
    let (tx_gas_limit, gas_used, metered_gas_used, payer_auth) = accepted_transfer_gas(
        node_config_with_chain_spec(chain_spec_with_upgrade(BaseUpgrade::Everest)),
    )
    .await?;
    assert_eq!(tx_gas_limit, OVER_PROVISIONED_TRANSFER_GAS_LIMIT);
    assert_eq!(gas_used, OVER_PROVISIONED_TRANSFER_GAS_LIMIT);
    assert_eq!(metered_gas_used, TRANSFER_GAS);
    assert!(payer_auth.is_null(), "only EIP-8130 transactions report payer_auth_gas_reserved");
    Ok(())
}

/// A builder whose gate is closed, as a sequencer that is not the active leader, journals
/// neither insert verdict it gives nor its build decisions; reopening the gate resumes all of
/// them. Each transaction is inserted twice, so the builder gives an accepted and a rejected
/// (already imported) verdict, and carries a satisfied predicate, since the native builder
/// journals only validity-gated candidates.
#[tokio::test]
async fn closed_gate_withholds_every_builder_event() -> eyre::Result<()> {
    let capture = TransactionEventCapture::install();
    let instance = validity_instance().build().await?;
    let driver = instance.driver().await?;
    let accounts = driver.fund_accounts(2, ONE_ETH).await?;

    let mut hashes = Vec::new();
    for (account, open) in [(0, false), (1, true)] {
        capture.gate().set_open(open);
        let tx = driver
            .create_transaction()
            .with_signer(&accounts[account])
            .with_to(Address::random())
            .with_value(1)
            .build()
            .await;
        hashes.push(tx.tx_hash());
        let insert = ValidatedTransaction {
            sender: accounts[account].address(),
            raw: tx.encoded_2718().into(),
            metering: None,
            extensions: TransactionValidity {
                validity: vec![ValidityPredicate::Balance {
                    address: accounts[account].address(),
                    op: ValidityOperator::NotEqual,
                    value: U256::ZERO,
                }],
                validity_signature: None,
            },
        };
        driver
            .provider()
            .raw_request::<_, ()>("base_insertValidatedTransaction".into(), (insert.clone(),))
            .await?;
        let duplicate = driver
            .provider()
            .raw_request::<_, ()>("base_insertValidatedTransaction".into(), (insert,))
            .await;
        assert!(duplicate.is_err(), "the second insert must be rejected as already imported");
        driver.build_new_block().await?;
    }
    let [withheld, journaled] = [hashes[0], hashes[1]];

    let events = capture.events();
    assert!(
        events.iter().all(|event| event.tx_hash != Some(withheld)),
        "a closed gate must withhold every event, got {:?}",
        events.iter().filter(|event| event.tx_hash == Some(withheld)).collect::<Vec<_>>()
    );
    let journaled_types: Vec<_> = events
        .iter()
        .filter(|event| event.tx_hash == Some(journaled))
        .map(|event| event.event_type)
        .collect();
    for expected in [
        TransactionEventType::TxpoolValidatedInsertAccepted,
        TransactionEventType::TxpoolValidatedInsertRejected,
        TransactionEventType::BuilderAccepted,
    ] {
        assert!(
            journaled_types.contains(&expected),
            "an open gate must journal {expected} again, got {journaled_types:?}"
        );
    }
    Ok(())
}
