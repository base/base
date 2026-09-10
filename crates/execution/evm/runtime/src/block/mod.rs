//! Block execution abstraction.

use alloy_eips::eip2718::WithEncoded;
use base_common_types_chain::transaction::Recovered;
use base_execution_evm_runtime::{
    Evm, FromRecoveredTx, FromTxWithEncoded, RecoveredTx, ResultAndState, ToTxEnv, either::Either,
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

/// Helper trait to encapsulate requirements for a type to be used as input for [`BlockExecutor`].
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

/// Alias for the [`ExecutableTxParts`] trait with types associated with the given
/// [`BlockExecutor`].
pub trait ExecutableTx<E: BlockExecutor + ?Sized>:
    ExecutableTxParts<<E::Evm as Evm>::Tx, E::Transaction>
{
}

impl<E: BlockExecutor + ?Sized, T> ExecutableTx<E> for T where
    T: ExecutableTxParts<<E::Evm as Evm>::Tx, E::Transaction>
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

/// A type that knows how to execute a single block.
///
/// The current abstraction assumes that block execution consists of the following steps:
/// 1. Apply pre-execution changes. Those might include system calls, irregular state transitions
///    (DAO fork), etc.
/// 2. Apply block transactions to the state.
/// 3. Apply post-execution changes and finalize the state. This might include other system calls,
///    block rewards, etc.
///
/// The output of [`BlockExecutor::finish`] is a [`BlockExecutionResult`] which contains all
/// relevant information about the block execution.
pub trait BlockExecutor {
    /// Input transaction type.
    ///
    /// This represents the consensus transaction type that the block executor operates on.
    /// It's typically a type from the consensus layer (e.g.,
    /// [`EthereumTxEnvelope`](base_common_types_chain::EthereumTxEnvelope)) that contains
    /// the raw transaction data, signature, and other consensus-level information.
    ///
    /// This type is used in several contexts:
    /// - As the generic parameter for [`RecoveredTx<T>`](crate::RecoveredTx) in [`ExecutableTx`]
    /// - As the generic parameter for [`FromRecoveredTx<T>`](crate::FromRecoveredTx) and
    ///   [`FromTxWithEncoded<T>`](crate::FromTxWithEncoded) in the EVM constraint
    /// - To generate receipts after transaction execution
    ///
    /// The transaction flow is:
    /// 1. `Self::Transaction` (consensus tx) →
    ///    [`Recovered<Self::Transaction>`](base_common_types_chain::transaction::Recovered) (with sender)
    /// 2. [`Recovered<Self::Transaction>`](base_common_types_chain::transaction::Recovered) →
    ///    [`TxEnv`](base_execution_evm_runtime::TxEnv) (via [`FromRecoveredTx`])
    /// 3. [`TxEnv`](base_execution_evm_runtime::TxEnv) → EVM execution → [`Self::Result`](BlockExecutor::Result)
    /// 4. [`Self::Result`](BlockExecutor::Result) + `Self::Transaction` → `Self::Receipt`
    ///
    /// Common examples:
    /// - [`EthereumTxEnvelope`](base_common_types_chain::EthereumTxEnvelope) for all Ethereum transaction
    ///   variants
    /// - `OpTxEnvelope` for opstack transaction variants
    type Transaction;
    /// Receipt type this executor produces.
    type Receipt;
    /// EVM used by the executor.
    ///
    /// The EVM's transaction type (`Evm::Tx`) must be able to be constructed from both:
    /// - [`FromRecoveredTx<Self::Transaction>`](crate::FromRecoveredTx) - for transactions with
    ///   recovered senders
    /// - [`FromTxWithEncoded<Self::Transaction>`](crate::FromTxWithEncoded) - for transactions with
    ///   encoded bytes
    ///
    /// This constraint ensures that the block executor can convert consensus transactions
    /// into the EVM's transaction format for execution.
    type Evm: Evm<Tx: FromRecoveredTx<Self::Transaction> + FromTxWithEncoded<Self::Transaction>>;
    /// Result of a transaction execution.
    type Result: TxResult<HaltReason = <Self::Evm as Evm>::HaltReason>;

    /// Applies any necessary changes before executing the block's transactions.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError>;

    /// Executes a single transaction and applies execution result to internal state.
    ///
    /// This method accepts any type implementing [`ExecutableTx`], which ensures the transaction:
    /// - Can be converted to the EVM's transaction environment for execution
    /// - Provides access to the original transaction and signer for receipt generation
    ///
    /// Common input types include:
    /// - `&Recovered<Transaction>` - A transaction with its recovered sender
    /// - `&WithEncoded<Recovered<Transaction>>` - A transaction with sender and encoded bytes
    ///
    /// The transaction is executed in the EVM, state changes are committed, and a receipt
    /// is generated internally.
    ///
    /// Returns the gas used by the transaction.
    fn execute_transaction(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_result_closure(tx, |_| ())
    }

    /// Executes a single transaction at its zero-based index in the block and applies execution
    /// result to internal state.
    ///
    /// This is equivalent to [`execute_transaction`](Self::execute_transaction), but first sets
    /// the underlying database's BAL index for the transaction's position in the block. BAL uses
    /// index `0` for pre-execution changes, transaction indexes are shifted by one (`tx_index + 1`)
    /// and post-execution changes use the index after the last transaction. This means transaction
    /// `0` is executed at BAL index `1`, transaction `1` at BAL index `2`, and so on.
    ///
    /// Callers that execute transactions individually should prefer this method when the
    /// transaction index in the block is known, so BAL reads and writes are attributed to the
    /// correct EIP-7928 index.
    fn execute_transaction_with_index(
        &mut self,
        tx: impl ExecutableTx<Self>,
        tx_index: usize,
    ) -> Result<GasOutput, BlockExecutionError>
    where
        <Self::Evm as Evm>::DB: BalIndexedDatabase,
    {
        self.execute_transaction_with_index_and_result_closure(tx, tx_index, |_| ())
    }

    /// Executes a single transaction at its zero-based index in the block and invokes the given
    /// closure with the internal [`Self::Result`](BlockExecutor::Result) produced by the EVM.
    ///
    /// This is the indexed counterpart to
    /// [`execute_transaction_with_result_closure`](Self::execute_transaction_with_result_closure).
    /// It first sets the underlying database's BAL index for the transaction's position in the
    /// block, then executes and commits the transaction while exposing the raw execution result to
    /// the closure.
    ///
    /// BAL uses index `0` for pre-execution changes, transaction indexes are shifted by one
    /// (`tx_index + 1`) and post-execution changes use the index after the last transaction. This
    /// means transaction `0` is executed at BAL index `1`, transaction `1` at BAL index `2`, and so
    /// on.
    fn execute_transaction_with_index_and_result_closure(
        &mut self,
        tx: impl ExecutableTx<Self>,
        tx_index: usize,
        f: impl FnOnce(&Self::Result),
    ) -> Result<GasOutput, BlockExecutionError>
    where
        <Self::Evm as Evm>::DB: BalIndexedDatabase,
    {
        self.evm_mut().db_mut().set_bal_index(tx_index as u64 + 1);
        self.execute_transaction_with_result_closure(tx, f)
    }

    /// Executes a single transaction and applies execution result to internal state. Invokes the
    /// given closure with an internal [`Self::Result`](BlockExecutor::Result) produced by the EVM.
    ///
    /// This method is similar to [`execute_transaction`](Self::execute_transaction) but provides
    /// access to the raw execution result before it's converted to a receipt. This is useful for:
    /// - Custom logging or metrics collection
    /// - Debugging transaction execution
    /// - Extracting additional information from the execution result
    ///
    /// The transaction is always committed after the closure is invoked.
    fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result),
    ) -> Result<GasOutput, BlockExecutionError> {
        self.execute_transaction_with_commit_condition(tx, |res| {
            f(res);
            CommitChanges::Yes
        })
        .map(Option::unwrap_or_default)
    }

    /// Executes a single transaction and applies execution result to internal state. Invokes the
    /// given closure with an internal [`Self::Result`](BlockExecutor::Result) produced by the EVM,
    /// and commits the transaction to the state on [`CommitChanges::Yes`].
    ///
    /// This is the most flexible transaction execution method, allowing conditional commitment
    /// based on the execution result. The closure receives the execution result and returns
    /// whether to commit the changes to state.
    ///
    /// Use cases:
    /// - Conditional execution based on transaction outcome
    /// - Simulating transactions without committing
    /// - Custom validation logic before committing
    ///
    /// The [`ExecutableTx`] constraint ensures that:
    /// 1. The transaction can be converted to `TxEnv` via [`ToTxEnv`] for EVM execution
    /// 2. The original transaction and signer can be accessed via [`RecoveredTx`] for receipt
    ///    generation
    ///
    /// Returns [`None`] if committing changes from the transaction should be skipped via
    /// [`CommitChanges::No`], otherwise returns the gas used by the transaction.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        // Execute transaction without committing
        let output = self.execute_transaction_without_commit(tx)?;

        if !f(&output).should_commit() {
            return Ok(None);
        }

        let gas_used = self.commit_transaction(output);
        Ok(Some(gas_used))
    }

    /// Executes a single transaction without committing state changes.
    ///
    /// This method performs the transaction execution through the EVM but does not
    /// commit the resulting state changes. The output can be inspected and potentially
    /// committed later using [`commit_transaction`](Self::commit_transaction).
    ///
    /// Returns a [`base_execution_evm_runtime::ResultAndState`] containing the execution
    /// result and state changes.
    ///
    /// # Use Cases
    /// - Transaction simulation without affecting state
    /// - Inspecting transaction effects before committing
    /// - Building custom commit logic
    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError>;

    /// Commits a previously executed transaction's state changes.
    ///
    /// Takes the output from
    /// [`execute_transaction_without_commit`](Self::execute_transaction_without_commit)
    /// and applies the state changes, updates gas accounting, and generates a receipt.
    ///
    /// Returns the gas used by the transaction (including both regular and state gas).
    ///
    /// # Parameters
    /// - `output`: The transaction output containing execution result and state changes
    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput;

    /// Applies any necessary changes after executing the block's transactions, completes execution
    /// and returns the underlying EVM along with execution result.
    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError>;

    /// A helper to invoke [`BlockExecutor::finish`] returning only the [`BlockExecutionResult`].
    fn apply_post_execution_changes(
        self,
    ) -> Result<BlockExecutionResult<Self::Receipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.finish().map(|(_, result)| result)
    }

    /// Exposes mutable reference to EVM.
    fn evm_mut(&mut self) -> &mut Self::Evm;

    /// Exposes immutable reference to EVM.
    fn evm(&self) -> &Self::Evm;

    /// Returns a reference to all recorded receipts.
    fn receipts(&self) -> &[Self::Receipt];

    /// Executes all transactions in a block, applying pre and post execution changes.
    ///
    /// This is a convenience method that orchestrates the complete block execution flow:
    /// 1. Applies pre-execution changes (system calls, irregular state transitions)
    /// 2. Executes all transactions in order
    /// 3. Applies post-execution changes (block rewards, system calls)
    ///
    /// Each transaction in the iterator must implement [`ExecutableTx`], ensuring it can be:
    /// - Converted to the EVM's transaction format for execution
    /// - Used to generate receipts with access to the original transaction data
    ///
    /// # Example
    ///
    /// ```ignore
    /// let recovered_txs: Vec<Recovered<Transaction>> = block.transactions
    ///     .iter()
    ///     .map(|tx| tx.recover_signer())
    ///     .collect::<Result<_, _>>()?;
    ///
    /// let result = executor.execute_block(recovered_txs.iter())?;
    /// ```
    fn execute_block(
        mut self,
        transactions: impl IntoIterator<Item = impl ExecutableTx<Self>>,
    ) -> Result<BlockExecutionResult<Self::Receipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.apply_pre_execution_changes()?;

        for tx in transactions {
            self.execute_transaction(tx)?;
        }

        self.apply_post_execution_changes()
    }
}

/// A result of transaction execution.
pub trait TxResult: Send + 'static {
    /// Halt reason.
    type HaltReason: Send + 'static;

    /// Returns the inner EVM result.
    fn result(&self) -> &ResultAndState<Self::HaltReason>;

    /// Consumes self and returns the inner EVM result.
    fn into_result(self) -> ResultAndState<Self::HaltReason>;
}
