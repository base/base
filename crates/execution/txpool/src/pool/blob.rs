use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::txpool::PendingFees;
use crate::{
    SubPoolLimit, ValidPoolTransaction, identifier::TransactionId, pool::size::SizeTracker,
    traits::BestTransactionsAttributes,
};

/// A set of validated blob transactions in the pool that are __not pending__.
///
/// The purpose of this pool is to keep track of blob transactions that are queued and to evict the
/// worst blob transactions once the sub-pool is full.
///
/// This expects that certain constraints are met:
///   - blob transactions are always gapless
#[derive(Debug, Clone)]
pub struct BlobTransactions {
    /// Keeps track of transactions inserted in the pool.
    ///
    /// This way we can determine when transactions were submitted to the pool.
    submission_id: u64,
    /// _All_ Transactions that are currently inside the pool grouped by their identifier.
    by_id: BTreeMap<TransactionId, BlobTransaction>,
    /// _All_ transactions sorted by blob priority.
    all: BTreeSet<BlobTransaction>,
    /// Keeps track of the current fees, so transaction priority can be calculated on insertion.
    pending_fees: PendingFees,
    /// Keeps track of the size of this pool.
    ///
    /// See also [`base_common_types_chain::InMemorySize::size`].
    size_of: SizeTracker,
}

// === impl BlobTransactions ===

impl BlobTransactions {
    /// Adds a new transactions to the pending queue.
    ///
    /// # Panics
    ///
    ///   - If the transaction is not a blob tx.
    ///   - If the transaction is already included.
    pub fn add_transaction(&mut self, tx: Arc<ValidPoolTransaction>) {
        assert!(tx.is_eip4844(), "transaction is not a blob tx");
        let id = *tx.id();
        assert!(!self.contains(&id), "transaction already included {:?}", self.get(&id).unwrap());
        let submission_id = self.next_id();

        // keep track of size
        self.size_of += tx.size();

        // set transaction, which will also calculate priority based on current pending fees
        let transaction = BlobTransaction::new(tx, submission_id, &self.pending_fees);

        self.by_id.insert(id, transaction.clone());
        self.all.insert(transaction);
    }

    const fn next_id(&mut self) -> u64 {
        let id = self.submission_id;
        self.submission_id = self.submission_id.wrapping_add(1);
        id
    }

    /// Removes the transaction from the pool
    pub fn remove_transaction(&mut self, id: &TransactionId) -> Option<Arc<ValidPoolTransaction>> {
        // remove from queues
        let tx = self.by_id.remove(id)?;

        self.all.remove(&tx);

        // keep track of size
        self.size_of -= tx.transaction.size();

        Some(tx.transaction)
    }

