use std::collections::HashSet;

use alloy_consensus::Block;
use alloy_primitives::{B256, U256};
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types::BaseTransactionReceipt;
use serde::Serialize;

use crate::{Candidate, DiffTotals};

/// Canonical measurements, including the full execution order (deposits included).
#[derive(Debug)]
pub struct CanonicalBlock {
    /// Canonical height.
    pub block_number: u64,
    /// Ordered transaction hashes.
    pub transaction_hashes: Vec<B256>,
    /// Execution gas consumed.
    pub gas_used: u64,
    /// Execution tips in wei; excludes base, L1 and operator fees and direct transfers.
    pub priority_fee_total: Option<U256>,
}

impl CanonicalBlock {
    /// Computes tips from complete, ordered RPC receipts; absent/mismatched receipts yield unknown.
    pub fn from_block(
        block: &Block<BaseTxEnvelope>,
        receipts: Option<&[BaseTransactionReceipt]>,
    ) -> Self {
        let priority_fee_total =
            receipts.filter(|rs| rs.len() == block.body.transactions.len()).and_then(|rs| {
                let hash = block.hash_slow();
                block.body.transactions.iter().zip(rs).try_fold(U256::ZERO, |total, (tx, r)| {
                    if r.inner.transaction_hash != tx.tx_hash() || r.inner.block_hash != Some(hash)
                    {
                        return None;
                    }
                    let tip = if tx.is_deposit() {
                        0
                    } else {
                        r.inner.effective_gas_price.checked_sub(u128::from(
                            block.header.base_fee_per_gas.unwrap_or_default(),
                        ))?
                    };
                    Some(total + U256::from(tip) * U256::from(r.inner.gas_used))
                })
            });
        Self {
            block_number: block.header.number,
            transaction_hashes: block.body.transactions.iter().map(|tx| tx.tx_hash()).collect(),
            gas_used: block.header.gas_used,
            priority_fee_total,
        }
    }
}

/// Invalid comparison inputs.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// Comparing different heights is not meaningful.
    #[error("candidate and canonical heights differ")]
    HeightMismatch,
    /// Reports require exactly N+1..M in increasing order.
    #[error("report blocks must be contiguous after the parent")]
    Window,
}

/// One candidate compared with its canonical block.
#[derive(Debug, Serialize)]
pub struct BlockDiff {
    /// Compared height.
    pub block_number: u64,
    /// N+1 is retained separately and excluded from aggregates.
    pub warmup: bool,
    /// Zero-based first mismatch over full lists; a strict prefix diverges at its end.
    pub first_divergence_index: Option<usize>,
    /// Inclusion and resource measurements.
    pub totals: DiffTotals,
}

