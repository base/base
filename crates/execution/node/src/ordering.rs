//! Custom ordering for transactions based on timestamp.

use std::marker::PhantomData;

use base_execution_txpool::{BasePooledTransaction, TimestampedTransaction};
use reth_transaction_pool::{CoinbaseTipOrdering, PoolTransaction, Priority, TransactionOrdering};

/// Configures which transaction ordering strategy the pool uses.
#[derive(Debug, Clone, Copy, Default)]
pub enum BaseOrderingMode {
    /// Order by coinbase tip (fee-based, higher tip = higher priority).
    #[default]
    CoinbaseTip,
    /// Order by receive timestamp (FIFO, earlier = higher priority).
    Timestamp,
}

/// Runtime-configurable transaction ordering.
///
/// Wraps both `CoinbaseTipOrdering` and `TimestampOrdering`, selecting
/// behavior based on the configured [`BaseOrderingMode`].
#[derive(Debug, Clone)]
pub struct BaseOrdering<T> {
    mode: BaseOrderingMode,
    coinbase: CoinbaseTipOrdering<T>,
    timestamp: TimestampOrdering<T>,
}

impl<T> BaseOrdering<T> {
    /// Creates a new ordering with the given mode.
    pub fn new(mode: BaseOrderingMode) -> Self {
        Self {
            mode,
            coinbase: CoinbaseTipOrdering::default(),
            timestamp: TimestampOrdering::default(),
        }
    }
}

impl<T> Default for BaseOrdering<T> {
    fn default() -> Self {
        Self::new(BaseOrderingMode::default())
    }
}

/// Ordering for transactions based on their timestamp (FIFO).
///
/// Transactions that arrived earlier get higher priority.
/// Uses a timestamp assigned at insertion time for deterministic ordering.
#[derive(Debug)]
#[non_exhaustive]
pub struct TimestampOrdering<T = BasePooledTransaction>(PhantomData<T>);

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

impl<T> TransactionOrdering for BaseOrdering<T>
where
    T: PoolTransaction + TimestampedTransaction + 'static,
{
    type PriorityValue = i64;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        match self.mode {
            BaseOrderingMode::CoinbaseTip => {
                let p = self.coinbase.priority(transaction, base_fee);
                match p {
                    Priority::Value(v) => Priority::Value(v.min(i64::MAX as u128) as i64),
                    Priority::None => Priority::None,
                }
            }
            BaseOrderingMode::Timestamp => self.timestamp.priority(transaction, base_fee),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Encodable2718;
    use base_execution_primitives::OpTransactionSigned;
    use base_execution_txpool::BasePooledTransaction;
    use base_test_utils::Account;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{TransactionOrdering, test_utils::TransactionBuilder};

    use super::*;

    fn create_test_tx(nonce: u64) -> BasePooledTransaction {
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
        BasePooledTransaction::new(recovered, len)
    }

    fn create_test_tx_with_timestamp(nonce: u64, received_at: i64) -> BasePooledTransaction {
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
        BasePooledTransaction::new_with_received_at(recovered, len, received_at)
    }

    #[test]
    fn test_older_tx_has_higher_priority() {
        let ordering = TimestampOrdering::<BasePooledTransaction>::default();

        let older_tx = create_test_tx_with_timestamp(1, 1000);
        let newer_tx = create_test_tx_with_timestamp(2, 2000);

        let older_priority = ordering.priority(&older_tx, 0);
        let newer_priority = ordering.priority(&newer_tx, 0);
        assert!(
            older_priority > newer_priority,
            "older_tx priority should be greater than newer_tx priority",
        );
    }

    #[test]
    fn test_priority_value_is_max_minus_timestamp() {
        let ordering = TimestampOrdering::<BasePooledTransaction>::default();
        let tx = create_test_tx(1);

        let priority = ordering.priority(&tx, 0);
        match priority {
            Priority::Value(val) => {
                assert_eq!(val, i64::MAX - tx.timestamp());
            }
            Priority::None => panic!("Expected Priority::Value"),
        }
    }

    #[test]
    fn test_base_ordering_coinbase_tip_mode() {
        let ordering = BaseOrdering::<BasePooledTransaction>::new(BaseOrderingMode::CoinbaseTip);

        let higher_tip = create_test_tx(1);
        let lower_tip = {
            let alice = Account::Alice;
            let bob = Account::Bob;

            let signed_tx = TransactionBuilder::default()
                .signer(alice.signer_b256())
                .chain_id(1)
                .nonce(2)
                .to(bob.address())
                .value(1_000)
                .gas_limit(21_000)
                .max_fee_per_gas(10)
                .max_priority_fee_per_gas(0)
                .into_eip1559();
            let tx = OpTransactionSigned::Eip1559(
                signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
            );
            let recovered = Recovered::new_unchecked(tx, alice.address());
            let len = recovered.encode_2718_len();
            BasePooledTransaction::new(recovered, len)
        };

        let higher_priority = ordering.priority(&higher_tip, 0);
        let lower_priority = ordering.priority(&lower_tip, 0);
        assert!(higher_priority > lower_priority);
    }

    #[test]
    fn test_base_ordering_timestamp_mode() {
        let ordering = BaseOrdering::<BasePooledTransaction>::new(BaseOrderingMode::Timestamp);

        let older_tx = create_test_tx_with_timestamp(1, 1000);
        let newer_tx = create_test_tx_with_timestamp(2, 2000);

        let older_priority = ordering.priority(&older_tx, 0);
        let newer_priority = ordering.priority(&newer_tx, 0);
        assert!(older_priority > newer_priority);
    }

    #[test]
    fn test_base_ordering_default_is_coinbase_tip() {
        let ordering = BaseOrdering::<BasePooledTransaction>::default();
        let tx = create_test_tx(1);
        let priority = ordering.priority(&tx, 0);
        let coinbase = CoinbaseTipOrdering::<BasePooledTransaction>::default();
        let coinbase_priority = coinbase.priority(&tx, 0);
        let expected = match coinbase_priority {
            Priority::Value(v) => {
                Priority::Value(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
            }
            Priority::None => Priority::None,
        };
        assert_eq!(priority, expected);
    }
}