    /// Returns all transactions that satisfy the given basefee and blobfee.
    ///
    /// Note: This does not remove any of the transactions from the pool.
    pub fn satisfy_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        let mut transactions = Vec::new();
        {
            // short path if blob_fee is None in provided best transactions attributes
            if let Some(blob_fee_to_satisfy) =
                best_transactions_attributes.blob_fee.map(|fee| fee as u128)
            {
                let mut iter = self.by_id.iter().peekable();

                while let Some((id, tx)) = iter.next() {
                    if tx.transaction.max_fee_per_blob_gas().unwrap_or_default()
                        < blob_fee_to_satisfy
                        || tx.transaction.max_fee_per_gas()
                            < best_transactions_attributes.basefee as u128
                    {
                        // does not satisfy the blob fee or base fee
                        // still parked in blob pool -> skip descendant transactions
                        'this: while let Some((peek, _)) = iter.peek() {
                            if peek.sender != id.sender {
                                break 'this;
                            }
                            iter.next();
                        }
                    } else {
                        transactions.push(tx.transaction.clone());
                    }
                }
            }
        }
        transactions
    }

    /// Returns true if the pool exceeds the given limit
    #[inline]
    pub fn exceeds(&self, limit: &SubPoolLimit) -> bool {
        limit.is_exceeded(self.len(), self.size())
    }

    /// The reported size of all transactions in this pool.
    pub fn size(&self) -> usize {
        self.size_of.into()
    }

    /// Number of transactions in the entire pool
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Returns whether the pool is empty
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Returns all transactions which:
    ///  * have a `max_fee_per_blob_gas` greater than or equal to the given `blob_fee`, _and_
    ///  * have a `max_fee_per_gas` greater than or equal to the given `base_fee`
    fn satisfy_pending_fee_ids(&self, pending_fees: &PendingFees) -> Vec<TransactionId> {
        let mut transactions = Vec::new();
        {
            let mut iter = self.by_id.iter().peekable();

            while let Some((id, tx)) = iter.next() {
                if tx.transaction.max_fee_per_blob_gas() < Some(pending_fees.blob_fee)
                    || tx.transaction.max_fee_per_gas() < pending_fees.base_fee as u128
                {
                    // still parked in blob pool -> skip descendant transactions
                    'this: while let Some((peek, _)) = iter.peek() {
                        if peek.sender != id.sender {
                            break 'this;
                        }
                        iter.next();
                    }
                } else {
                    transactions.push(*id);
                }
            }
        }
        transactions
    }

    /// Resorts the transactions in the pool based on the pool's current [`PendingFees`].
    pub fn reprioritize(&mut self) {
        // mem::take to modify without allocating, then collect to rebuild the BTreeSet
        self.all = std::mem::take(&mut self.all)
            .into_iter()
            .map(|mut tx| {
                tx.update_priority(&self.pending_fees);
                tx
            })
            .collect();

        // we need to update `by_id` as well because removal from `all` can only happen if the
        // `BlobTransaction`s in each struct are consistent
        for tx in self.by_id.values_mut() {
            tx.update_priority(&self.pending_fees);
        }
    }

    /// Removes all transactions (and their descendants) which:
    ///  * have a `max_fee_per_blob_gas` greater than or equal to the given `blob_fee`, _and_
    ///  * have a `max_fee_per_gas` greater than or equal to the given `base_fee`
    ///
    /// This also sets the [`PendingFees`] for the pool, resorting transactions based on their
    /// updated priority.
    ///
    /// Note: the transactions are not returned in a particular order.
    pub fn enforce_pending_fees(
        &mut self,
        pending_fees: &PendingFees,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        let removed = self
            .satisfy_pending_fee_ids(pending_fees)
            .into_iter()
            .map(|id| self.remove_transaction(&id).expect("transaction exists"))
            .collect();

        // Update pending fees and reprioritize
        self.pending_fees = pending_fees.clone();
        self.reprioritize();

        removed
    }

    /// Removes transactions until the pool satisfies its [`SubPoolLimit`].
    ///
    /// This is done by removing transactions according to their ordering in the pool, defined by
    /// the [`BlobOrd`] struct.
    ///
    /// Removed transactions are returned in the order they were removed.
    pub fn truncate_pool(&mut self, limit: SubPoolLimit) -> Vec<Arc<ValidPoolTransaction>> {
        let mut removed = Vec::new();

        while self.exceeds(&limit) {
            let tx = self.all.last().expect("pool is not empty");
            let id = *tx.transaction.id();
            removed.push(self.remove_transaction(&id).expect("transaction exists"));
        }

        removed
    }

    /// Returns `true` if the transaction with the given id is already included in this pool.
    pub fn contains(&self, id: &TransactionId) -> bool {
        self.by_id.contains_key(id)
    }

    /// Retrieves a transaction with the given ID from the pool, if it exists.
    fn get(&self, id: &TransactionId) -> Option<&BlobTransaction> {
        self.by_id.get(id)
    }

    /// Asserts that the bijection between `by_id` and `all` is valid.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn assert_invariants(&self) {
        assert_eq!(self.by_id.len(), self.all.len(), "by_id.len() != all.len()");
    }
}

impl Default for BlobTransactions {
    fn default() -> Self {
        Self {
            submission_id: 0,
            by_id: Default::default(),
            all: Default::default(),
            size_of: Default::default(),
            pending_fees: Default::default(),
        }
    }
}