impl BlockDiff {
    /// Compares unique inclusion sets and full ordering; recording gaps are a subset of missing.
    ///
    /// # Errors
    /// Returns [`DiffError::HeightMismatch`] for different heights.
    pub fn compare(
        candidate: &Candidate,
        canonical: &CanonicalBlock,
        recorded: Option<&HashSet<B256>>,
    ) -> Result<Self, DiffError> {
        if candidate.block_number != canonical.block_number {
            return Err(DiffError::HeightMismatch);
        }
        let actual: HashSet<_> = candidate.transaction_hashes.iter().copied().collect();
        let expected: HashSet<_> = canonical.transaction_hashes.iter().copied().collect();
        let missing: HashSet<_> = expected.difference(&actual).copied().collect();
        let first_divergence_index = candidate
            .transaction_hashes
            .iter()
            .zip(&canonical.transaction_hashes)
            .position(|(a, b)| a != b)
            .or_else(|| {
                (candidate.transaction_hashes.len() != canonical.transaction_hashes.len())
                    .then_some(
                        candidate.transaction_hashes.len().min(canonical.transaction_hashes.len()),
                    )
            });
        Ok(Self {
            block_number: candidate.block_number,
            warmup: false,
            first_divergence_index,
            totals: DiffTotals {
                missing: missing.len(),
                extra: actual.difference(&expected).count(),
                common: actual.intersection(&expected).count(),
                recording_gap: recorded.map(|hashes| missing.difference(hashes).count()),
                canonical_gas_used: u128::from(canonical.gas_used),
                candidate_gas_used: u128::from(candidate.gas_used),
                canonical_priority_fee_wei: canonical.priority_fee_total,
                candidate_priority_fee_wei: candidate.priority_fee_total,
                build_latency_us: candidate.build_latency.as_micros(),
                flashblock_count: candidate.flashblock_count,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, time::Duration};

    use alloy_consensus::{Block, Sealable, SignableTransaction, TxLegacy};
    use alloy_primitives::{B256, Signature, U256};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types::BaseTransactionReceipt;

    use super::*;
    use crate::Candidate;

    #[test]
    fn counts_inclusion_and_recording_gaps_when_transactions_differ() {
        let canonical = CanonicalBlock {
            block_number: 2,
            transaction_hashes: vec![B256::ZERO, B256::repeat_byte(1)],
            gas_used: 42,
            priority_fee_total: Some(U256::from(7)),
        };
        let candidate = Candidate {
            block_number: 2,
            transaction_hashes: vec![B256::ZERO, B256::repeat_byte(2)],
            gas_used: 21,
            priority_fee_total: U256::from(3),
            build_latency: Duration::from_micros(9),
            flashblock_count: Some(4),
        };

        let diff = BlockDiff::compare(&candidate, &canonical, Some(&HashSet::new())).unwrap();

        assert_eq!((diff.totals.missing, diff.totals.extra, diff.totals.common), (1, 1, 1));
        assert_eq!(diff.totals.recording_gap, Some(1));
        assert_eq!(diff.first_divergence_index, Some(1));
        assert_eq!((diff.totals.canonical_gas_used, diff.totals.candidate_gas_used), (42, 21));
        assert_eq!(diff.totals.canonical_priority_fee_wei, Some(U256::from(7)));
        assert_eq!(diff.totals.candidate_priority_fee_wei, U256::from(3));
        assert_eq!((diff.totals.build_latency_us, diff.totals.flashblock_count), (9, Some(4)));
    }

    #[test]
    fn distinguishes_prefix_order_and_unknown_recording_when_lists_change() {
        for (hashes, divergence) in [
            (vec![], Some(0)),
            (vec![B256::ZERO], Some(1)),
            (vec![B256::ZERO, B256::repeat_byte(1)], None),
            (vec![B256::repeat_byte(1), B256::ZERO], Some(0)),
            (vec![B256::ZERO, B256::repeat_byte(1), B256::repeat_byte(2)], Some(2)),
        ] {
            let canonical = CanonicalBlock {
                block_number: 1,
                transaction_hashes: vec![B256::ZERO, B256::repeat_byte(1)],
                gas_used: 0,
                priority_fee_total: None,
            };
            let candidate = Candidate {
                block_number: 1,
                transaction_hashes: hashes,
                gas_used: 0,
                priority_fee_total: U256::ZERO,
                build_latency: Duration::ZERO,
                flashblock_count: None,
            };

            let diff = BlockDiff::compare(&candidate, &canonical, None).unwrap();

            assert_eq!(diff.first_divergence_index, divergence);
            assert_eq!(diff.totals.recording_gap, None);
            let recorded = canonical.transaction_hashes.iter().copied().collect();
            let known = BlockDiff::compare(&candidate, &canonical, Some(&recorded)).unwrap();
            assert_eq!(known.totals.recording_gap, Some(0));
        }
    }

    #[test]
    fn computes_tips_when_complete_rpc_receipts_match_canonical_block() {
        let mut block = Block::<BaseTxEnvelope>::default();
        block.header.base_fee_per_gas = Some(10);
        block.body.transactions = vec![
            BaseTxEnvelope::Deposit(TxDeposit::default().seal_slow()),
            BaseTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::test_signature())),
        ];
        let mut receipts: Vec<_> = block
            .body
            .transactions
            .iter()
            .map(|tx| {
                serde_json::from_value::<BaseTransactionReceipt>(serde_json::json!({
                    "type": "0x0", "status": "0x1", "cumulativeGasUsed": "0x5208",
                    "logsBloom": alloy_primitives::Bloom::ZERO, "logs": [],
                    "transactionHash": tx.tx_hash(), "gasUsed": "0x5208",
                    "blockHash": block.hash_slow(), "effectiveGasPrice": "0xd",
                    "from": alloy_primitives::Address::ZERO
                }))
                .unwrap()
            })
            .collect();

        let canonical = CanonicalBlock::from_block(&block, Some(&receipts));

        assert_eq!(canonical.priority_fee_total, Some(U256::from(63_000)));
        assert_eq!(canonical.transaction_hashes[0], block.body.transactions[0].tx_hash());
        assert_eq!(CanonicalBlock::from_block(&block, None).priority_fee_total, None);
        receipts[1].inner.transaction_hash = B256::ZERO;
        assert_eq!(CanonicalBlock::from_block(&block, Some(&receipts)).priority_fee_total, None);
        assert_eq!(CanonicalBlock::from_block(&block, Some(&[])).priority_fee_total, None);
    }
}
