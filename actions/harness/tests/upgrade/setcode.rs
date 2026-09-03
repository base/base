//! Action tests for EIP-7702 `SetCode` transaction handling.

use alloy_consensus::TxReceipt;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, L2Sequencer,
    SharedL1Chain, TEST_ACCOUNT_ADDRESS, TestAccount, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_protocol::{SpanBatchError, SpanBatchTransactions, SpanDecodingError};
use base_test_utils::Account;

/// Runtime that stores `1` at slot 0, then stops.
///
/// Initcode copies that 6-byte runtime to memory and returns it.
const STORE_ONE_INITCODE: [u8; 18] = hex!("6006600c60003960066000f3600160005500");

const CREATE_GAS: u64 = 3_000_000; // matches `Account::create_deployment_tx`
const SETCODE_GAS: u64 = 1_000_000;

fn calldata_batcher() -> BatcherConfig {
    BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    }
}

const fn tx_kind(tx: &BaseTxEnvelope) -> &'static str {
    match tx {
        BaseTxEnvelope::Legacy(_) => "legacy",
        BaseTxEnvelope::Eip2930(_) => "eip2930",
        BaseTxEnvelope::Eip1559(_) => "eip1559",
        BaseTxEnvelope::Eip7702(_) => "eip7702",
        BaseTxEnvelope::Deposit(_) => "deposit",
        BaseTxEnvelope::Eip8130(_) => "eip8130",
    }
}

fn deposit_count(block: &BaseBlock) -> usize {
    block.body.transactions.iter().take_while(|tx| matches!(tx, BaseTxEnvelope::Deposit(_))).count()
}

fn user_tx_succeeded(
    sequencer: &L2Sequencer,
    block: &BaseBlock,
    user_tx_index: usize,
    label: &str,
) -> bool {
    let deposit_count = deposit_count(block);
    let receipts = sequencer
        .receipts_at(block.header.number)
        .unwrap_or_else(|| panic!("receipts must exist for L2 block {}", block.header.number));
    let receipt_count = receipts.len();
    receipts
        .into_iter()
        .nth(deposit_count + user_tx_index)
        .unwrap_or_else(|| {
            panic!(
                "{label}: user tx receipt {user_tx_index} must exist (deposits={deposit_count}, receipts={receipt_count}, txs={:?})",
                block.body.transactions.iter().map(tx_kind).collect::<Vec<_>>(),
            )
        })
        .status()
}

/// Keep the block's deposit prefix and replace every user transaction.
///
/// Used to put an EIP-7702 envelope into a pre-Isthmus batch: the Holocene EL
/// will not execute type-4 txs (Prague is scheduled with Isthmus), but the
/// derivation drop path only needs the encoded type byte in the batch.
fn with_user_txs(mut block: BaseBlock, user_txs: Vec<BaseTxEnvelope>) -> BaseBlock {
    block.body.transactions.truncate(deposit_count(&block));
    block.body.transactions.extend(user_txs);
    block
}

fn create_store_one_tx(sequencer: &L2Sequencer, chain_id: u64) -> (Address, BaseTxEnvelope) {
    let account = sequencer.test_account();
    let mut account = account.lock().expect("test account lock");
    let address = account.address().create(account.nonce());
    let tx = account.create_tx(
        chain_id,
        TxKind::Create,
        Bytes::from_static(&STORE_ONE_INITCODE),
        U256::ZERO,
        CREATE_GAS,
    );
    (address, tx)
}

fn create_self_setcode_tx(
    sequencer: &L2Sequencer,
    chain_id: u64,
    delegate: Address,
) -> BaseTxEnvelope {
    let account = sequencer.test_account();
    let mut account = account.lock().expect("test account lock");
    let authorization = account.sign_authorization(chain_id, delegate, account.nonce() + 1);
    account.create_eip7702_tx(
        chain_id,
        TEST_ACCOUNT_ADDRESS,
        Bytes::new(),
        SETCODE_GAS,
        vec![authorization],
    )
}

fn create_call_delegated_tx(chain_id: u64) -> BaseTxEnvelope {
    let mut bob = TestAccount::new(Account::Bob.signer_b256());
    bob.create_tx(
        chain_id,
        TxKind::Call(TEST_ACCOUNT_ADDRESS),
        Bytes::new(),
        U256::ZERO,
        SETCODE_GAS,
    )
}