/// A transaction that is ready to be included in a block.
#[derive(Debug)]
pub struct BlobTransaction {
    /// Actual blob transaction.
    transaction: Arc<ValidPoolTransaction>,
    /// The value that determines the order of this transaction.
    ord: BlobOrd,
}

impl BlobTransaction {
    /// Creates a new blob transaction, based on the pool transaction, submission id, and current
    /// pending fees.
    pub fn new(
        transaction: Arc<ValidPoolTransaction>,
        submission_id: u64,
        pending_fees: &PendingFees,
    ) -> Self {
        let priority = blob_tx_priority(
            transaction.max_fee_per_blob_gas().unwrap_or_default(),
            pending_fees.blob_fee,
            transaction.max_fee_per_gas(),
            pending_fees.base_fee as u128,
        );
        let ord = BlobOrd { priority, submission_id };
        Self { transaction, ord }
    }

    /// Updates the priority for the transaction based on the current pending fees.
    pub fn update_priority(&mut self, pending_fees: &PendingFees) {
        self.ord.priority = blob_tx_priority(
            self.transaction.max_fee_per_blob_gas().unwrap_or_default(),
            pending_fees.blob_fee,
            self.transaction.max_fee_per_gas(),
            pending_fees.base_fee as u128,
        );
    }
}

impl Clone for BlobTransaction {
    fn clone(&self) -> Self {
        Self { transaction: self.transaction.clone(), ord: self.ord.clone() }
    }
}

impl Eq for BlobTransaction {}

impl PartialEq<Self> for BlobTransaction {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd<Self> for BlobTransaction {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BlobTransaction {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ord.cmp(&other.ord)
    }
}

/// This is the log base 2 of 1.125, which we'll use to calculate the priority
const LOG_2_1_125: f64 = 0.16992500144231237;

/// The blob step function, attempting to compute the delta given the `max_tx_fee`, and
/// `current_fee`.
///
/// The `max_tx_fee` is the maximum fee that the transaction is willing to pay, this
/// would be the priority fee for the EIP1559 component of transaction fees, and the blob fee cap
/// for the blob component of transaction fees.
///
/// The `current_fee` is the current value of the fee, this would be the base fee for the EIP1559
/// component, and the blob fee (computed from the current head) for the blob component.
///
/// This is supposed to get the number of fee jumps required to get from the current fee to the fee
/// cap, or where the transaction would not be executable any more.
///
/// A positive value means that the transaction will remain executable unless the current fee
/// increases.
///
/// A negative value means that the transaction is currently not executable, and requires the
/// current fee to decrease by some number of jumps before the max fee is greater than the current
/// fee.
pub fn fee_delta(max_tx_fee: u128, current_fee: u128) -> i64 {
    if max_tx_fee == current_fee {
        // if these are equal, then there's no fee jump
        return 0;
    }

    let max_tx_fee_jumps = if max_tx_fee == 0 {
        // we can't take log2 of 0, so we set this to zero here
        0f64
    } else {
        (max_tx_fee.ilog2() as f64) / LOG_2_1_125
    };

    let current_fee_jumps = if current_fee == 0 {
        // we can't take log2 of 0, so we set this to zero here
        0f64
    } else {
        (current_fee.ilog2() as f64) / LOG_2_1_125
    };

    // jumps = log1.125(txfee) - log1.125(basefee)
    let jumps = max_tx_fee_jumps - current_fee_jumps;

    // delta = sign(jumps) * log(abs(jumps))
    match (jumps as i64).cmp(&0) {
        Ordering::Equal => {
            // can't take ilog2 of 0
            0
        }
        Ordering::Greater => (jumps.ceil() as i64).ilog2() as i64,
        Ordering::Less => -((-jumps.floor() as i64).ilog2() as i64),
    }
}

/// Returns the priority for the transaction, based on the "delta" blob fee and priority fee.
pub fn blob_tx_priority(
    blob_fee_cap: u128,
    blob_fee: u128,
    max_priority_fee: u128,
    base_fee: u128,
) -> i64 {
    let delta_blob_fee = fee_delta(blob_fee_cap, blob_fee);
    let delta_priority_fee = fee_delta(max_priority_fee, base_fee);

    // TODO: this could be u64:
    // * if all are positive, zero is returned
    // * if all are negative, the min negative value is returned
    // * if some are positive and some are negative, the min negative value is returned
    //
    // the BlobOrd could then just be a u64, and higher values represent worse transactions (more
    // jumps for one of the fees until the cap satisfies)
    //
    // priority = min(delta-basefee, delta-blobfee, 0)
    delta_blob_fee.min(delta_priority_fee).min(0)
}

/// A struct used to determine the ordering for a specific blob transaction in the pool. This uses
/// a `priority` value to determine the ordering, and uses the `submission_id` to break ties.
///
/// The `priority` value is calculated using the [`blob_tx_priority`] function, and should be
/// re-calculated on each block.
#[derive(Debug, Clone)]
pub struct BlobOrd {
    /// Identifier that tags when transaction was submitted in the pool.
    pub submission_id: u64,
    /// The priority for this transaction, calculated using the [`blob_tx_priority`] function,
    /// taking into account both the blob and priority fee.
    pub priority: i64,
}

impl Eq for BlobOrd {}

impl PartialEq<Self> for BlobOrd {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd<Self> for BlobOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BlobOrd {
    /// Compares two `BlobOrd` instances.
    ///
    /// The comparison is performed in reverse order based on the priority field. This is
    /// because transactions with larger negative values in the priority field will take more fee
    /// jumps, making them take longer to become executable. Therefore, transactions with lower
    /// ordering should return `Greater`, ensuring they are evicted first.
    ///
    /// If the priority values are equal, the submission ID is used to break ties.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| self.submission_id.cmp(&other.submission_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockTransaction, MockTransactionFactory};

