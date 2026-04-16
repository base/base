//! Merged best-transactions iterator that interleaves the standard pool
//! and the EIP-8130 2D nonce pool by effective priority.
//!
//! During block building the payload builder needs a single ordered stream of
//! transactions. [`MergedBestTransactions`] combines two [`BestTransactions`]
//! iterators — one from the standard Reth pool and one from [`Eip8130Pool`] —
//! yielding the higher-priority transaction at each step.
//!
//! Follows the pattern established by Tempo's `MergeBestTransactions`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alloy_primitives::B256;
use reth_transaction_pool::{
    BestTransactions, EthPoolTransaction, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

/// Merges two [`BestTransactions`] iterators by effective tip.
///
/// At each step the iterator peeks at the heads of both sub-iterators and
/// yields the one with the higher basefee-aware effective tip. On equal
/// priority the standard pool (left) wins to preserve existing ordering
/// behaviour for non-AA transactions.
///
/// Deduplicates by transaction hash to handle the case where a 2D-nonce
/// transaction also exists in the standard pool due to the collision-safe
/// routing added at validation time.
pub struct MergedBestTransactions<L, R>
where
    L: BestTransactions,
    R: BestTransactions,
{
    left: L,
    right: R,
    peeked_left: Option<L::Item>,
    peeked_right: Option<R::Item>,
    seen: HashSet<B256>,
    selected_from: HashMap<B256, SelectedSource>,
    base_fee: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedSource {
    Left,
    Right,
}

impl<L: BestTransactions, R: BestTransactions> core::fmt::Debug for MergedBestTransactions<L, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MergedBestTransactions")
            .field("seen_len", &self.seen.len())
            .field("selected_from_len", &self.selected_from.len())
            .finish_non_exhaustive()
    }
}

impl<L, R> MergedBestTransactions<L, R>
where
    L: BestTransactions,
    R: BestTransactions,
{
    /// Creates a new merged iterator from two sub-iterators.
    pub fn new(left: L, right: R) -> Self {
        Self::with_base_fee(left, right, 0)
    }

    /// Creates a merged iterator with an explicit base-fee context.
    pub fn with_base_fee(left: L, right: R, base_fee: u64) -> Self {
        Self {
            left,
            right,
            peeked_left: None,
            peeked_right: None,
            seen: HashSet::new(),
            selected_from: HashMap::new(),
            base_fee,
        }
    }
}

impl<T, L, R> Iterator for MergedBestTransactions<L, R>
where
    T: EthPoolTransaction,
    L: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    R: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.peeked_left.is_none() {
                self.peeked_left = self.left.next();
            }
            if self.peeked_right.is_none() {
                self.peeked_right = self.right.next();
            }

            let chosen = match (&self.peeked_left, &self.peeked_right) {
                (Some(l), Some(r)) => {
                    let l_prio =
                        l.transaction.effective_tip_per_gas(self.base_fee).unwrap_or_default();
                    let r_prio =
                        r.transaction.effective_tip_per_gas(self.base_fee).unwrap_or_default();
                    if l_prio >= r_prio {
                        self.peeked_left.take().map(|tx| (tx, SelectedSource::Left))
                    } else {
                        self.peeked_right.take().map(|tx| (tx, SelectedSource::Right))
                    }
                }
                (Some(_), None) => self.peeked_left.take().map(|tx| (tx, SelectedSource::Left)),
                (None, Some(_)) => self.peeked_right.take().map(|tx| (tx, SelectedSource::Right)),
                (None, None) => return None,
            };

            if let Some((tx, source)) = chosen {
                let hash = *tx.hash();
                if self.seen.insert(hash) {
                    self.selected_from.insert(hash, source);
                    return Some(tx);
                }
                // duplicate — skip and try again
            }
        }
    }
}