// ---------------------------------------------------------------------------
// A. Post-Isthmus SetCode delegation deployment and execution
// ---------------------------------------------------------------------------

/// After Isthmus, a `SetCode` transaction can install a delegation on the sender
/// EOA and a later call to that EOA executes the delegated contract.
///
/// Flow:
///   L2 block 1: CREATE a contract whose runtime stores `1` at slot 0
///   L2 block 2: `SetCode` — sender authorizes the new contract
///   L2 block 3: CALL the sender EOA from a second account, which now runs the delegated runtime
///
/// The sequencer state is checked directly (delegation code + storage), then
/// the same blocks are derived so the verifier accepts the `SetCode` batch.
#[tokio::test]
async fn setcode_delegation_deploys_and_executes_after_isthmus() {
    let batcher_cfg = calldata_batcher();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_isthmus().build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let (delegate, create_tx) = create_store_one_tx(&sequencer, chain_id);
    let create_block = sequencer.build_next_block_with_transactions(vec![create_tx]).await;
    assert!(user_tx_succeeded(&sequencer, &create_block, 0, "CREATE"), "CREATE must succeed");
    assert!(sequencer.has_code(delegate), "created contract must have code");

    let setcode_block = sequencer
        .build_next_block_with_transactions(vec![create_self_setcode_tx(
            &sequencer, chain_id, delegate,
        )])
        .await;
    assert!(user_tx_succeeded(&sequencer, &setcode_block, 0, "SetCode"), "SetCode must succeed");
    assert!(
        sequencer.has_code(TEST_ACCOUNT_ADDRESS),
        "SetCode must install delegation code on the sender EOA"
    );

    let call_block = sequencer
        .build_next_block_with_transactions(vec![create_call_delegated_tx(chain_id)])
        .await;
    assert!(user_tx_succeeded(&sequencer, &call_block, 0, "CALL"), "delegated CALL must succeed");
    assert_eq!(
        sequencer.storage_at(TEST_ACCOUNT_ADDRESS, U256::ZERO),
        U256::from(1),
        "delegated runtime must SSTORE 1 at slot 0 of the EOA"
    );

    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg);
    for block in [create_block, setcode_block, call_block] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 3, "all three post-Isthmus SetCode blocks must derive");
    let counts = node.derived_user_tx_counts();
    let user_txs = |n: u64| counts.iter().find(|(bn, _)| *bn == n).map(|(_, c)| *c);
    assert_eq!(user_txs(1), Some(1), "block 1: CREATE");
    assert_eq!(user_txs(2), Some(1), "block 2: SetCode");
    assert_eq!(user_txs(3), Some(1), "block 3: delegated CALL");
}

// ---------------------------------------------------------------------------
// B. Pre-Isthmus SetCode batches are dropped
// ---------------------------------------------------------------------------

