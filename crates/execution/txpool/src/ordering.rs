//! Custom ordering for transactions based on timestamp.

use std::marker::PhantomData;

use reth_transaction_pool::{CoinbaseTipOrdering, PoolTransaction, Priority, TransactionOrdering};

use crate::{BasePooledTransaction, TimestampedTransaction};

/// Transaction ordering strategy for the pool.
///
/// Each variant holds only the ordering implementation it needs.
#[derive(Debug)]
pub enum BaseOrdering<T> {
    /// Order by coinbase tip (fee-based, higher tip = higher priority).
    CoinbaseTip(CoinbaseTipOrdering<T>),
    /// Order by receive timestamp (FIFO, earlier = higher priority).
    Timestamp(TimestampOrdering<T>),
}

impl<T> BaseOrdering<T> {
    /// Creates a new coinbase-tip ordering (fee-based).
    pub fn coinbase_tip() -> Self {
        Self::CoinbaseTip(CoinbaseTipOrdering::default())
    }

    /// Creates a new timestamp ordering (FIFO).
    pub fn timestamp() -> Self {
        Self::Timestamp(TimestampOrdering::default())
    }
}

impl<T> Clone for BaseOrdering<T> {
    fn clone(&self) -> Self {
        match self {
            Self::CoinbaseTip(ordering) => Self::CoinbaseTip(ordering.clone()),
            Self::Timestamp(ordering) => Self::Timestamp(ordering.clone()),
        }
    }
}

impl<T> Default for BaseOrdering<T> {
    fn default() -> Self {
        Self::coinbase_tip()
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
    type PriorityValue = u128;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        _base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        // Reth sorts descending (higher value = picked first).
        // We want older transactions (lower timestamp) first,
        // so invert: MAX - timestamp.
        Priority::Value(u128::MAX - transaction.received_at())
    }
}

impl<T> TransactionOrdering for BaseOrdering<T>
where
    T: PoolTransaction + TimestampedTransaction + 'static,
{
    type PriorityValue = u128;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        match self {
            Self::CoinbaseTip(ordering) => ordering.priority(transaction, base_fee),
            Self::Timestamp(ordering) => ordering.priority(transaction, base_fee),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Encodable2718;
    use base_common_consensus::BaseTransactionSigned;
    use base_test_utils::Account;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{TransactionOrdering, test_utils::TransactionBuilder};

    use super::*;
    use crate::BasePooledTransaction;

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
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, len)
    }

    fn create_test_tx_with_timestamp(nonce: u64, received_at: u128) -> BasePooledTransaction {
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
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new_with_received_at(recovered, len, received_at)
    }

    fn create_test_tx_from(
        account: Account,
        nonce: u64,
        received_at: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(max_priority_fee_per_gas)
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, account.address());
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
                assert_eq!(val, u128::MAX - tx.received_at());
            }
            Priority::None => panic!("Expected Priority::Value"),
        }
    }

    #[test]
    fn test_base_ordering_coinbase_tip_mode() {
        let ordering = BaseOrdering::<BasePooledTransaction>::coinbase_tip();

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
            let tx = BaseTransactionSigned::Eip1559(
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
        let ordering = BaseOrdering::<BasePooledTransaction>::timestamp();

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
        assert_eq!(priority, coinbase_priority);
    }

    // NOTE: Same-sender nonce ordering is enforced by the txpool layer, not the
    // ordering trait. The `TransactionOrdering::priority` method is called per
    // transaction in isolation — it has no visibility into sender or nonce.
    // Reth's txpool first groups pending transactions by sender and orders them
    // by nonce within each sender's queue, then uses the ordering's priority to
    // rank across senders. A full integration test against the txpool is needed
    // to verify same-sender nonce ordering interacts correctly with timestamp
    // priority.
    //
    // This test verifies the ordering's behavior for transactions from the same
    // sender: the one with the lower timestamp should receive higher priority,
    // regardless of nonce.
    #[test]
    fn test_same_sender_timestamp_ordering() {
        let ordering = BaseOrdering::<BasePooledTransaction>::timestamp();

        let tx_nonce_0 = create_test_tx_from(Account::Alice, 0, 1000, 1);
        let tx_nonce_1 = create_test_tx_from(Account::Alice, 1, 2000, 1);

        let priority_0 = ordering.priority(&tx_nonce_0, 0);
        let priority_1 = ordering.priority(&tx_nonce_1, 0);

        assert!(
            priority_0 > priority_1,
            "earlier tx should have higher priority regardless of nonce",
        );
    }
}
