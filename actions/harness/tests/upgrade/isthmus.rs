//! Action tests for Isthmus `withdrawals_root` header semantics.
//!
//! Covers the Holocene → Isthmus boundary: empty withdrawals root before
//! activation, `L2ToL1MessagePasser` storage root at/after activation, and both
//! paths with and without a withdrawal transaction.

use alloy_consensus::{TxReceipt, constants::EMPTY_WITHDRAWALS};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{Bytes, TxKind, U256};
use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, L2Sequencer, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_common_consensus::Predeploys;

const WITHDRAWAL_VALUE: u64 = 500;
const WITHDRAWAL_GAS_LIMIT: u64 = 100_000;

/// Call the harness `MessagePasser` stub so its storage root (and Isthmus
/// `withdrawals_root`) can change.
fn withdrawal_tx(sequencer: &L2Sequencer, chain_id: u64) -> base_common_consensus::BaseTxEnvelope {
    let account = sequencer.test_account();
    let mut account = account.lock().expect("test account lock");
    account.create_tx(
        chain_id,
        TxKind::Call(Predeploys::L2_TO_L1_MESSAGE_PASSER),
        Bytes::new(),
        U256::from(WITHDRAWAL_VALUE),
        WITHDRAWAL_GAS_LIMIT,
    )
}

fn assert_user_tx_succeeded(sequencer: &L2Sequencer, block_number: u64) {
    let receipts = sequencer
        .engine_client()
        .receipts_at(block_number)
        .unwrap_or_else(|| panic!("missing receipts for block {block_number}"));
    let user_receipt =
        receipts.last().unwrap_or_else(|| panic!("block {block_number} has no receipts"));
    assert!(user_receipt.status(), "expected successful withdrawal tx in block {block_number}");
}

fn assert_pre_isthmus_withdrawals_root(block_number: u64, root: Option<alloy_primitives::B256>) {
    assert_eq!(
        root,
        Some(EMPTY_WITHDRAWALS),
        "block {block_number}: pre-Isthmus withdrawals_root must be EMPTY_WITHDRAWALS"
    );
}

fn assert_isthmus_withdrawals_root(
    sequencer: &L2Sequencer,
    block_number: u64,
    root: Option<alloy_primitives::B256>,
) {
    let expected =
        sequencer.engine_client().account_storage_root(Predeploys::L2_TO_L1_MESSAGE_PASSER);
    assert_eq!(
        root,
        Some(expected),
        "block {block_number}: Isthmus withdrawals_root must equal L2ToL1MessagePasser storage root"
    );
}

/// Pre-Isthmus: `withdrawals_root` stays `EMPTY_WITHDRAWALS` even if a withdrawal
/// executes — the field is not the `MessagePasser` storage root until Isthmus.
#[tokio::test]
async fn withdrawals_root_empty_before_isthmus() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_holocene().build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let empty = sequencer.build_empty_block().await;
    assert_pre_isthmus_withdrawals_root(empty.header.number, empty.header.withdrawals_root);
    assert!(empty.header.requests_hash.is_none(), "pre-Isthmus blocks must not set requests_hash");

    let with_withdrawal = sequencer
        .build_next_block_with_transactions(vec![withdrawal_tx(&sequencer, chain_id)])
        .await;
    assert_pre_isthmus_withdrawals_root(
        with_withdrawal.header.number,
        with_withdrawal.header.withdrawals_root,
    );
}

/// Isthmus at genesis: header root matches `MessagePasser` storage, and a
/// withdrawal must update both.
#[tokio::test]
async fn withdrawals_root_matches_message_passer_at_isthmus_genesis() {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_isthmus().build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let empty = sequencer.build_empty_block().await;
    assert_isthmus_withdrawals_root(&sequencer, empty.header.number, empty.header.withdrawals_root);
    assert_eq!(
        empty.header.requests_hash,
        Some(EMPTY_REQUESTS_HASH),
        "Isthmus blocks must set the empty requests hash"
    );

    assert!(
        sequencer.engine_client().has_code(Predeploys::L2_TO_L1_MESSAGE_PASSER),
        "L2ToL1MessagePasser must be deployed in genesis"
    );

    let before_withdrawal = empty.header.withdrawals_root.expect("isthmus root present");
    let with_withdrawal = sequencer
        .build_next_block_with_transactions(vec![withdrawal_tx(&sequencer, chain_id)])
        .await;
    assert_user_tx_succeeded(&sequencer, with_withdrawal.header.number);
    assert_isthmus_withdrawals_root(
        &sequencer,
        with_withdrawal.header.number,
        with_withdrawal.header.withdrawals_root,
    );
    assert_ne!(
        with_withdrawal.header.withdrawals_root,
        Some(before_withdrawal),
        "initiating a withdrawal must change the Isthmus withdrawals_root"
    );
}

