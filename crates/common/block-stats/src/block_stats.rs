//! Per-block statistics derived from a header and transaction list.

use alloy_consensus::Transaction;
use base_common_consensus::BaseTransaction;

/// Per-block figures shared by the live builder emitter and the offline
/// shadow-block reader.
///
/// Deposits stay in [`Self::gas_used`] and [`Self::transaction_count`] but leave
/// the fee-ordered vector because they are not fee-ordered. Transactions with no
/// effective tip are skipped, not zeroed; zero would invent an inversion. An
/// inversion is a strict `next > previous` increase in adjacent effective tips,
/// matching the builder's descending-priority assertion and excluding equal tips.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockStats {
    /// Header gas used, including deposits.
    pub gas_used: u64,
    /// Transaction count, including deposits.
    pub transaction_count: usize,
    /// Non-deposit transaction count.
    pub non_deposit_transaction_count: usize,
    /// Adjacent effective-tip increases among non-deposit transactions.
    pub priority_fee_inversions: usize,
}

impl BlockStats {
    /// Derives per-block statistics from a header's `gas_used`, the block
    /// `base_fee`, and the block's transactions.
    #[must_use]
    pub fn from_transactions<T>(gas_used: u64, base_fee: u64, transactions: &[T]) -> Self
    where
        T: Transaction + BaseTransaction,
    {
        let mut non_deposit_transaction_count = 0usize;
        let mut priority_fee_inversions = 0usize;
        let mut previous_tip: Option<u128> = None;

        for tx in transactions.iter().filter(|tx| !tx.is_deposit()) {
            non_deposit_transaction_count += 1;

            // Missing tips are skipped, not zeroed, and do not reset the running
            // comparison: the next present tip is compared to the last present one.
            let Some(tip) = tx.effective_tip_per_gas(base_fee) else { continue };
            if let Some(previous) = previous_tip
                && tip > previous
            {
                priority_fee_inversions += 1;
            }
            previous_tip = Some(tip);
        }

        Self {
            gas_used,
            transaction_count: transactions.len(),
            non_deposit_transaction_count,
            priority_fee_inversions,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Sealable, SignableTransaction, TxEip1559};
    use alloy_primitives::{Signature, TxKind, U256};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};

    use super::*;

    const BASE_FEE: u64 = 100;

    /// Effective tip is `min(max_priority_fee, max_fee - base_fee)`; the generous
    /// `max_fee` makes `tip` the binding effective tip at [`BASE_FEE`].
    fn tx(tip: u128) -> BaseTxEnvelope {
        TxEip1559 {
            max_priority_fee_per_gas: tip,
            max_fee_per_gas: BASE_FEE as u128 + tip,
            gas_limit: 21_000,
            to: TxKind::Call(Default::default()),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
        .into()
    }

    /// EIP-1559 tx priced below the base fee, so its effective tip is `None`.
    fn tx_below_base_fee() -> BaseTxEnvelope {
        TxEip1559 {
            max_priority_fee_per_gas: 5,
            max_fee_per_gas: BASE_FEE as u128 - 1,
            gas_limit: 21_000,
            to: TxKind::Call(Default::default()),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
        .into()
    }

    fn deposit() -> BaseTxEnvelope {
        BaseTxEnvelope::Deposit(
            TxDeposit { gas_limit: 21_000, value: U256::from(1u64), ..Default::default() }
                .seal_slow(),
        )
    }

    #[test]
    fn descending_tips_have_no_inversions() {
        let txs = [tx(30), tx(20), tx(10)];
        let stats = BlockStats::from_transactions(63_000, BASE_FEE, &txs);
        assert_eq!(stats.gas_used, 63_000);
        assert_eq!(stats.transaction_count, 3);
        assert_eq!(stats.non_deposit_transaction_count, 3);
        assert_eq!(stats.priority_fee_inversions, 0);
    }

    #[test]
    fn each_ascending_step_is_one_inversion() {
        // 10 -> 20 -> 15: one strict increase (10->20), then a decrease.
        let txs = [tx(10), tx(20), tx(15)];
        let stats = BlockStats::from_transactions(63_000, BASE_FEE, &txs);
        assert_eq!(stats.priority_fee_inversions, 1);
    }

    #[test]
    fn equal_adjacent_tips_are_not_inversions() {
        let txs = [tx(20), tx(20), tx(20)];
        let stats = BlockStats::from_transactions(63_000, BASE_FEE, &txs);
        assert_eq!(stats.priority_fee_inversions, 0);
    }

    #[test]
    fn deposits_leave_the_fee_vector_but_stay_in_totals() {
        // Deposit sits between two descending tips; it must not create an inversion
        // and must not count as a non-deposit tx, but still counts in the total.
        let txs = [tx(30), deposit(), tx(10)];
        let stats = BlockStats::from_transactions(63_000, BASE_FEE, &txs);
        assert_eq!(stats.transaction_count, 3);
        assert_eq!(stats.non_deposit_transaction_count, 2);
        assert_eq!(stats.priority_fee_inversions, 0);
    }

    #[test]
    fn missing_tips_are_skipped_not_zeroed() {
        // The middle tx has no effective tip; comparison skips it, so 30 -> 10 is
        // still descending (no inversion) rather than 30 -> 0 -> 10 (one inversion).
        let txs = [tx(30), tx_below_base_fee(), tx(10)];
        let stats = BlockStats::from_transactions(63_000, BASE_FEE, &txs);
        assert_eq!(stats.non_deposit_transaction_count, 3);
        assert_eq!(stats.priority_fee_inversions, 0);
    }

    #[test]
    fn empty_block_is_all_zero() {
        let stats = BlockStats::from_transactions(0, BASE_FEE, &[] as &[BaseTxEnvelope]);
        assert_eq!(stats, BlockStats::default());
    }
}