    #[test]
    fn priority_tests() {
        // Test vectors from:
        // <https://github.com/ethereum/go-ethereum/blob/e91cdb49beb4b2a3872b5f2548bf2d6559e4f561/core/txpool/blobpool/priority_test.go#L27-L49>
        let vectors = vec![
            (7u128, 10u128, 2i64),
            (17_200_000_000, 17_200_000_000, 0),
            (9_853_941_692, 11_085_092_510, 0),
            (11_544_106_391, 10_356_781_100, 0),
            (17_200_000_000, 7, -7),
            (7, 17_200_000_000, 7),
        ];

        for (base_fee, tx_fee, expected) in vectors {
            let actual = fee_delta(tx_fee, base_fee);
            assert_eq!(
                actual, expected,
                "fee_delta({tx_fee}, {base_fee}) = {actual}, expected: {expected}"
            );
        }
    }

    #[test]
    fn test_empty_pool_operations() {
        let mut pool: BlobTransactions = BlobTransactions::default();

        // Ensure pool is empty
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.size(), 0);

        // Attempt to remove a non-existent transaction
        let non_existent_id = TransactionId::new(0.into(), 0);
        assert!(pool.remove_transaction(&non_existent_id).is_none());

        // Check contains method on empty pool
        assert!(!pool.contains(&non_existent_id));
    }

    #[test]
    fn test_satisfy_attributes_empty_pool() {
        let pool: BlobTransactions = BlobTransactions::default();
        let attributes = BestTransactionsAttributes { blob_fee: Some(100), basefee: 100 };
        // Satisfy attributes on an empty pool should return an empty vector
        let satisfied = pool.satisfy_attributes(attributes);
        assert!(satisfied.is_empty());
    }

    #[test]
    #[should_panic(expected = "transaction is not a blob tx")]
    fn test_add_non_blob_transaction() {
        // Ensure that adding a non-blob transaction causes a panic
        let mut factory = MockTransactionFactory::default();
        let mut pool = BlobTransactions::default();
        let tx = factory.validated_arc(MockTransaction::eip1559()); // Not a blob transaction
        pool.add_transaction(tx);
    }

    #[test]
    fn test_empty_pool_invariants() {
        // Ensure that the invariants hold for an empty pool
        let pool: BlobTransactions = BlobTransactions::default();
        pool.assert_invariants();
        assert!(pool.is_empty());
        assert_eq!(pool.size(), 0);
        assert_eq!(pool.len(), 0);
    }
}
