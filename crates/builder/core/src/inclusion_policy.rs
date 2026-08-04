//! Pluggable transaction inclusion policy for the flashblocks build loop.
//!
//! When building a (flash)block the builder executes each candidate transaction before committing
//! its state changes. [`InclusionPolicy`] lets that post-execution decision be customized — using
//! the original pooled transaction plus the execution success flag — without forking the build loop.
//!
//! [`DefaultInclusionPolicy`] is the default and includes every executed transaction, reproducing
//! prior behavior byte-for-byte.

use base_execution_txpool::BasePooledTx;

/// The boxed inclusion policy used when callers need dynamic dispatch.
pub type BoxedInclusionPolicy<T> = Box<dyn InclusionPolicy<T>>;

/// Decides whether an executed candidate transaction should be included in the current payload.
///
/// The builder calls [`InclusionPolicy::should_include`] after execution has produced an outcome
/// and before state changes are committed. Implementations receive the original pooled transaction
/// so any admission-time extension data can be inspected while it is still available.
pub trait InclusionPolicy<T>: Send + Sync + std::fmt::Debug + 'static
where
    T: BasePooledTx,
{
    /// Returns `true` when the transaction should be included in the payload.
    fn should_include(&self, tx: &T, is_success: bool) -> bool;
}

impl<T> InclusionPolicy<T> for BoxedInclusionPolicy<T>
where
    T: BasePooledTx,
{
    fn should_include(&self, tx: &T, is_success: bool) -> bool {
        self.as_ref().should_include(tx, is_success)
    }
}

/// The default inclusion policy: include every executed transaction.
///
/// This reproduces prior behavior byte-for-byte — every candidate that reaches the post-execution
/// decision point is included.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultInclusionPolicy;

impl<T> InclusionPolicy<T> for DefaultInclusionPolicy
where
    T: BasePooledTx,
{
    fn should_include(&self, _tx: &T, _is_success: bool) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Encodable2718;
    use base_common_consensus::BaseTransactionSigned;
    use base_execution_txpool::BasePooledTransaction;
    use base_test_utils::Account;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;
    use crate::TxnExecutionError;

    #[derive(Debug)]
    struct AlwaysSkip;

    impl InclusionPolicy<BasePooledTransaction> for AlwaysSkip {
        fn should_include(&self, _tx: &BasePooledTransaction, _is_success: bool) -> bool {
            false
        }
    }

    fn test_tx() -> BasePooledTransaction {
        let alice = Account::Alice;
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(alice.signer_b256())
            .chain_id(1)
            .nonce(0)
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

    #[test]
    fn default_policy_includes_non_successful_outcome() {
        assert!(DefaultInclusionPolicy.should_include(&test_tx(), false));
    }

    #[test]
    fn skip_policy_excludes_and_policy_error_is_not_permanent() {
        assert!(!AlwaysSkip.should_include(&test_tx(), true));
        assert!(!TxnExecutionError::PolicyRejected.is_permanent());
    }
}
