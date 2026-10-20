//! Transaction events journaled by the forwarding pipeline, end to end.
//!
//! A mempool node with txpool tracing and forwarding sends to builder nodes serving the real
//! `base_insertValidatedTransaction` RPC. Each test submits one transaction and asserts the exact
//! set of events journaled for it by every node.

use std::{sync::Arc, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Bytes, TxHash, keccak256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, DEFAULT_MAX_VALIDITY_PREDICATES, TransactionValidity,
};
use base_node_runner::{BaseNodeExtension, NodeHooks, test_utils::TestHarness};
use base_observability_events::TransactionEventCapture;
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis};
use base_tx_forwarding::{TxForwardingConfig, TxForwardingExtension};
use base_txpool_rpc::{SendRawTransactionValidityConfig, SendRawTransactionValidityExtension};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};
use eyre::Result;
use url::Url;

/// Longest a test waits for the expected journal.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the journal must stay unchanged after it matches, so late extra events are caught.
const SETTLE: Duration = Duration::from_millis(300);

/// Long enough that no resend happens while a test runs.
const RESEND_AFTER_MS: u64 = 60_000;

/// Serves `base_insertValidatedTransaction` the way a builder does.
#[derive(Debug)]
struct BuilderRpcExtension;

impl BaseNodeExtension for BuilderRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(|ctx| {
            let api = BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                ctx.pool().clone(),
                true,
                DEFAULT_MAX_VALIDITY_PREDICATES,
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

async fn builder() -> Result<(TestHarness, Url)> {
    let harness = TestHarness::builder()
        .with_extension(BuilderRpcExtension)
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(build_test_genesis())))
        .build()
        .await?;
    let url = harness.rpc_url().parse()?;
    Ok((harness, url))
}

/// A mempool node configured as in production: txpool tracing, RPC admission events and
/// forwarding to `builder_urls`.
async fn mempool(builder_urls: Vec<Url>) -> Result<TestHarness> {
    TestHarness::builder()
        .with_ext::<TxPoolExtension>(TxpoolConfig {
            tracing_enabled: true,
            tracing_logs_enabled: false,
            transaction_event_node_role: None,
            flashblocks_config: None,
        })
        .with_ext::<SendRawTransactionValidityExtension>(SendRawTransactionValidityConfig::default())
        .with_ext::<TxForwardingExtension>(
            TxForwardingConfig::new(builder_urls).with_resend_after_ms(RESEND_AFTER_MS),
        )
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(build_test_genesis())))
        .build()
        .await
}

/// A URL nothing listens on.
fn unreachable_url() -> Result<Url> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(format!("http://{}", listener.local_addr()?).parse()?)
}

fn signed_transfer(account: Account, nonce: u64) -> Bytes {
    let transaction = BaseTransactionRequest::default()
        .from(account.address())
        .transaction_type(2u8)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(DEVNET_CHAIN_ID)
        .to(Account::Bob.address())
        .with_nonce(nonce)
        .build_typed_tx()
        .expect("valid transaction request");
    let signature = account
        .signer()
        .sign_hash_sync(&transaction.signature_hash())
        .expect("test account should sign transaction");
    transaction.into_signed(signature).encoded_2718().into()
}

/// The events journaled for `tx_hash` by any node, as sorted `PRODUCER EVENT_TYPE` entries.
/// Forwarder events also carry their RPC attempt.
fn journal(capture: &TransactionEventCapture, tx_hash: TxHash) -> Vec<String> {
    let mut entries: Vec<String> = capture
        .events()
        .into_iter()
        .filter(|event| event.tx_hash == Some(tx_hash))
        .map(|event| match event.data.get("attempt") {
            Some(attempt) => format!("{} {} attempt={attempt}", event.producer, event.event_type),
            None => format!("{} {}", event.producer, event.event_type),
        })
        .collect();
    entries.sort();
    entries
}

/// Waits until the journal for `tx_hash` equals `expected`, then checks that nothing else arrives.
async fn assert_journal(capture: &TransactionEventCapture, tx_hash: TxHash, expected: &[&str]) {
    let mut expected: Vec<String> = expected.iter().map(ToString::to_string).collect();
    expected.sort();
    let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
    while journal(capture, tx_hash) != expected && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(SETTLE).await;
    assert_eq!(journal(capture, tx_hash), expected);
}