impl<T, L, R> BestTransactions for MergedBestTransactions<L, R>
where
    T: EthPoolTransaction,
    L: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    R: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: &InvalidPoolTransactionError) {
        match self.selected_from.remove(transaction.hash()) {
            Some(SelectedSource::Left) => self.left.mark_invalid(transaction, kind),
            Some(SelectedSource::Right) => self.right.mark_invalid(transaction, kind),
            None => {
                self.left.mark_invalid(transaction, kind);
                self.right.mark_invalid(transaction, kind);
            }
        }
    }

    fn no_updates(&mut self) {
        self.left.no_updates();
        self.right.no_updates();
    }

    fn skip_blobs(&mut self) {
        self.left.skip_blobs();
        self.right.skip_blobs();
    }

    fn set_skip_blobs(&mut self, skip: bool) {
        self.left.set_skip_blobs(skip);
        self.right.set_skip_blobs(skip);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc as StdArc, Mutex};
    use std::time::Instant;

    use alloy_consensus::{Transaction, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use base_alloy_consensus::OpTransactionSigned;
    use base_alloy_consensus::OpTypedTransaction;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{
        TransactionOrigin, ValidPoolTransaction,
        identifier::{SenderId, TransactionId},
    };

    use super::*;
    use crate::BasePooledTransaction;

    fn make_valid_tx(
        sender_byte: u8,
        nonce: u64,
        priority_fee: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        make_valid_tx_with_caps(sender_byte, nonce, 1000, priority_fee)
    }

    fn make_valid_tx_with_caps(
        sender_byte: u8,
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let sender = Address::repeat_byte(sender_byte);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(Address::repeat_byte(0xFF)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Default::default(),
        };
        let sig = Signature::new(
            U256::from(sender_byte as u64 * 1000 + nonce),
            U256::from(max_priority_fee_per_gas),
            false,
        );
        let signed = OpTransactionSigned::new_unhashed(OpTypedTransaction::Eip1559(tx), sig);
        let recovered = Recovered::new_unchecked(signed, sender);
        let len = recovered.encode_2718_len();
        let pooled = BasePooledTransaction::new(recovered, len);

        Arc::new(ValidPoolTransaction {
            transaction: pooled,
            transaction_id: TransactionId::new(SenderId::from(sender_byte as u64), nonce),
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    /// Minimal BestTransactions impl backed by a VecDeque.
    struct VecBest {
        txs: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    }

    impl VecBest {
        fn new(txs: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { txs: txs.into() }
        }
    }

    impl Iterator for VecBest {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;
        fn next(&mut self) -> Option<Self::Item> {
            self.txs.pop_front()
        }
    }

    impl BestTransactions for VecBest {
        fn mark_invalid(&mut self, _tx: &Self::Item, _kind: &InvalidPoolTransactionError) {}
        fn no_updates(&mut self) {}
        fn skip_blobs(&mut self) {}
        fn set_skip_blobs(&mut self, _skip: bool) {}
    }

    impl core::fmt::Debug for VecBest {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("VecBest").field("len", &self.txs.len()).finish()
        }
    }

    #[derive(Debug)]
    struct SpyBest {
        txs: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
        marked_invalid: StdArc<Mutex<usize>>,
    }

    impl SpyBest {
        fn new(
            txs: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
            marked_invalid: StdArc<Mutex<usize>>,
        ) -> Self {
            Self { txs: txs.into(), marked_invalid }
        }
    }

    impl Iterator for SpyBest {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.txs.pop_front()
        }
    }

    impl BestTransactions for SpyBest {
        fn mark_invalid(&mut self, _tx: &Self::Item, _kind: &InvalidPoolTransactionError) {
            *self.marked_invalid.lock().expect("poisoned lock") += 1;
        }

        fn no_updates(&mut self) {}
        fn skip_blobs(&mut self) {}
        fn set_skip_blobs(&mut self, _skip: bool) {}
    }

    #[test]
    fn merged_yields_higher_priority_first() {
        let left = VecBest::new(vec![make_valid_tx(0x01, 0, 10)]);
        let right = VecBest::new(vec![make_valid_tx(0x02, 0, 50)]);
        let mut merged = MergedBestTransactions::new(left, right);

        let first = merged.next().unwrap();
        assert_eq!(first.transaction.max_priority_fee_per_gas(), Some(50));

        let second = merged.next().unwrap();
        assert_eq!(second.transaction.max_priority_fee_per_gas(), Some(10));

        assert!(merged.next().is_none());
    }

    #[test]
    fn merged_left_wins_on_tie() {
        let left = VecBest::new(vec![make_valid_tx(0x01, 0, 20)]);
        let right = VecBest::new(vec![make_valid_tx(0x02, 0, 20)]);
        let mut merged = MergedBestTransactions::new(left, right);

        let first = merged.next().unwrap();
        assert_eq!(first.sender(), Address::repeat_byte(0x01));
    }

    #[test]
    fn merged_interleaves_correctly() {
        let left = VecBest::new(vec![make_valid_tx(0x01, 0, 50), make_valid_tx(0x01, 1, 20)]);
        let right = VecBest::new(vec![make_valid_tx(0x02, 0, 30), make_valid_tx(0x02, 1, 10)]);
        let merged = MergedBestTransactions::new(left, right);

        let priorities: Vec<u128> = merged
            .map(|tx| tx.transaction.max_priority_fee_per_gas().unwrap_or_default())
            .collect();
        assert_eq!(priorities, vec![50, 30, 20, 10]);
    }

    #[test]
    fn merged_deduplicates_by_hash() {
        let tx = make_valid_tx(0x01, 0, 10);
        let left = VecBest::new(vec![tx.clone()]);
        let right = VecBest::new(vec![tx]);
        let merged = MergedBestTransactions::new(left, right);

        let results: Vec<_> = merged.collect();
        assert_eq!(results.len(), 1, "duplicate should be skipped");
    }

    #[test]
    fn merged_empty_iterators() {
        let left = VecBest::new(vec![]);
        let right = VecBest::new(vec![]);
        let mut merged = MergedBestTransactions::new(left, right);
        assert!(merged.next().is_none());
    }

    #[test]
    fn merged_one_side_empty() {
        let left = VecBest::new(vec![make_valid_tx(0x01, 0, 10)]);
        let right = VecBest::new(vec![]);
        let mut merged = MergedBestTransactions::new(left, right);

        assert!(merged.next().is_some());
        assert!(merged.next().is_none());
    }

    #[test]
    fn merged_uses_effective_tip_with_base_fee() {
        let left = VecBest::new(vec![make_valid_tx_with_caps(0x01, 0, 101, 100)]);
        let right = VecBest::new(vec![make_valid_tx_with_caps(0x02, 0, 500, 5)]);
        let mut merged = MergedBestTransactions::with_base_fee(left, right, 100);

        let first = merged.next().expect("missing first tx");
        assert_eq!(
            first.sender(),
            Address::repeat_byte(0x02),
            "right tx should win because effective tip is 5 vs 1"
        );
    }

    #[test]
    fn merged_mark_invalid_routes_to_selected_source() {
        let left_invalid = StdArc::new(Mutex::new(0usize));
        let right_invalid = StdArc::new(Mutex::new(0usize));
        let left = SpyBest::new(vec![make_valid_tx(0x01, 0, 10)], left_invalid.clone());
        let right = SpyBest::new(vec![make_valid_tx(0x02, 0, 50)], right_invalid.clone());
        let mut merged = MergedBestTransactions::new(left, right);

        let first = merged.next().expect("missing tx");
        assert_eq!(first.sender(), Address::repeat_byte(0x02), "right tx should be selected first");

        let err = InvalidPoolTransactionError::Other(Box::new(crate::Eip8130PoolError::PoolFull));
        merged.mark_invalid(&first, &err);

        assert_eq!(*left_invalid.lock().expect("poisoned lock"), 0);
        assert_eq!(*right_invalid.lock().expect("poisoned lock"), 1);
    }
}
