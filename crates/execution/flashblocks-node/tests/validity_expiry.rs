//! Regression test for validity-transaction expiry on nodes that follow the flashblock stream
//! instead of building it (mempool and RPC nodes).
//!
//! A validity transaction whose flashblock deadline has been published must release its
//! same-nonce slot as soon as this node receives that flashblock, so an unbumped replacement is
//! admitted before the block commits. Without follower-side expiry the replacement is rejected as
//! underpriced against a transaction the builder already dropped.

use alloy_eips::Encodable2718;
use alloy_primitives::{B256, Bytes, TxHash, U256, hex::FromHex};
use alloy_provider::Provider;
use base_common_consensus::BaseTransactionSigned;
use base_execution_txpool::{ValidityOperator, ValidityPredicate};
use base_flashblocks_node::test_harness::{FlashblockBuilder, FlashblocksBuilderTestHarness};
use base_test_utils::Account;
use base_txpool_rpc::{SendRawTransactionValidityExtension, SendRawTransactionValidityOptions};
use reth_chainspec::EthChainSpec;
use reth_provider::ChainSpecProvider;
use reth_transaction_pool::test_utils::TransactionBuilder;

/// Last flashblock index in which the validity transaction may be included.
const DEADLINE_FLASHBLOCK: u64 = 1;

/// A transfer of `value` wei from Alice at `nonce` with fixed fees, so two calls with different
/// values are distinct transactions and the second is an unbumped replacement of the first.
fn alice_transfer(
    harness: &FlashblocksBuilderTestHarness,
    nonce: u64,
    value: u128,
) -> BaseTransactionSigned {
    let signer = B256::from_hex(Account::Alice.private_key()).expect("valid hex-encoded key");
    let tx = TransactionBuilder::default()
        .signer(signer)
        .chain_id(harness.provider.chain_spec().chain_id())
        .to(Account::Bob.address())
        .nonce(nonce)
        .value(value)
        .gas_limit(21_000)
        .max_fee_per_gas(2_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559()
        .as_eip1559()
        .expect("built as EIP-1559")
        .clone();
    BaseTransactionSigned::Eip1559(tx)
}

fn encoded(tx: &BaseTransactionSigned) -> Bytes {
    tx.encoded_2718().into()
}

async fn pooled(harness: &FlashblocksBuilderTestHarness, hash: TxHash) -> bool {
    harness
        .node
        .provider()
        .get_transaction_by_hash(hash)
        .await
        .expect("eth_getTransactionByHash succeeds")
        .is_some()
}

#[tokio::test]
async fn received_flashblock_deadline_releases_same_nonce_before_block_commits() {
    let harness = FlashblocksBuilderTestHarness::with_extensions(vec![Box::new(
        SendRawTransactionValidityExtension::default(),
    )])
    .await;
    let provider = harness.node.provider();
    let target_block = harness.node.latest_block().number + 1;

    // Alice's validity transaction may only land in `target_block` up to flashblock 1.
    let original = alice_transfer(&harness, 0, 1);
    let original_hash = *original.hash();
    let options = SendRawTransactionValidityOptions {
        validity: vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(target_block),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(DEADLINE_FLASHBLOCK),
            },
        ],
        validity_signature: None,
    };
    let accepted: TxHash = provider
        .raw_request("base_sendRawTransactionValidity".into(), (encoded(&original), options))
        .await
        .expect("validity transaction is admitted");
    assert_eq!(accepted, original_hash);
    assert!(pooled(&harness, original_hash).await);

    // Same nonce, same fees: a plain replacement that reth prices as underpriced while the
    // original still occupies the slot.
    let replacement = alice_transfer(&harness, 0, 2);
    let replacement_hash = *replacement.hash();
    let replacement_bytes = encoded(&replacement);

    // Flashblock 0 of the target block is published; the deadline has not passed yet.
    harness.send_flashblock(FlashblockBuilder::new_base(&harness).build()).await;
    let early = provider.send_raw_transaction(&replacement_bytes).await;
    assert!(
        early.as_ref().is_err_and(|error| error.to_string().contains("underpriced")),
        "unbumped replacement must still be blocked before the deadline: {early:?}"
    );
    assert!(pooled(&harness, original_hash).await);

    // Flashblock 1 is published without the original, so its deadline has passed. The block has
    // not committed on this node, yet the nonce slot must already be free.
    harness.send_flashblock(FlashblockBuilder::new(&harness, DEADLINE_FLASHBLOCK).build()).await;
    assert!(!pooled(&harness, original_hash).await, "expired original must leave the pool");
    let admitted = provider
        .send_raw_transaction(&replacement_bytes)
        .await
        .expect("unbumped replacement is admitted once the deadline flashblock is received");
    assert_eq!(*admitted.tx_hash(), replacement_hash);
    assert!(pooled(&harness, replacement_hash).await);
}
