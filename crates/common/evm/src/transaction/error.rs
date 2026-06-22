//! Contains the `[BaseTransactionError]` type.

use core::fmt::Display;

use alloy_evm::InvalidTxError;
use revm::{
    context::tx::TxEnvBuildError,
    context_interface::{
        result::{EVMError, InvalidTransaction},
        transaction::TransactionError,
    },
};

/// Error type for building [`TxEnv`]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BuildError {
    /// Base transaction build error
    Base(TxEnvBuildError),
    /// Missing enveloped transaction bytes
    MissingEnvelopedTxBytes,
    /// Missing source hash for deposit transaction
    MissingSourceHashForDeposit,
}

impl From<TxEnvBuildError> for BuildError {
    fn from(error: TxEnvBuildError) -> Self {
        Self::Base(error)
    }
}

/// Base transaction validation error.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BaseTransactionError {
    /// Base transaction error.
    Base(InvalidTransaction),
    /// System transactions are not supported post-regolith upgrade.
    ///
    /// Before the Regolith upgrade, there was a special field in the `Deposit` transaction
    /// type that differentiated between `system` and `user` deposit transactions. This field
    /// was deprecated in the Regolith upgrade, and this error is thrown if a `Deposit` transaction
    /// is found with this field set to `true` after the upgrade activation.
    ///
    /// In addition, this error is internal, and bubbles up into a [`BaseHaltReason::FailedDeposit`][crate::BaseHaltReason::FailedDeposit] error
    /// in the `revm` handler for the consumer to easily handle. This is due to a state transition
    /// rule on Base where, if for any reason a deposit transaction fails, the transaction
    /// must still be included in the block, the sender nonce is bumped, the `mint` value persists, and
    /// special gas accounting rules are applied. Normally on L1, [`EVMError::Transaction`] errors
    /// are cause for non-inclusion, so a special [`BaseHaltReason`][crate::BaseHaltReason] variant was introduced to handle this
    /// case for failed deposit transactions.
    DepositSystemTxPostRegolith,
    /// Deposit transaction halts bubble up to the global main return handler, wiping state and
    /// only increasing the nonce + persisting the mint value.
    ///
    /// This is a catch-all error for any deposit transaction that results in a [`BaseHaltReason`][crate::BaseHaltReason] error
    /// post-regolith upgrade. This allows for a consumer to easily handle special cases where
    /// a deposit transaction fails during validation, but must still be included in the block.
    ///
    /// In addition, this error is internal, and bubbles up into a [`BaseHaltReason::FailedDeposit`][crate::BaseHaltReason::FailedDeposit] error
    /// in the `revm` handler for the consumer to easily handle. This is due to a state transition
    /// rule on Base where, if for any reason a deposit transaction fails, the transaction
    /// must still be included in the block, the sender nonce is bumped, the `mint` value persists, and
    /// special gas accounting rules are applied. Normally on L1, [`EVMError::Transaction`] errors
    /// are cause for non-inclusion, so a special [`BaseHaltReason`][crate::BaseHaltReason] variant was introduced to handle this
    /// case for failed deposit transactions.
    HaltedDepositPostRegolith,
    /// Missing enveloped transaction bytes for non-deposit transaction.
    ///
    /// Non-deposit transactions on Base must have `enveloped_tx` field set
    /// to properly calculate L1 costs.
    MissingEnvelopedTx,
    /// An EIP-8130 (account-abstraction) transaction was rejected during its
    /// enshrined execution pipeline (authorization, nonce, intrinsic gas, fee,
    /// or account-change apply). The string is the underlying rejection reason.
    ///
    /// As an [`EVMError::Transaction`] error this is cause for non-inclusion, so
    /// the transaction's journal writes are reverted and it is not added to the
    /// block.
    Eip8130(alloc::string::String),
}

impl BaseTransactionError {
    /// Wraps an EIP-8130 pipeline rejection (any [`Display`] error from the
    /// authorize / nonce / intrinsic-gas / fee / apply stages) as
    /// [`BaseTransactionError::Eip8130`]. Keeps the call sites a single
    /// `.map_err(BaseTransactionError::eip8130)?`.
    pub fn eip8130(reason: impl Display) -> Self {
        Self::Eip8130(alloc::string::ToString::to_string(&reason))
    }
}

impl TransactionError for BaseTransactionError {}

impl Display for BaseTransactionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Base(error) => error.fmt(f),
            Self::DepositSystemTxPostRegolith => {
                write!(f, "deposit system transactions post regolith upgrade are not supported")
            }
            Self::HaltedDepositPostRegolith => {
                write!(
                    f,
                    "deposit transaction halted post-regolith; error will be bubbled up to main return handler"
                )
            }
            Self::MissingEnvelopedTx => {
                write!(f, "missing enveloped transaction bytes for non-deposit transaction")
            }
            Self::Eip8130(reason) => {
                write!(f, "EIP-8130 transaction rejected: {reason}")
            }
        }
    }
}

impl InvalidTxError for BaseTransactionError {
    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        match self {
            Self::Base(tx) => Some(tx),
            _ => None,
        }
    }
}

impl core::error::Error for BaseTransactionError {}

impl From<InvalidTransaction> for BaseTransactionError {
    fn from(value: InvalidTransaction) -> Self {
        Self::Base(value)
    }
}

impl<DBError> From<BaseTransactionError> for EVMError<DBError, BaseTransactionError> {
    fn from(value: BaseTransactionError) -> Self {
        Self::Transaction(value)
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::*;

    #[test]
    fn test_display_base_errors() {
        assert_eq!(
            BaseTransactionError::Base(InvalidTransaction::NonceTooHigh { tx: 2, state: 1 })
                .to_string(),
            "nonce 2 too high, expected 1"
        );
        assert_eq!(
            BaseTransactionError::DepositSystemTxPostRegolith.to_string(),
            "deposit system transactions post regolith upgrade are not supported"
        );
        assert_eq!(
            BaseTransactionError::HaltedDepositPostRegolith.to_string(),
            "deposit transaction halted post-regolith; error will be bubbled up to main return handler"
        );
        assert_eq!(
            BaseTransactionError::MissingEnvelopedTx.to_string(),
            "missing enveloped transaction bytes for non-deposit transaction"
        );
        assert_eq!(
            BaseTransactionError::eip8130("nonce too low").to_string(),
            "EIP-8130 transaction rejected: nonce too low"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serialize_json_base_transaction_error() {
        let response = r#""DepositSystemTxPostRegolith""#;

        let base_transaction_error: BaseTransactionError = serde_json::from_str(response).unwrap();
        assert_eq!(base_transaction_error, BaseTransactionError::DepositSystemTxPostRegolith);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serialize_json_eip8130_error() {
        let error = BaseTransactionError::Eip8130("payer balance too low".to_string());

        let serialized = serde_json::to_string(&error).unwrap();
        assert_eq!(serialized, r#"{"Eip8130":"payer balance too low"}"#);

        let round_trip: BaseTransactionError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_trip, error);
    }
}
