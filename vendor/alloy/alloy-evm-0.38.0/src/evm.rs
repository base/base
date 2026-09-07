//! Abstraction over EVM.

use crate::{env::BlockEnvironment, tracing::TxTracer, EvmEnv, EvmError, IntoTxEnv};
use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{Address, Bytes, B256};
use core::{error::Error, fmt::Debug, hash::Hash};
use revm::{
    context::{result::ExecutionResult, CfgEnv, DBErrorMarker},
    context_interface::{
        result::{HaltReasonTr, ResultAndState},
        ContextTr,
    },
    inspector::{JournalExt, NoOpInspector},
    DatabaseCommit, Inspector,
};

/// Helper trait to bound [`revm::Database::Error`] with common requirements.
pub trait Database: revm::Database<Error: Error + Send + Sync + 'static> + Debug {}
impl<T> Database for T where T: revm::Database<Error: Error + Send + Sync + 'static> + Debug {}

/// An instance of an ethereum virtual machine.
///
/// An EVM is commonly initialized with the corresponding block context and state and it's only
/// purpose is to execute transactions.
///
/// Executing a transaction will return the outcome of the transaction.
pub trait Evm {
    /// Database type held by the EVM.
    type DB: Database;
    /// The transaction object that the EVM will execute.
    ///
    /// This type represents the transaction environment that the EVM operates on internally.
    /// Typically this is [`revm::context::TxEnv`], which contains all necessary transaction
    /// data like sender, gas limits, value, and calldata.
    ///
    /// The EVM accepts flexible transaction inputs through the [`IntoTxEnv`] trait. This means
    /// that while the EVM internally works with `Self::Tx` (usually `TxEnv`), users can pass
    /// various transaction formats to [`Evm::transact`], including:
    /// - Direct [`TxEnv`](revm::context::TxEnv) instances
    /// - [`Recovered<T>`](alloy_consensus::transaction::Recovered) where `T` implements
    ///   [`crate::FromRecoveredTx`]
    /// - [`WithEncoded<Recovered<T>>`](alloy_eips::eip2718::WithEncoded) where `T` implements
    ///   [`crate::FromTxWithEncoded`]
    ///
    /// This design allows the EVM to accept recovered consensus transactions seamlessly.
    type Tx: IntoTxEnv<Self::Tx>;
    /// Error type returned by EVM. Contains either errors related to invalid transactions or
    /// internal irrecoverable execution errors.
    type Error: EvmError;
    /// Halt reason. Enum over all possible reasons for halting the execution. When execution halts,
    /// it means that transaction is valid, however, it's execution was interrupted (e.g because of
    /// running out of gas or overflowing stack).
    type HaltReason: HaltReasonTr + Send + Sync + 'static;
    /// Identifier of the EVM specification. EVM is expected to use this identifier to determine
    /// which features are enabled.
    type Spec: Debug + Copy + Hash + Eq + Send + Sync + Default + 'static;
    /// Block environment used by the EVM.
    type BlockEnv: BlockEnvironment + Clone;
    /// Precompiles used by the EVM.
    type Precompiles;
    /// Evm inspector.
    type Inspector;

    /// Reference to [`Evm::BlockEnv`].
    fn block(&self) -> &Self::BlockEnv;

    /// Reference to [`CfgEnv`].
    fn cfg_env(&self) -> &CfgEnv<Self::Spec>;

    /// Returns the chain ID of the environment.
    fn chain_id(&self) -> u64;

    /// Executes a transaction and returns the outcome.
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;

    /// Same as [`Evm::transact_raw`], but takes any type implementing [`IntoTxEnv`].
    ///
    /// This is the primary method for executing transactions. It accepts flexible input types
    /// that can be converted to the EVM's transaction environment, including:
    /// - [`TxEnv`](revm::context::TxEnv) - Direct transaction environment
    /// - [`Recovered<T>`](alloy_consensus::transaction::Recovered) - Consensus transaction with
    ///   recovered sender
    /// - [`WithEncoded<Recovered<T>>`](alloy_eips::eip2718::WithEncoded) - Transaction with sender
    ///   and encoded bytes
    ///
    /// The conversion happens automatically through the [`IntoTxEnv`] trait.
    fn transact(
        &mut self,
        tx: impl IntoTxEnv<Self::Tx>,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.transact_raw(tx.into_tx_env())
    }

    /// Executes a system call.
    ///
    /// Note: this will only keep the target `contract` in the state. This is done because revm is
    /// loading [`revm::context::Block::beneficiary`] into state by default, and we need to avoid it
    /// by also covering edge cases when beneficiary is set to the system contract address.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;

    /// Returns an immutable reference to the underlying database.
    fn db(&self) -> &Self::DB {
        self.components().0
    }

    /// Returns a mutable reference to the underlying database.
    fn db_mut(&mut self) -> &mut Self::DB {
        self.components_mut().0
    }

    /// Executes a transaction and commits the state changes to the underlying database.
    fn transact_commit(
        &mut self,
        tx: impl IntoTxEnv<Self::Tx>,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error>
    where
        Self::DB: DatabaseCommit,
    {
        let ResultAndState { result, state } = self.transact(tx)?;
        self.db_mut().commit(state);

        Ok(result)
    }

    /// Consumes the EVM and returns the inner [`EvmEnv`].
    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>)
    where
        Self: Sized;

    /// Consumes the EVM and returns the inner database.
    fn into_db(self) -> Self::DB
    where
        Self: Sized,
    {
        self.finish().0
    }