#[derive(Clone, Copy)]
struct TransitionCase {
    name: &'static str,
    withdrawal_tx: bool,
    /// 1-indexed L2 block that should include the withdrawal, if any.
    withdrawal_block: u64,
    /// With `block_time=2` and Isthmus at ts=6, activation is L2 block 3.
    total_blocks: u64,
}

/// Cross Isthmus activation at ts=6 and check roots before / at / after the fork.
#[tokio::test]
async fn withdrawals_root_before_at_and_after_isthmus() {
    const ISTHMUS_TIME: u64 = 6;

    let cases = [
        TransitionCase {
            name: "before_isthmus_without_withdrawal",
            withdrawal_tx: false,
            withdrawal_block: 0,
            total_blocks: 1,
        },
        TransitionCase {
            name: "before_isthmus_with_withdrawal",
            withdrawal_tx: true,
            withdrawal_block: 1,
            total_blocks: 1,
        },
        TransitionCase {
            name: "at_isthmus_without_withdrawal",
            withdrawal_tx: false,
            withdrawal_block: 0,
            total_blocks: 3,
        },
        TransitionCase {
            name: "at_isthmus_with_withdrawal",
            withdrawal_tx: true,
            withdrawal_block: 3,
            total_blocks: 3,
        },
        TransitionCase {
            name: "after_isthmus_without_withdrawal",
            withdrawal_tx: false,
            withdrawal_block: 0,
            total_blocks: 4,
        },
        TransitionCase {
            name: "after_isthmus_with_withdrawal",
            withdrawal_tx: true,
            withdrawal_block: 4,
            total_blocks: 4,
        },
    ];

    for case in cases {
        run_transition_case(ISTHMUS_TIME, case).await;
    }
}

async fn run_transition_case(isthmus_time: u64, case: TransitionCase) {
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_holocene()
        .with_isthmus_at(isthmus_time)
        .build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    let mut last_root = None;
    for block_number in 1..=case.total_blocks {
        let include_withdrawal = case.withdrawal_tx && case.withdrawal_block == block_number;
        let block = if include_withdrawal {
            sequencer
                .build_next_block_with_transactions(vec![withdrawal_tx(&sequencer, chain_id)])
                .await
        } else {
            sequencer.build_empty_block().await
        };

        assert_eq!(
            block.header.number, block_number,
            "{}: expected L2 block {block_number}",
            case.name
        );
        if include_withdrawal {
            assert_user_tx_succeeded(&sequencer, block.header.number);
        }

        let is_isthmus = block.header.timestamp >= isthmus_time;
        if is_isthmus {
            assert_isthmus_withdrawals_root(
                &sequencer,
                block.header.number,
                block.header.withdrawals_root,
            );
            assert_eq!(
                block.header.requests_hash,
                Some(EMPTY_REQUESTS_HASH),
                "{}: Isthmus block {} must set empty requests_hash",
                case.name,
                block.header.number
            );
            if include_withdrawal {
                assert_ne!(
                    block.header.withdrawals_root, last_root,
                    "{}: withdrawal in block {} must change withdrawals_root",
                    case.name, block_number
                );
            }
        } else {
            assert_pre_isthmus_withdrawals_root(block.header.number, block.header.withdrawals_root);
            assert!(
                block.header.requests_hash.is_none(),
                "{}: pre-Isthmus block {} must not set requests_hash",
                case.name,
                block.header.number
            );
        }

        last_root = block.header.withdrawals_root;
    }
}
