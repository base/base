//! Custom ordering for transactions based on timestamp.

use std::marker::PhantomData;

use base_execution_txpool::{OpPooledTransaction, TimestampedTransaction};
use reth_transaction_pool::{PoolTransaction, Priority, TransactionOrdering};

/// Ordering for transactions based on their timestamp (FIFO).
///
/// Transactions that arrived earlier get higher priority.
/// Uses a timestamp assigned at insertion time for deterministic ordering.
#[derive(Debug)]
#[non_exhaustive]
pub struct TimestampOrdering<T = OpPooledTransaction>(PhantomData<T>);

impl<T> Default for TimestampOrdering<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> Clone for TimestampOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T> TransactionOrdering for TimestampOrdering<T>
where
    T: PoolTransaction + TimestampedTransaction + 'static,
{
    /// Priority value is inverted timestamp.
    /// Higher value = higher priority, so we use MAX - timestamp.
    type PriorityValue = i64;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        _base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        // Reth sorts descending (higher value = picked first)
        // We want older transactions (lower timestamp) first
        // So invert: MAX - timestamp
        Priority::Value(i64::MAX.saturating_sub(transaction.timestamp()))
    }
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use alloy_eips::eip2718::Encodable2718;
    use base_execution_primitives::OpTransactionSigned;
    use base_execution_txpool::OpPooledTransaction;
    use base_test_utils::Account;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{TransactionOrdering, test_utils::TransactionBuilder};

    use super::*;

    fn create_test_tx(nonce: u64) -> OpPooledTransaction {
        let alice = Account::Alice;
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(alice.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();
        let tx = OpTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        OpPooledTransaction::new(recovered, len)
    }

    #[test]
    fn test_older_tx_has_higher_priority() {
        let ordering = TimestampOrdering::<OpPooledTransaction>::default();

        let older_tx = create_test_tx(1);
        sleep(Duration::from_millis(1000));
        let newer_tx = create_test_tx(2);

        let older_priority = ordering.priority(&older_tx, 0);
        let newer_priority = ordering.priority(&newer_tx, 0);
        assert!(
            older_priority > newer_priority,
            "older_tx priority should be greater than newer_tx priority",
        );
    }

    #[test]
    fn test_priority_value_is_max_minus_timestamp() {
        let ordering = TimestampOrdering::<OpPooledTransaction>::default();
        let tx = create_test_tx(1);

        let priority = ordering.priority(&tx, 0);
        match priority {
            Priority::Value(val) => {
                assert_eq!(val, i64::MAX - tx.timestamp());
            }
            Priority::None => panic!("Expected Priority::Value"),
        }
    }
}