/// A `SetCode` transaction batched into a pre-Isthmus L2 block is dropped by the
/// verifier (`BatchValidity::Drop::Eip7702PreIsthmus`), and the pipeline
/// force-includes a deposit-only block once the sequencing window closes.
///
/// Setup (`block_time` = 2, Isthmus unscheduled):
/// ```text
///   seq_window_size = 4  (epoch-0 window closes at L1 block 4)
///   L1 block 1: batch for L2 block 1 (user tx)     → derived normally
///   L1 block 2: batch for L2 block 2 (user tx)     → derived normally
///   L1 block 3: batch for L2 block 3 (SetCode)     → DROPPED (pre-Isthmus)
///   L1 block 4: empty (closes epoch-0 window)      → block 3 force-included
///                                                      as deposit-only
/// ```
#[tokio::test]
async fn setcode_batch_is_dropped_before_isthmus() {
    let batcher_cfg = calldata_batcher();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_holocene()
        .with_seq_window_size(4)
        .build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut setcode_block_hash = Default::default();
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg);
    for i in 1u64..=3 {
        if i == 3 {
            // Prague (and therefore type-4 execution) is scheduled with Isthmus, so
            // the Holocene EL will not include a SetCode tx. Splice one into the
            // batch so derivation still sees `Eip7702PreIsthmus`.
            let tx = create_self_setcode_tx(&sequencer, chain_id, Address::repeat_byte(0x42));
            let block = sequencer.build_next_block_with_transactions(Vec::new()).await;
            let block = with_user_txs(block, vec![tx]);
            assert!(
                block.body.transactions.iter().any(|tx| matches!(tx, BaseTxEnvelope::Eip7702(_))),
                "pre-Isthmus batch must carry a SetCode envelope"
            );
            setcode_block_hash = block.header.hash_slow();
            batcher.push_block(block);
        } else {
            batcher.push_block(sequencer.build_next_block_with_single_transaction().await);
        }
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.register_block_hash(3, setcode_block_hash);
    node.initialize().await;

    node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        2,
        "blocks 1-2 derive; block 3's SetCode batch is dropped pre-Isthmus"
    );

    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    node.run_until_idle().await;

    assert!(
        node.l2_safe_number() >= 3,
        "safe head must advance past block 3 via a deposit-only block"
    );

    let counts = node.derived_user_tx_counts();
    let user_txs = |n: u64| counts.iter().find(|(bn, _)| *bn == n).map(|(_, c)| *c);
    assert_eq!(user_txs(1), Some(1), "block 1: valid batch, 1 user tx");
    assert_eq!(user_txs(2), Some(1), "block 2: valid batch, 1 user tx");
    assert_eq!(
        user_txs(3),
        Some(0),
        "block 3: SetCode batch dropped pre-Isthmus -> deposit-only, 0 user txs"
    );
}

// ---------------------------------------------------------------------------
// C. Span-batch SetCode with the contract-creation bit set is rejected
// ---------------------------------------------------------------------------

/// Span-batch encoding of a `SetCode` transaction must carry a destination
/// address. If the contract-creation bit is set, span decode fails with
/// [`SpanDecodingError::InvalidTransactionData`] — the same check the
/// derivation pipeline runs in [`SpanBatchTransactions::full_txs`].
///
/// The `SetCode` envelope is produced by the sequencer (real execution), then
/// re-encoded as span-batch tx data so the reject path is exercised on a
/// production-shaped transaction rather than a hand-rolled byte string.
///
/// A well-formed span batch of the same block is also derived end-to-end to
/// show the creation-bit check is the thing that fails, not span encoding of
/// `SetCode` itself.
///
/// [`SpanBatchTransactions::full_txs`]: base_protocol::SpanBatchTransactions::full_txs
#[tokio::test]
async fn setcode_span_batch_creation_bit_is_rejected() {
    let batcher_cfg = calldata_batcher();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_isthmus().build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let setcode_tx = create_self_setcode_tx(&sequencer, chain_id, Address::repeat_byte(0x42));
    let encoded = Bytes::from(setcode_tx.encoded_2718());
    let block = sequencer.build_next_block_with_transactions(vec![setcode_tx]).await;
    assert!(
        user_tx_succeeded(&sequencer, &block, 0, "span SetCode"),
        "post-Isthmus SetCode must execute"
    );

    let mut span_txs = SpanBatchTransactions::default();
    span_txs.add_txs(vec![encoded], chain_id).expect("well-formed SetCode encodes as span tx data");
    assert_eq!(
        span_txs.contract_creation_bits.get_bit(0),
        Some(0),
        "honest span encoding never sets the creation bit on SetCode"
    );

    span_txs.contract_creation_bits.set_bit(0, true);
    assert_eq!(
        span_txs.full_txs(chain_id),
        Err(SpanBatchError::Decoding(SpanDecodingError::InvalidTransactionData)),
        "SetCode with the contract-creation bit set must fail span reconstruction"
    );

    h.submit_span_batch_calldata(&batcher_cfg, &[block], 0)
        .expect("well-formed span-batch SetCode fixture submission");

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;
    node.run_until_idle().await;

    assert_eq!(node.l2_safe_number(), 1, "well-formed span-batch SetCode must derive");
    assert_eq!(
        node.derived_user_tx_counts().iter().find(|(bn, _)| *bn == 1).map(|(_, c)| *c),
        Some(1),
        "derived span-batch SetCode block keeps the user tx"
    );
}
