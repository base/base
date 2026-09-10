//! Test-only dispatch for existing pool ordering fixtures, which deliberately bypass protocol checks.

use base_common_types_chain::SealedBlock;

use super::OkValidator;
use crate::{
    BasePooledTransaction, BaseTransactionValidator, MockTransactionValidator, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
};

/// Validation choices confined to pool test fixtures.
#[derive(Debug, Clone)]
pub enum PoolValidator {
    /// Production asynchronous validation.
    Task(TransactionValidationTaskExecutor),
    /// Production validation exercised synchronously by unit tests.
    Base(BaseTransactionValidator),
    /// Scripted pool outcomes.
    Mock(MockTransactionValidator),
    /// Unconditional admission for gossip and ordering tests.
    Ok(OkValidator),
}

impl PoolValidator {
    /// Returns the Base validator for protocol-pool fixtures.
    pub fn validator(&self) -> &BaseTransactionValidator {
        match self {
            Self::Task(validator) => validator.validator(),
            Self::Base(validator) => validator,
            Self::Mock(_) | Self::Ok(_) => panic!("ordering fixtures have no Base validator"),
        }
    }
}
impl From<TransactionValidationTaskExecutor> for PoolValidator {
    fn from(value: TransactionValidationTaskExecutor) -> Self {
        Self::Task(value)
    }
}
impl From<BaseTransactionValidator> for PoolValidator {
    fn from(value: BaseTransactionValidator) -> Self {
        Self::Base(value)
    }
}
impl From<MockTransactionValidator> for PoolValidator {
    fn from(value: MockTransactionValidator) -> Self {
        Self::Mock(value)
    }
}
impl From<OkValidator> for PoolValidator {
    fn from(value: OkValidator) -> Self {
        Self::Ok(value)
    }
}
impl TransactionValidator for PoolValidator {
    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        match self {
            Self::Task(validator) => validator.validate_transaction(origin, transaction).await,
            Self::Base(validator) => validator.validate_transaction(origin, transaction).await,
            Self::Mock(validator) => validator.validate_transaction(origin, transaction).await,
            Self::Ok(validator) => validator.validate_transaction(origin, transaction).await,
        }
    }
    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<
            Item = (TransactionOrigin, BasePooledTransaction),
            IntoIter: Send,
        > + Send,
    ) -> Vec<TransactionValidationOutcome> {
        match self {
            Self::Task(validator) => validator.validate_transactions(transactions).await,
            Self::Base(validator) => validator.validate_transactions(transactions).await,
            Self::Mock(validator) => validator.validate_transactions(transactions).await,
            Self::Ok(validator) => validator.validate_transactions(transactions).await,
        }
    }
    async fn validate_transactions_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = BasePooledTransaction, IntoIter: Send> + Send,
    ) -> Vec<TransactionValidationOutcome> {
        match self {
            Self::Task(validator) => {
                validator.validate_transactions_with_origin(origin, transactions).await
            }
            Self::Base(validator) => {
                validator.validate_transactions_with_origin(origin, transactions).await
            }
            Self::Mock(validator) => {
                validator.validate_transactions_with_origin(origin, transactions).await
            }
            Self::Ok(validator) => {
                validator.validate_transactions_with_origin(origin, transactions).await
            }
        }
    }
    fn on_new_head_block(&self, block: &SealedBlock) {
        match self {
            Self::Task(validator) => validator.on_new_head_block(block),
            Self::Base(validator) => validator.on_new_head_block(block),
            Self::Mock(validator) => validator.on_new_head_block(block),
            Self::Ok(validator) => validator.on_new_head_block(block),
        }
    }
}
