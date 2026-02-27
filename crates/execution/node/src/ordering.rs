//! Ordering for transactions based on their timestamp (FIFO).

use std::{
    marker::PhantomData,
    time::{Duration, Instant},
};

use base_execution_txpool::OpPooledTransaction;
use reth_transaction_pool::{PoolTransaction, Priority, TransactionOrdering};

/// Ordering for transactions based on their timestamp (FIFO).
///
/// Transactions that arrived earlier get higher priority. This ensures fair ordering based on submission time. In other words, smaller timestamps get higher priority.
#[derive(Debug)]
#[non_exhaustive]
pub struct TimestampOrdering<T = OpPooledTransaction>(PhantomData<T>);

impl<T> Default for TimestampOrdering<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<T> Clone for TimestampOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// Trait for transactions that expose their timestamp.
pub trait TimestampedTransaction {
    /// Returns the timestamp when this transaction was received.
    fn timestamp(&self) -> Instant;
}

impl<Cons, Pooled> TimestampedTransaction for OpPooledTransaction<Cons, Pooled>
where
    Cons: reth_primitives_traits::SignedTransaction,
    Pooled: Send + Sync + 'static,
{
    fn timestamp(&self) -> Instant {
        self.timestamp()
    }
}

impl<T> TransactionOrdering for TimestampOrdering<T>
where
    T: PoolTransaction + TimestampedTransaction + 'static,
{
    type PriorityValue = Duration;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        _base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        // Earlier timestamp = higher priority
        // We use elapsed() which gives larger values for older transactions
        // Reth sorts by descending order (higher priority value = selected first)
        // So older transactions (larger elapsed) get picked first = FIFO
        Priority::Value(transaction.timestamp().elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_ordering() {
        let t1 = Priority::Value(Duration::from_secs(1));
        let t2 = Priority::Value(Duration::from_secs(2));
        let t3 = Priority::Value(Duration::from_secs(3));

        assert!(t1 < t2); // t1 is older than t2
        assert!(t2 < t3); // t2 is older than t3
        assert!(t3 > t2); // t3 is newer than t2
        assert!(t2 > t1); // t2 is newer than t1
        assert!(t1 < t3); // t1 is older than t3
    }
}
