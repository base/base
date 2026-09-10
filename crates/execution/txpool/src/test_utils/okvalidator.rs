use base_common_types_chain::Transaction;

use crate::{
    TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
    validate::ValidTransaction,
};

/// A transaction validator that determines all transactions to be valid.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OkValidator {
    /// Whether to mark transactions as propagatable.
    propagate: bool,
}

impl OkValidator {
    /// Determines whether transactions should be allowed to be propagated
    pub const fn set_propagate_transactions(mut self, propagate: bool) -> Self {
        self.propagate = propagate;
        self
    }
}

impl Default for OkValidator {
    fn default() -> Self {
        Self { propagate: false }
    }
}

impl TransactionValidator for OkValidator {
    async fn validate_transaction(
        &self,
        _origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        // Always return valid
        let authorities = transaction.authorization_list().map(|auths| {
            auths.iter().flat_map(|auth| auth.recover_authority()).collect::<Vec<_>>()
        });
        TransactionValidationOutcome::Valid {
            balance: *transaction.cost(),
            state_nonce: transaction.nonce(),
            bytecode_hash: None,
            transaction: ValidTransaction::Valid(transaction),
            propagate: self.propagate,
            authorities,
        }
    }
}