    /// Consumes the EVM and returns the inner [`EvmEnv`].
    fn into_env(self) -> EvmEnv<Self::Spec, Self::BlockEnv>
    where
        Self: Sized,
    {
        self.finish().1
    }

    /// Determines whether additional transactions should be inspected or not.
    ///
    /// See also [`EvmFactory::create_evm_with_inspector`].
    fn set_inspector_enabled(&mut self, enabled: bool);

    /// Enables the configured inspector.
    ///
    /// All additional transactions will be inspected if enabled.
    fn enable_inspector(&mut self) {
        self.set_inspector_enabled(true)
    }

    /// Disables the configured inspector.
    ///
    /// Transactions will no longer be inspected.
    fn disable_inspector(&mut self) {
        self.set_inspector_enabled(false)
    }

    /// Getter of precompiles.
    fn precompiles(&self) -> &Self::Precompiles {
        self.components().2
    }

    /// Mutable getter of precompiles.
    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        self.components_mut().2
    }

    /// Getter of inspector.
    fn inspector(&self) -> &Self::Inspector {
        self.components().1
    }

    /// Mutable getter of inspector.
    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        self.components_mut().1
    }

    /// Provides immutable references to the database, inspector and precompiles.
    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles);

    /// Provides mutable references to the database, inspector and precompiles.
    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles);
}

/// An extension trait for [`Evm`] providing additional functionality.
pub trait EvmExt: Evm {
    /// Replays all the transactions until the target transaction is found.
    ///
    /// This stops before transacting the target hash and commits all previous changes.
    ///
    /// Returns the index of the target transaction in the iterator.
    fn replay_transactions_until<I, T>(
        &mut self,
        transactions: I,
        target_tx_hash: B256,
    ) -> Result<usize, Self::Error>
    where
        Self::DB: DatabaseCommit,
        I: IntoIterator<Item = T>,
        T: IntoTxEnv<Self::Tx> + TxHashRef,
    {
        let mut index = 0;
        for tx in transactions {
            if *tx.tx_hash() == target_tx_hash {
                // reached the target transaction
                break;
            }
            self.transact_commit(tx)?;
            index += 1;
        }
        Ok(index)
    }

    /// Replays all the previous transactions and returns the [`ResultAndState`] of the target
    /// transaction.
    ///
    /// Returns `None` if the target transaction was not found.
    fn replay_transaction<I, T>(
        &mut self,
        transactions: I,
        target_tx_hash: B256,
    ) -> Result<Option<ResultAndState<Self::HaltReason>>, Self::Error>
    where
        Self::DB: DatabaseCommit,
        I: IntoIterator<Item = T>,
        T: IntoTxEnv<Self::Tx> + TxHashRef,
    {
        for tx in transactions {
            if *tx.tx_hash() == target_tx_hash {
                // reached the target transaction
                return self.transact(tx).map(Some);
            } else {
                self.transact_commit(tx)?;
            }
        }
        Ok(None)
    }
}

/// Automatic implementation of [`EvmExt`] for all types that implement [`Evm`].
impl<T: Evm> EvmExt for T {}

/// A type responsible for creating instances of an ethereum virtual machine given a certain input.
pub trait EvmFactory {
    /// The EVM type that this factory creates.
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>>: Evm<
        DB = DB,
        Tx = Self::Tx,
        HaltReason = Self::HaltReason,
        Error = Self::Error<DB::Error>,
        Spec = Self::Spec,
        BlockEnv = Self::BlockEnv,
        Precompiles = Self::Precompiles,
        Inspector = I,
    >;

    /// The EVM context for inspectors.
    ///
    /// The context may use a database adapter around `DB`, while the EVM continues to expose and
    /// return the original database as [`Evm::DB`].
    type Context<DB: Database>: ContextTr<Journal: JournalExt>;
    /// Transaction environment.
    type Tx: IntoTxEnv<Self::Tx>;
    /// EVM error. See [`Evm::Error`].
    type Error<DBError: DBErrorMarker>: EvmError;
    /// Halt reason. See [`Evm::HaltReason`].
    type HaltReason: HaltReasonTr + Send + Sync + 'static;
    /// The EVM specification identifier, see [`Evm::Spec`].
    type Spec: Debug + Copy + Hash + Eq + Send + Sync + Default + 'static;
    /// Block environment used by the EVM. See [`Evm::BlockEnv`].
    type BlockEnv: BlockEnvironment + Clone;
    /// Precompiles used by the EVM.
    type Precompiles;

    /// Creates a new instance of an EVM.
    fn create_evm<DB: Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector>;

    /// Creates a new instance of an EVM with an inspector.
    ///
    /// Note: It is expected that the [`Inspector`] is usually provided as `&mut Inspector` so that
    /// it remains owned by the call site when [`Evm::transact`] is invoked.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I>;
}

/// An extension trait for [`EvmFactory`] providing useful non-overridable methods.
pub trait EvmFactoryExt: EvmFactory {
    /// Creates a new [`TxTracer`] instance with the given database, input and fused inspector.
    fn create_tracer<DB, I>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        fused_inspector: I,
    ) -> TxTracer<Self::Evm<DB, I>>
    where
        DB: Database + DatabaseCommit,
        I: Inspector<Self::Context<DB>> + Clone,
    {
        TxTracer::new(self.create_evm_with_inspector(db, input, fused_inspector))
    }
}

impl<T: EvmFactory> EvmFactoryExt for T {}