#[tokio::test]
async fn forwarded_transaction_is_journaled_once_by_the_node_and_once_per_builder() -> Result<()> {
    let capture = TransactionEventCapture::install();
    let (_first, first_url) = builder().await?;
    let (_second, second_url) = builder().await?;
    let mempool = mempool(vec![first_url, second_url]).await?;

    let raw = signed_transfer(Account::Alice, 0);
    let _pending = mempool.provider().send_raw_transaction(&raw).await?;

    assert_journal(
        &capture,
        keccak256(&raw),
        &[
            "base-reth-node TXPOOL_SEND_RAW_TRANSACTION",
            "base-reth-node TXPOOL_PENDING",
            "base-builder TXPOOL_VALIDATED_INSERT_ACCEPTED",
            "base-builder TXPOOL_VALIDATED_INSERT_ACCEPTED",
        ],
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn unreachable_builder_journals_each_retry_and_the_drop() -> Result<()> {
    let capture = TransactionEventCapture::install();
    let (_healthy, healthy_url) = builder().await?;
    let mempool = mempool(vec![healthy_url, unreachable_url()?]).await?;

    let raw = signed_transfer(Account::Charlie, 0);
    let _pending = mempool.provider().send_raw_transaction(&raw).await?;

    assert_journal(
        &capture,
        keccak256(&raw),
        &[
            "base-reth-node TXPOOL_SEND_RAW_TRANSACTION",
            "base-reth-node TXPOOL_PENDING",
            "base-builder TXPOOL_VALIDATED_INSERT_ACCEPTED",
            "base-reth-node TXPOOL_BUILDER_FORWARD_ATTEMPT attempt=1",
            "base-reth-node TXPOOL_BUILDER_FORWARD_ATTEMPT attempt=2",
            "base-reth-node TXPOOL_BUILDER_FORWARD_ATTEMPT attempt=3",
            "base-reth-node TXPOOL_BUILDER_FORWARD_DROPPED attempt=3",
        ],
    )
    .await;
    Ok(())
}

/// A builder that already holds the transaction, as after an earlier send, rejects the insert.
/// The builder journals the rejection and the node journals the failed send.
#[tokio::test]
async fn builder_rejection_is_journaled_by_both_sides() -> Result<()> {
    let (builder, url) = builder().await?;
    let raw = signed_transfer(Account::Deployer, 0);
    let _pending = builder.provider().send_raw_transaction(&raw).await?;
    // Installed after seeding the builder, which clears the seeding's admission event.
    let capture = TransactionEventCapture::install();
    let mempool = mempool(vec![url]).await?;

    let _pending = mempool.provider().send_raw_transaction(&raw).await?;

    assert_journal(
        &capture,
        keccak256(&raw),
        &[
            "base-reth-node TXPOOL_SEND_RAW_TRANSACTION",
            "base-reth-node TXPOOL_PENDING",
            "base-builder TXPOOL_VALIDATED_INSERT_REJECTED",
            "base-reth-node TXPOOL_BUILDER_FORWARD_FAILURE attempt=0",
        ],
    )
    .await;
    Ok(())
}

/// Only pending transactions are forwarded: one behind a nonce gap waits in the queued subpool
/// and is forwarded once the gap is filled.
#[tokio::test]
async fn queued_transaction_is_forwarded_once_its_nonce_gap_fills() -> Result<()> {
    let capture = TransactionEventCapture::install();
    let (_builder, url) = builder().await?;
    let mempool = mempool(vec![url]).await?;

    let gapped = signed_transfer(Account::Bob, 1);
    let _pending = mempool.provider().send_raw_transaction(&gapped).await?;
    assert_journal(
        &capture,
        keccak256(&gapped),
        &["base-reth-node TXPOOL_SEND_RAW_TRANSACTION", "base-reth-node TXPOOL_QUEUED"],
    )
    .await;

    let _pending =
        mempool.provider().send_raw_transaction(&signed_transfer(Account::Bob, 0)).await?;
    assert_journal(
        &capture,
        keccak256(&gapped),
        &[
            "base-reth-node TXPOOL_SEND_RAW_TRANSACTION",
            "base-reth-node TXPOOL_QUEUED",
            "base-reth-node TXPOOL_QUEUED_TO_PENDING",
            "base-builder TXPOOL_VALIDATED_INSERT_ACCEPTED",
        ],
    )
    .await;
    Ok(())
}
