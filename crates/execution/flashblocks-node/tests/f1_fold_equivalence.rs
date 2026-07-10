//! F1 invariant: flashblocks fold-equivalence at the external boundary.
//!
//! For a sequencer slot `S`, folding flashblocks `0..N` for `S` must yield the
//! same state root, receipts root, and transaction list as the eventual full
//! sealed block for `S`, and preserve the eventual block hash in the latest
//! flashblock delta.
//!
//! Note: pending RPC blocks intentionally expose a zero hash while flashblocks
//! are still speculative. The true folded hash is carried in
//! `latest_flashblock.diff.block_hash`.

use alloy_eips::{Decodable2718, Encodable2718};
use alloy_network::TransactionResponse;
use alloy_primitives::{B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::{BaseBlock, BaseTransactionSigned, TxDeposit};
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
};
use base_flashblocks::{FlashblocksAPI, PendingBlocksAPI};
use base_flashblocks_node::test_harness::FlashblocksBuilderTestHarness;
use base_node_runner::test_utils::L1_BLOCK_INFO_DEPOSIT_TX;
use base_test_utils::Account;
use reth_primitives_traits::RecoveredBlock;

#[tokio::test]
async fn f1_fold_equivalence() {
    for flashblock_count in [1_u64, 3, 10] {
        assert_fold_matches_full_block(flashblock_count).await;
    }
}

async fn assert_fold_matches_full_block(flashblock_count: u64) {
    let mut harness = FlashblocksBuilderTestHarness::new().await;

    let tx_count = flashblock_count.max(2);
    let mut user_transactions = Vec::with_capacity(tx_count as usize + 1);
    let deposit = TxDeposit::decode_2718(&mut L1_BLOCK_INFO_DEPOSIT_TX.as_ref())
        .expect("fixture deposit transaction should decode");
    user_transactions.push(BaseTransactionSigned::from(deposit));

    for nonce in 0..tx_count {
        user_transactions.push(harness.build_transaction_to_send_eth_with_nonce(
            Account::Alice,
            Account::Bob,
            10_000 + nonce as u128,
            nonce,
        ));
    }

    let full_block = harness.new_canonical_block_without_processing(user_transactions).await;
    let full_block_number = full_block.header().number;

    let flashblocks = build_flashblocks_for_full_block(&full_block, flashblock_count as usize);
    for flashblock in flashblocks {
        harness.send_flashblock(flashblock).await;
    }

    let folded = harness
        .flashblocks
        .get_pending_blocks()
        .get_block(true)
        .expect("pending block should be produced from flashblocks");

    assert_eq!(folded.header.number, full_block_number);
    assert_eq!(folded.header.hash, B256::ZERO);
    assert_eq!(folded.header.state_root, full_block.header().state_root);
    assert_eq!(folded.header.receipts_root, full_block.header().receipts_root);

    let folded_flashblocks = harness
        .flashblocks
        .get_pending_blocks()
        .as_ref()
        .expect("pending blocks should be present")
        .latest_block_flashblocks();
    let folded_hash = folded_flashblocks
        .last()
        .expect("latest pending block should have at least one flashblock")
        .diff
        .block_hash;
    assert_eq!(folded_hash, full_block.hash());

    let folded_tx_hashes: Vec<B256> = folded
        .transactions
        .as_transactions()
        .expect("full transactions requested")
        .iter()
        .map(|transaction| transaction.tx_hash())
        .collect();
    let full_block_tx_hashes: Vec<B256> =
        full_block.body().transactions.iter().map(|transaction| transaction.tx_hash()).collect();

    assert_eq!(folded_tx_hashes, full_block_tx_hashes, "transaction order must be preserved");

    // Negative regression guard: dropping the latest flashblock must break equivalence.
    if flashblock_count > 1 {
        let mut dropped_latest =
            build_flashblocks_for_full_block(&full_block, flashblock_count as usize);
        dropped_latest.pop();

        let dropped_harness = FlashblocksBuilderTestHarness::new().await;
        for flashblock in dropped_latest {
            dropped_harness.send_flashblock(flashblock).await;
        }

        let dropped_folded = dropped_harness
            .flashblocks
            .get_pending_blocks()
            .get_block(true)
            .expect("pending block should be produced from partial flashblocks");

        let dropped_hash = dropped_harness
            .flashblocks
            .get_pending_blocks()
            .as_ref()
            .expect("pending blocks should be present for partial fold")
            .latest_block_flashblocks()
            .last()
            .expect("partial fold should still expose latest flashblock")
            .diff
            .block_hash;

        assert_ne!(dropped_hash, full_block.hash());
        assert_ne!(dropped_folded.header.state_root, full_block.header().state_root);
    }
}

fn build_flashblocks_for_full_block(
    full_block: &RecoveredBlock<BaseBlock>,
    flashblock_count: usize,
) -> Vec<Flashblock> {
    let full_header = full_block.header();
    let block_number = full_header.number;

    let base = ExecutionPayloadBaseV1 {
        parent_beacon_block_root: full_header.parent_beacon_block_root.unwrap_or_default(),
        parent_hash: full_header.parent_hash,
        fee_recipient: full_header.beneficiary,
        prev_randao: full_header.mix_hash,
        block_number,
        gas_limit: full_header.gas_limit,
        timestamp: full_header.timestamp,
        extra_data: full_header.extra_data.clone(),
        base_fee_per_gas: U256::from(full_header.base_fee_per_gas.unwrap_or_default()),
    };

    let tx_bytes: Vec<Bytes> = full_block
        .body()
        .transactions
        .iter()
        .map(|transaction| transaction.encoded_2718().into())
        .collect();
    let tx_chunks = split_ordered(tx_bytes, flashblock_count);

    (0..flashblock_count)
        .map(|index| {
            let is_latest = index + 1 == flashblock_count;
            let intermediate_state_root = B256::from([0xAB_u8; 32]);
            Flashblock {
                payload_id: PayloadId::default(),
                index: index as u64,
                base: (index == 0).then_some(base.clone()),
                diff: ExecutionPayloadFlashblockDeltaV1 {
                    state_root: if is_latest {
                        full_header.state_root
                    } else {
                        intermediate_state_root
                    },
                    receipts_root: if is_latest { full_header.receipts_root } else { B256::ZERO },
                    block_hash: if is_latest { full_block.hash() } else { B256::ZERO },
                    gas_used: if is_latest { full_header.gas_used } else { 0 },
                    withdrawals: Vec::new(),
                    logs_bloom: if is_latest { full_header.logs_bloom } else { Bloom::default() },
                    withdrawals_root: if is_latest {
                        full_header.withdrawals_root.unwrap_or_default()
                    } else {
                        B256::ZERO
                    },
                    transactions: tx_chunks[index].clone(),
                    blob_gas_used: if is_latest { full_header.blob_gas_used } else { None },
                },
                metadata: Metadata::new(block_number),
            }
        })
        .collect()
}

fn split_ordered(transactions: Vec<Bytes>, chunks: usize) -> Vec<Vec<Bytes>> {
    let len = transactions.len();
    (0..chunks)
        .map(|index| {
            let start = index * len / chunks;
            let end = (index + 1) * len / chunks;
            transactions[start..end].to_vec()
        })
        .collect()
}
