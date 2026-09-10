//! Block execution abstraction.

use alloy_eips::eip2718::WithEncoded;
use base_common_types_chain::transaction::Recovered;
use base_execution_evm_runtime::{
    FromRecoveredTx, FromTxWithEncoded, RecoveredTx, ResultAndState, ToTxEnv, either::Either,
};

mod error;
pub use error::*;

mod gas_output;
pub use gas_output::*;

mod system_calls;
pub use system_calls::*;

mod state_changes;
pub use state_changes::*;

mod state;
pub use state::*;

mod calc;
pub use base_common_types_chain::BlockExecutionResult;
pub use calc::*;

/// Helper trait to encapsulate requirements for a type to be used as input for [`base_execution_evm_runtime::BaseBlockExecutor`].
///
/// This trait combines the requirements for a transaction to be executable by a block executor:
/// - Must be convertible to the EVM's transaction environment, such as revm's
///   [`TxEnv`](base_execution_evm_runtime::TxEnv)
/// - Must provide access to the transaction and signer via [`RecoveredTx`]
///
/// The trait ensures that the block executor can both execute the transaction in the EVM
/// and access the original transaction data for receipt generation.
///
/// # Implementations
///
/// The following implementations are provided:
/// - `Recovered<T>` and `Recovered<&T>` - owned recovered transactions
/// - `WithEncoded<Recovered<T>>` and `WithEncoded<&Recovered<T>>` - encoded transactions
/// - `Either<L, R>` where both `L` and `R` implement this trait
/// - `&S` where `S: ToTxEnv + RecoveredTx` - covers `&Recovered<T>`, `&WithEncoded<...>`, etc.
pub trait ExecutableTxParts<TxEnv, T> {
    /// The recovered transaction accessor type.
    type Recovered: RecoveredTx<T>;

    /// Converts the transaction into the executable transaction environment (`TxEnv`) and the
    /// original recovered transaction.
    fn into_parts(self) -> (TxEnv, Self::Recovered);
}

/// Blanket implementation for references to types implementing both [`ToTxEnv`] and
/// [`RecoveredTx`].
///
/// This covers:
/// - `&Recovered<T>` and `&Recovered<&T>`
/// - `&WithEncoded<Recovered<T>>` and similar wrappers
/// - Any `&S` where `S: ToTxEnv<TxEnv> + RecoveredTx<T>`
impl<'a, S, TxEnv, T> ExecutableTxParts<TxEnv, T> for &'a S
where
    S: ToTxEnv<TxEnv> + RecoveredTx<T>,
{
    type Recovered = &'a S;

    fn into_parts(self) -> (TxEnv, &'a S) {
        (self.to_tx_env(), self)
    }
}

impl<TxEnv, T: RecoveredTx<Tx>, Tx> ExecutableTxParts<TxEnv, Tx> for (TxEnv, T) {
    type Recovered = T;

    fn into_parts(self) -> (TxEnv, T) {
        (self.0, self.1)
    }
}

impl<T, TxEnv: FromRecoveredTx<T>> ExecutableTxParts<TxEnv, T> for Recovered<T> {
    type Recovered = Self;

    fn into_parts(self) -> (TxEnv, Self) {
        (self.to_tx_env(), self)
    }
}

impl<T, TxEnv: FromRecoveredTx<T>> ExecutableTxParts<TxEnv, T> for Recovered<&T> {
    type Recovered = Self;

    fn into_parts(self) -> (TxEnv, Self) {
        (self.to_tx_env(), self)
    }
}

impl<T, TxEnv: FromTxWithEncoded<T>> ExecutableTxParts<TxEnv, T> for WithEncoded<Recovered<T>> {
    type Recovered = Self;

    fn into_parts(self) -> (TxEnv, Self) {
        (self.to_tx_env(), self)
    }
}

impl<T, TxEnv: FromTxWithEncoded<T>> ExecutableTxParts<TxEnv, T> for WithEncoded<&Recovered<T>> {
    type Recovered = Self;

    fn into_parts(self) -> (TxEnv, Self) {
        (self.to_tx_env(), self)
    }
}

impl<L, R, TxEnv, T> ExecutableTxParts<TxEnv, T> for Either<L, R>
where
    L: ExecutableTxParts<TxEnv, T>,
    R: ExecutableTxParts<TxEnv, T>,
{
    type Recovered = Either<L::Recovered, R::Recovered>;

    fn into_parts(self) -> (TxEnv, Self::Recovered) {
        match self {
            Self::Left(l) => {
                let (env, rec) = l.into_parts();
                (env, Either::Left(rec))
            }
            Self::Right(r) => {
                let (env, rec) = r.into_parts();
                (env, Either::Right(rec))
            }
        }
    }
}

/// A transaction that can be executed by the Base block executor.
pub trait ExecutableTx:
    ExecutableTxParts<crate::BaseTransaction, base_common_types_chain::BaseTxEnvelope>
{
}

impl<T> ExecutableTx for T where
    T: ExecutableTxParts<crate::BaseTransaction, base_common_types_chain::BaseTxEnvelope>
{
}

/// Marks whether transaction should be committed into block executor's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum CommitChanges {
    /// Transaction should be committed into block executor's state.
    Yes,
    /// Transaction should not be committed.
    No,
}

impl CommitChanges {
    /// Returns `true` if transaction should be committed into block executor's state.
    pub const fn should_commit(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Result of transaction execution.
pub trait TxResult: Send + 'static {
    /// Halt reason.
    type HaltReason: Send + 'static;

    /// Returns the inner EVM result.
    fn result(&self) -> &ResultAndState<Self::HaltReason>;

    /// Consumes self and returns the inner EVM result.
    fn into_result(self) -> ResultAndState<Self::HaltReason>;
}
