//! EVM execution host.
//!
//! This module exposes the [`Evm`] dispatcher, transaction result types, database adapters,
//! state-change streaming traits, and block-state accumulator used by the host.
//!
//! ## State output and transaction lifecycle
//!
//! Transaction execution separates execution output from state materialization. [`Evm::transact`]
//! validates and executes a transaction through the registered handler, finalizes transaction-level
//! effects, and returns an [`ExecutedTx`] handle. The handle owns no copied write-set; it keeps the
//! transaction's post-finalization writes in reusable scratch state until the caller chooses how to
//! resolve them.
//!
//! Resolve an [`ExecutedTx`] with one of these methods:
//!
//! - [`ExecutedTx::commit`] accepts the transaction into the accepted overlay;
//! - [`ExecutedTx::commit_to`] accepts it and records the same writes in a
//!   [`BlockStateAccumulator`];
//! - [`ExecutedTx::commit_with`] streams writes to a [`StateChangeSink`] and then accepts them;
//! - [`ExecutedTx::discard`] drops the writes and returns only the result;
//! - [`ExecutedTx::discard_with`] streams writes to a [`StateChangeSink`] and then drops them;
//! - [`ExecutedTx::detach`] moves the pending transaction overlay out as an owned
//!   [`TxResultWithState`] without accepting the writes.
//!
//! Dropping an unresolved [`ExecutedTx`] is equivalent to [`ExecutedTx::discard`], so transaction
//! scratch cannot leak into later execution.
//!
//! ## State layers
//!
//! The host state is split into three layers:
//!
//! 1. **Accepted overlay**: transaction-boundary state accepted by prior commits. It shadows the
//!    wrapped database and is visible to later transactions executed by the same [`Evm`].
//! 2. **Transaction scratch**: writes, warm-access state, transient storage, journal entries,
//!    touched accounts, selfdestruct markers, and logs for the currently executing transaction. It
//!    is cleared after `commit`, `commit_to`, `commit_with`, `discard`, `discard_with`, or `detach`
//!    while retaining capacity where possible.
//! 3. **Block accumulator**: optional block-level state output. It coalesces committed transaction
//!    writes and keeps block-boundary originals.
//!
//! The accepted overlay is for execution correctness between transactions. The block accumulator
//! is for final block output.
//!
//! ## Outcomes, logs, and detached state
//!
//! [`TxResult`] is the cheap result-only shape: status, gas used, output, stop reason, logs,
//! host error code, and extension data. Logs live in [`TxResult`] because logs are
//! execution output, not database state.
//!
//! [`PendingState`] is the owned transaction overlay, moved out by [`ExecutedTx::detach`]. Normal
//! serial block execution can build receipts from [`TxResult`] and stream state directly into a
//! [`BlockStateAccumulator`] without detaching a per-transaction [`PendingState`].
//!
//! ## Source and sink API
//!
//! [`StateChangeSource`] and [`StateChangeSink`] provide borrowed state-change streaming. Sources
//! include transaction scratch, [`PendingState`], and [`BlockStateAccumulator`]. Sinks include
//! [`BlockStateAccumulator`], [`CacheDB`], [`Tee`], and custom consumers such as trie updaters,
//! witnesses, execution caches, or test recorders.
//!
//! The common hot path can therefore stream the same transaction writes into multiple consumers
//! without cloning or materializing the write-set first.
//!
//! ## Error and status behavior
//!
//! - Successful execution returns an [`ExecutedTx`] whose result has `status = true`.
//! - EVM revert/halt can still return an [`ExecutedTx`]. The result records the failed
//!   status/stop/output, while transaction-level effects remain resolvable if finalization
//!   completed.
//! - Invalid transactions and handler errors return a handler error and clear transaction scratch;
//!   there is no [`ExecutedTx`] to resolve.
//! - Host errors during execution/finalization are recorded as compact error codes. The owning
//!   component can recover the full error from that code.
//!
//! ## Common flows
//!
//! ```text
//! eth_call / simulation: transact -> discard
//! serial block:          transact -> commit
//! block output:          transact -> commit_to -> BlockStateAccumulator
//! traced simulation:     transact -> discard_with -> Sink
//! parallel worker:       transact -> detach -> send owned TxResultWithState
//! ```
//!
//! Result-only execution:
//!
//! ```rust,ignore
//! let executed = evm.transact(&tx)?;
//! let outcome = executed.discard();
//! ```
//!
//! Serial block execution with coalesced block output:
//!
//! ```rust,ignore
//! let mut block_state = BlockStateAccumulator::new();
//!
//! for tx in block.transactions() {
//!     let executed = evm.transact(tx)?;
//!     receipt_builder.observe(executed.result());
//!     let outcome = executed.commit_to(&mut block_state);
//!     receipts.push(receipt_builder.finish(outcome));
//! }
//!
//! let storage_deltas = block_state.storage_sorted();
//! ```
//!
//! Detached materialized output:
//!
//! ```rust,ignore
//! let result: TxResultWithState<_> = evm.transact(&tx)?.detach();
//! ```

use alloc::{boxed::Box, sync::Arc, vec};
#[cfg(feature = "async")]
use core::future::Future;
use core::ptr::NonNull;

use alloy_consensus::transaction::Recovered;
use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{Address, B256, Bytes, Log, LogData};
use derive_where::derive_where;

use self::{
    inspector::{Inspector, boxed_inspector},
    precompile::{PrecompileOutput, PrecompileProvider, boxed_precompile_provider},
};
use crate::{
    AnyError, ErrorCode, EvmConfigSelector, EvmTypes, EvmTypesHost, ExecutionConfig,
    PrecompileError, PrecompileHalt, SpecId,
    bytecode::Bytecode,
    constants::{CALL_DEPTH_LIMIT, EIP7708_TRANSFER_TOPIC},
    env::{BlockEnv, TxEnv},
    error::error_unavailable,
    interpreter::{
        Gas, GasTracker, Host, InstrStop, Interpreter, InterpreterPool, Message, MessageKind,
        MessageResult, MessageResultExt, Word,
    },
    registry::{HandlerError, HandlerResult, TxRegistry},
    trustme,
    version::{EvmFeatures, GasId},
};

#[cfg(feature = "async")]
pub mod r#async;
pub mod config;
pub mod env;
pub mod handler;
pub mod inspector;
pub mod precompile;
pub mod registry;
mod system;
pub use system::{
    BEACON_ROOTS_ADDRESS, BUILDER_DEPOSIT_REQUEST_ADDRESS, BUILDER_EXIT_REQUEST_ADDRESS,
    CONSOLIDATION_REQUEST_ADDRESS, HISTORY_STORAGE_ADDRESS, SYSTEM_ADDRESS, SYSTEM_CALL_GAS_LIMIT,
    SYSTEM_MAX_SSTORES_PER_CALL, SystemTx, WITHDRAWAL_REQUEST_ADDRESS,
};

mod any;
pub use any::NonStaticAny;

pub mod bal;
pub use bal::{
    AccountBal, AccountInfoBal, Bal, BalChange, BalChanges, BalCodeChange, BalContext, BalError,
    BlockAccessIndex, StorageBal,
};

mod db;
use db::boxed_dyn_database;
pub use db::{
    AccountStorageCache, Cache, CacheDB, Database, Db, DbResult, DbStats, DbStatsCounts,
    DynDatabase, EmptyDB, InMemoryDB,
};

mod tx;
pub use tx::{ExecutedTx, TxResult, TxResultExt, TxResultWithState};

mod state;
pub use state::{
    AccountChangeRef, AccountHandle, AccountInfo, BlockStateAccumulator, JournalEntry,
    NoopChangeSink, PendingState, State, StateChangeSink, StateChangeSource, StateCheckpoint,
    StateInner, StorageChange, StorageHandle, StorageOverlay, StorageSlot, StorageSlotHandle, Tee,
    Tracked,
};

mod prewarm_set;
pub use prewarm_set::PrewarmSet;

/// Builds a `map_err` closure that records the error code on `$host` and returns
/// [`registry::HandlerError::Fatal`].
///
/// This expands to a closure that records the code through a disjoint borrow of
/// `$host.error_code` rather than calling a `&mut self` method, so Rust 2021 disjoint closure
/// capture borrows only `$host.error_code`. That lets it be used in `.map_err(..)` on a
/// `Result` that already mutably borrows another field of `$host` (such as `$host.state` through a
/// live [`AccountHandle`]), where a closure calling a `&mut self` method would conflict on the
/// whole `$host` borrow.
macro_rules! error_handler {
    ($host:expr) => {
        |code| {
            $host.error_code = ::core::option::Option::Some(code);
            $crate::registry::HandlerError::Fatal(code)
        }
    };
}
pub(crate) use error_handler;

/// Inlined [`Evm::store_error`] that records the error code and yields
/// [`InstrStop::FatalExternalError`] through a disjoint borrow of `$host.error_code`.
///
/// Like [`error_handler!`], inlining keeps this from borrowing all of `$host`, so it composes
/// with a live [`AccountHandle`] (or a `Result` carrying one) that already borrows `$host.state`.
macro_rules! store_error {
    ($host:expr, $code:expr) => {{
        $host.error_code = ::core::option::Option::Some($code);
        $crate::interpreter::InstrStop::FatalExternalError
    }};
}

/// Optional external interpreter runner.
///
/// Returning `Some(stop)` means the runner executed the frame. Returning `None` makes the EVM run
/// the regular interpreter for the same frame.
pub trait InterpreterRunner<T: EvmTypesHost>: core::fmt::Debug + Send + Sync + 'static {
    /// Attempts to execute `interpreter` with an external backend.
    fn run<'frame, 'host>(
        &self,
        config: &ExecutionConfig<T>,
        interpreter: &mut Interpreter<'frame, 'host, T>,
        host: &mut T::Host<'host>,
    ) -> Option<InstrStop>;
}

/// EVM host and transaction dispatcher.
#[derive_where(Debug)]
pub struct Evm<'a, T: EvmTypesHost> {
    #[derive_where(skip)]
    spec_id: T::SpecId,
    #[derive_where(skip)]
    execution_config: ExecutionConfig<T>,
    features: EvmFeatures,
    pub(crate) block: BlockEnv<T>,
    registry: TxRegistry<T, TxResult<T>>,
    #[derive_where(skip)]
    pub(crate) state: State<'a>,
    #[derive_where(skip)]
    ext: T::EvmExt,
    #[derive_where(skip)]
    precompiles: Box<dyn PrecompileProvider<T> + 'a>,
    #[derive_where(skip)]
    interpreter_pool: InterpreterPool<T>,
    #[derive_where(skip)]
    inspector: Option<Box<dyn Inspector<T> + 'a>>,
    #[derive_where(skip)]
    interpreter_runner: Option<Arc<dyn InterpreterRunner<T>>>,
    /// The currently running interpreter frame, if any.
    ///
    /// This is passed to the inspector call and create hooks as the parent frame.
    #[derive_where(skip)]
    current_frame: Option<NonNull<Interpreter<'static, 'static, T>>>,
    #[derive_where(skip)]
    running: bool,
    #[cfg(feature = "async")]
    #[derive_where(skip)]
    async_stack: r#async::FiberStack,
    evm_send: bool,
    pub(crate) error_code: Option<ErrorCode>,
    #[derive_where(skip)]
    error: Option<AnyError>,
}

impl<'a, T: EvmTypes> Evm<'a, T> {
    /// Creates an EVM for `spec_id` with the provided transaction registry, database, and
    /// precompile provider.
    #[inline]
    pub fn new(
        spec_id: T::SpecId,
        block: BlockEnv<T>,
        registry: TxRegistry<T, TxResult<T>>,
        database: impl DynDatabase + 'a,
        precompiles: impl PrecompileProvider<T> + 'a,
    ) -> Self
    where
        T::EvmExt: Default,
    {
        Self::new_with_ext(spec_id, block, registry, database, precompiles, T::EvmExt::default())
    }

    /// Creates an EVM with explicit instance-specific extension state.
    #[inline]
    pub fn new_with_ext(
        spec_id: T::SpecId,
        block: BlockEnv<T>,
        registry: TxRegistry<T, TxResult<T>>,
        database: impl DynDatabase + 'a,
        precompiles: impl PrecompileProvider<T> + 'a,
        ext: T::EvmExt,
    ) -> Self {
        Self::new_with_execution_config_and_ext(
            <T::ConfigSelector as EvmConfigSelector<T>>::execution_config(spec_id),
            spec_id,
            block,
            registry,
            database,
            precompiles,
            ext,
        )
    }

    /// Creates an EVM with the provided transaction registry, database, and precompile provider.
    #[inline]
    pub fn new_with_execution_config(
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        block: BlockEnv<T>,
        registry: TxRegistry<T, TxResult<T>>,
        database: impl DynDatabase + 'a,
        precompiles: impl PrecompileProvider<T> + 'a,
    ) -> Self
    where
        T::EvmExt: Default,
    {
        Self::new_with_execution_config_and_ext(
            execution_config,
            spec_id,
            block,
            registry,
            database,
            precompiles,
            T::EvmExt::default(),
        )
    }

    /// Creates an EVM with an execution config and explicit instance-specific extension state.
    #[inline]
    pub fn new_with_execution_config_and_ext(
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        block: BlockEnv<T>,
        registry: TxRegistry<T, TxResult<T>>,
        database: impl DynDatabase + 'a,
        precompiles: impl PrecompileProvider<T> + 'a,
        ext: T::EvmExt,
    ) -> Self {
        Self::new_mono(
            execution_config,
            spec_id,
            block,
            registry,
            boxed_dyn_database(database),
            boxed_precompile_provider(precompiles),
            ext,
        )
    }

    #[inline]
    fn new_mono(
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        block: BlockEnv<T>,
        registry: TxRegistry<T, TxResult<T>>,
        database: Box<dyn DynDatabase + 'a>,
        precompiles: Box<dyn PrecompileProvider<T> + 'a>,
        ext: T::EvmExt,
    ) -> Self {
        assert_eq!(
            spec_id.into(),
            execution_config.base_spec_id(),
            "execution config spec mismatch"
        );
        Self {
            spec_id,
            features: execution_config.version().features,
            execution_config,
            block,
            registry,
            state: State::new_mono(database),
            ext,
            precompiles,
            interpreter_pool: InterpreterPool::new(),
            inspector: None,
            interpreter_runner: None,
            current_frame: None,
            running: false,
            #[cfg(feature = "async")]
            async_stack: r#async::FiberStack::default(),
            evm_send: false,
            error_code: None,
            error: None,
        }
    }

    #[inline]
    fn contains_precompile(&self, message: &Message<T>) -> bool {
        !message.disable_precompiles && self.precompiles.contains(&message.code_address)
    }

    #[inline]
    fn execute_precompile(
        &mut self,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> Result<PrecompileOutput, PrecompileError> {
        let guard = self.enter_execution();
        let precompiles = guard.evm.precompiles.as_mut() as *mut dyn PrecompileProvider<T>;
        let evm_ptr = guard.evm as *mut Self;
        // SAFETY: Precompile execution may need access to both the provider and the host EVM.
        // The provider is not moved or replaced during this call, and `execute` is expected to
        // preserve `Evm` invariants while using the host reference.
        unsafe {
            (&mut *precompiles)
                .execute(&mut *evm_ptr, message, gas)
                .expect("precompile was checked before execution")
        }
    }

    #[inline]
    fn assert_precompiles_mutable(&self) {
        assert!(!self.running, "precompile provider cannot be modified during EVM execution");
    }

    #[inline]
    fn assert_precompiles_downcast_mutable(&self) {
        self.assert_precompiles_mutable();
    }

    #[inline]
    fn assert_inspector_mutable(&self) {
        assert!(!self.running, "inspector cannot be modified during EVM execution");
    }

    #[inline]
    fn assert_interpreter_runner_mutable(&self) {
        assert!(!self.running, "interpreter runner cannot be modified during EVM execution");
    }

    #[inline]
    fn assert_execution_config_mutable(&self) {
        assert!(!self.running, "execution config cannot be modified during EVM execution");
    }

    #[inline]
    const fn enter_execution(&mut self) -> ExecutionGuard<'_, 'a, T> {
        let was_running = self.running;
        self.running = true;
        ExecutionGuard { evm: self, was_running }
    }

    /// Returns the transaction handler registry.
    #[inline]
    pub const fn registry(&self) -> &TxRegistry<T, TxResult<T>> {
        &self.registry
    }

    /// Returns the EVM instance-specific extension state.
    #[inline]
    pub const fn ext(&self) -> &T::EvmExt {
        &self.ext
    }

    /// Returns the EVM instance-specific extension state mutably.
    #[inline]
    pub const fn ext_mut(&mut self) -> &mut T::EvmExt {
        &mut self.ext
    }

    /// Returns the active block environment.
    #[inline]
    pub const fn block(&self) -> &BlockEnv<T> {
        &self.block
    }

    /// Replaces the active block environment, including during execution.
    ///
    /// This changes only the block fields visible to execution. The [`State`] object is kept in
    /// place: its accepted account and storage overlay, backing database, cached block hashes,
    /// attached BAL, BAL builder, and current BAL index all remain unchanged. Transaction
    /// scratch is also left untouched.
    ///
    /// A live replacement is intentional context mutation for testing and simulation hosts. The
    /// current frame and nested frames observe the new environment on subsequent host reads, so
    /// callers can implement controls such as changing the block number or timestamp during a
    /// call. This does not change the active execution specification or its gas rules; use
    /// [`Self::set_execution_config`] only at a frame boundary for a fork transition.
    ///
    /// For normal block execution, call this at a segment boundary so a transaction observes one
    /// coherent block context. The caller is responsible for the resulting semantics when it is
    /// called from an inspector or precompile.
    #[inline]
    pub const fn set_block(&mut self, block: BlockEnv<T>) {
        self.block = block;
    }

    /// Replaces the active execution specification and its fork-dependent dispatch state.
    ///
    /// `execution_config` must have the same base specification as `spec_id`. The transaction
    /// registry and precompile provider are supplied together with the new config so a hardfork
    /// transition cannot silently retain handlers or precompiles from the previous fork.
    ///
    /// The existing [`State`] is preserved in place. In particular, the accepted account and
    /// storage overlay, backing database, cached block hashes, attached BAL, BAL builder, and
    /// current BAL index are not recreated or cleared. The derived feature set is refreshed from
    /// `execution_config.version()`.
    ///
    /// # Panics
    ///
    /// Panics if an interpreter frame is currently executing or if `spec_id` does not match
    /// `execution_config`.
    #[inline]
    pub fn set_execution_config(
        &mut self,
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        registry: TxRegistry<T, TxResult<T>>,
        precompiles: impl PrecompileProvider<T> + 'a,
    ) {
        self.assert_execution_config_mutable();
        assert_eq!(
            spec_id.into(),
            execution_config.base_spec_id(),
            "execution config spec mismatch"
        );
        let precompiles = boxed_precompile_provider(precompiles);
        self.replace_execution_config(execution_config, spec_id, registry, precompiles);
    }

    /// Replaces the block environment and execution specification as one segment-boundary update.
    ///
    /// This is equivalent to [`Self::set_block`] and [`Self::set_execution_config`] performed as
    /// one atomic update. The state, database/cache, block-hash cache, and BAL context are kept in
    /// place; only the supplied block/config/registry/precompile fields are replaced.
    ///
    /// # Panics
    ///
    /// Panics if an interpreter frame is currently executing or if `spec_id` does not match
    /// `execution_config`.
    #[inline]
    pub fn set_block_and_execution_config(
        &mut self,
        block: BlockEnv<T>,
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        registry: TxRegistry<T, TxResult<T>>,
        precompiles: impl PrecompileProvider<T> + 'a,
    ) {
        self.assert_execution_config_mutable();
        assert_eq!(
            spec_id.into(),
            execution_config.base_spec_id(),
            "execution config spec mismatch"
        );
        let precompiles = boxed_precompile_provider(precompiles);
        self.block = block;
        self.replace_execution_config(execution_config, spec_id, registry, precompiles);
    }

    #[inline]
    fn replace_execution_config(
        &mut self,
        execution_config: ExecutionConfig<T>,
        spec_id: T::SpecId,
        registry: TxRegistry<T, TxResult<T>>,
        precompiles: Box<dyn PrecompileProvider<T> + 'a>,
    ) {
        self.spec_id = spec_id;
        self.features = execution_config.version().features;
        self.execution_config = execution_config;
        self.registry = registry;
        self.precompiles = precompiles;
        self.evm_send = false;
    }

    /// Returns the accepted-state overlay database.
    ///
    /// This cache contains state changes committed through transaction lifecycle methods or
    /// [`Self::commit_source`]. The wrapped backing database is available through
    /// [`Self::database`].
    #[inline]
    pub fn overlay_db(&self) -> &CacheDB<Box<dyn DynDatabase + 'a>> {
        self.state.overlay_db()
    }

    /// Returns the accepted-state overlay database mutably.
    ///
    /// This is useful when an external state-change source should be streamed into the accepted
    /// overlay with a [`Tee`] or another [`StateChangeSink`]. The wrapped backing database is
    /// available through [`Self::database_mut`].
    #[inline]
    pub fn overlay_db_mut(&mut self) -> &mut CacheDB<Box<dyn DynDatabase + 'a>> {
        self.state.overlay_db_mut()
    }

    /// Returns the backing database.
    #[inline]
    pub fn database(&self) -> &(dyn DynDatabase + 'a) {
        self.state.initial()
    }

    /// Returns the backing database mutably.
    #[inline]
    pub fn database_mut(&mut self) -> &mut (dyn DynDatabase + 'a) {
        self.state.initial_mut()
    }

    /// Returns the latest host error code raised during execution.
    #[inline]
    pub const fn error_code(&self) -> Option<ErrorCode> {
        self.error_code
    }

    /// Stores the latest host error code raised during execution.
    #[inline]
    pub const fn set_error_code(&mut self, code: ErrorCode) {
        self.error_code = Some(code);
    }

    /// Retrieves the full error for a previously returned error code.
    pub fn error(&mut self, code: ErrorCode) -> AnyError {
        if code == ErrorCode::FATAL_PRECOMPILE {
            if let Some(error) = self.error.clone() {
                return error;
            }
            return error_unavailable(code);
        }
        self.database_mut().error(code)
    }

    /// Returns account information visible through the accepted state overlay.
    #[inline]
    pub fn read_account_info(&mut self, address: &Address) -> DbResult<Option<AccountInfo>> {
        self.state.account_info_untracked(address)
    }

    /// Applies borrowed changes to the accepted state overlay.
    #[inline]
    pub fn commit_source<S: StateChangeSource>(&mut self, source: &S) {
        self.state.commit_source(source);
    }

    /// Replaces the backing database.
    #[inline]
    pub fn set_database(&mut self, database: impl DynDatabase + 'a) {
        self.state.set_initial(database);
        self.evm_send = false;
    }

    #[cfg(feature = "async")]
    #[inline]
    fn async_stack(&mut self) -> core::ptr::NonNull<r#async::FiberStack> {
        core::ptr::NonNull::from(&mut self.async_stack)
    }

    #[cfg(feature = "async")]
    #[inline]
    fn assert_erased_send(&self) {
        assert!(
            self.evm_send,
            "async EVM execution requires EVM erased fields to be verified as Send with \
             Evm::evm_is_send"
        );
    }

    /// Marks this EVM as thread-sendable after checking the current erased field types.
    ///
    /// This requires no active inspector. Use [`Self::evm_is_send_with_inspector`] when an
    /// inspector is installed.
    #[inline]
    pub fn evm_is_send<D, P>(&mut self) -> &mut Self
    where
        D: DynDatabase + Send + 'static,
        P: PrecompileProvider<T> + Send + 'static,
        'a: 'static,
    {
        self.assert_database_type::<D>();
        self.assert_precompiles_type::<P>();
        assert!(self.inspector.is_none(), "inspector type mismatch");
        self.evm_send = true;
        self
    }

    /// Marks this EVM as thread-sendable after checking the current erased field types.
    #[inline]
    pub fn evm_is_send_with_inspector<D, P, I>(&mut self) -> &mut Self
    where
        D: DynDatabase + Send + 'static,
        P: PrecompileProvider<T> + Send + 'static,
        I: Inspector<T> + Send + 'static,
        'a: 'static,
    {
        self.assert_database_type::<D>();
        self.assert_precompiles_type::<P>();
        self.assert_inspector_type::<I>();
        self.evm_send = true;
        self
    }

    #[inline]
    fn assert_database_type<D: DynDatabase + 'static>(&self)
    where
        'a: 'static,
    {
        assert_eq!(self.database().type_id(), typeid::of::<D>(), "database type mismatch");
    }

    #[inline]
    fn assert_precompiles_type<P: PrecompileProvider<T> + 'static>(&self)
    where
        'a: 'static,
    {
        assert_eq!(
            self.precompiles().type_id(),
            typeid::of::<P>(),
            "precompile provider type mismatch"
        );
    }

    #[inline]
    fn assert_inspector_type<I: Inspector<T> + 'static>(&self)
    where
        'a: 'static,
    {
        let inspector = self.inspector().expect("inspector type mismatch");
        assert_eq!(inspector.type_id(), typeid::of::<I>(), "inspector type mismatch");
    }

    /// Returns the backing database as `D` if it has that concrete type.
    #[inline]
    pub fn database_as<D: DynDatabase + 'static>(&self) -> Option<&D>
    where
        'a: 'static,
    {
        self.database().downcast_ref()
    }

    /// Returns the backing database mutably as `D` if it has that concrete type.
    #[inline]
    pub fn database_as_mut<D: DynDatabase + 'static>(&mut self) -> Option<&mut D>
    where
        'a: 'static,
    {
        self.database_mut().downcast_mut()
    }

    /// Returns the mutable EVM state.
    #[inline]
    pub const fn state(&self) -> &State<'a> {
        &self.state
    }

    /// Returns the mutable EVM state.
    #[inline]
    pub const fn state_mut(&mut self) -> &mut State<'a> {
        &mut self.state
    }

    /// Returns logs emitted by the current in-flight transaction.
    #[inline]
    pub fn logs(&self) -> &[Log] {
        self.state.logs()
    }

    /// Returns the precompile provider.
    #[inline]
    pub fn precompiles(&self) -> &(dyn PrecompileProvider<T> + 'a) {
        self.precompiles.as_ref()
    }

    /// Warms every precompile address in the prewarm set.
    ///
    /// Consumes the precompile address iterator directly into the prewarm set. This relies on
    /// disjoint borrows of the `precompiles` and `state` fields, which a caller holding only
    /// `&mut Evm` cannot express through [`Self::precompiles`] (it borrows all of `self`).
    #[inline]
    pub fn warm_precompiles(&mut self) {
        for address in self.precompiles.addresses() {
            self.state.prewarm(&address);
        }
    }

    /// Returns the precompile provider mutably.
    #[inline]
    pub fn precompiles_mut(&mut self) -> &mut (dyn PrecompileProvider<T> + 'a) {
        self.assert_precompiles_mutable();
        self.precompiles.as_mut()
    }

    /// Replaces the precompile provider.
    #[inline]
    pub fn set_precompiles(&mut self, precompiles: impl PrecompileProvider<T> + 'a) {
        self.assert_precompiles_mutable();
        self.precompiles = boxed_precompile_provider(precompiles);
        self.evm_send = false;
    }

    /// Returns the precompile provider as `P` if it has that concrete type.
    #[inline]
    pub fn precompiles_as<P: PrecompileProvider<T> + 'static>(&self) -> Option<&P>
    where
        'a: 'static,
    {
        self.precompiles().downcast_ref()
    }

    /// Returns the precompile provider mutably as `P` if it has that concrete type.
    #[inline]
    pub fn precompiles_as_mut<P: PrecompileProvider<T> + 'static>(&mut self) -> Option<&mut P>
    where
        'a: 'static,
    {
        self.assert_precompiles_downcast_mutable();
        self.precompiles.as_mut().downcast_mut()
    }

    /// Returns the active execution inspector.
    #[inline]
    pub fn inspector(&self) -> Option<&(dyn Inspector<T> + 'a)> {
        self.inspector.as_deref()
    }

    /// Returns the active execution inspector mutably.
    #[inline]
    pub fn inspector_mut(&mut self) -> Option<&mut (dyn Inspector<T> + 'a)> {
        self.assert_inspector_mutable();
        self.inspector.as_deref_mut()
    }

    #[inline]
    fn inspect_log(&mut self, log: &Log) {
        let guard = self.enter_execution();
        if let Some(inspector) = guard.evm.inspector.as_deref_mut() {
            // SAFETY: The inspector is stored in `self`; the execution guard prevents inspector
            // replacement while the hook is running.
            let inspector = unsafe { trustme::decouple_lt_mut(inspector) };
            inspector.log(log, guard.evm);
        }
    }

    #[inline]
    fn emit_log(&mut self, log: Log) {
        self.inspect_log(&log);
        self.state.log(log);
    }

    #[inline]
    fn finalize_transaction(&mut self) -> Result<(), InstrStop> {
        self.state
            .finalize_transaction(self.execution_config.version())
            .map_err(|code| self.store_error(code))
    }

    #[inline]
    fn clear_top_level_error_state(&mut self) {
        self.error_code = None;
        self.error = None;
    }

    #[inline]
    fn finish_executed_tx(&mut self, mut result: TxResult<T>) -> ExecutedTx<'_, 'a, T> {
        let has_pending_state = if let Err(stop) = self.finalize_transaction() {
            result.status = false;
            result.stop = stop;
            result.output = Bytes::new();
            result.logs.clear();
            self.state.clear_transaction_state();
            false
        } else {
            result.logs = self.state.take_logs();
            true
        };
        result.error_code = self.error_code;
        ExecutedTx::from_result(self, result, has_pending_state)
    }

    #[inline(never)]
    fn log_eip7708_transfer(&mut self, from: &Address, to: &Address, value: &Word) {
        if self.feature(EvmFeatures::EIP7708)
            && let Some(log) = eip7708_transfer_log(from, to, value)
        {
            self.emit_log(log);
        }
    }

    /// Sets the active execution inspector.
    #[inline]
    pub fn set_inspector<I: Inspector<T> + 'a>(&mut self, inspector: I) {
        self.assert_inspector_mutable();
        self.inspector = Some(boxed_inspector(inspector));
        self.evm_send = false;
    }

    /// Sets the active boxed execution inspector.
    #[inline]
    pub fn set_boxed_inspector(&mut self, inspector: Box<dyn Inspector<T> + 'a>) {
        self.assert_inspector_mutable();
        self.inspector = Some(inspector);
        self.evm_send = false;
    }

    /// Removes the active execution inspector.
    #[inline]
    pub fn clear_inspector(&mut self) -> Option<Box<dyn Inspector<T> + 'a>> {
        self.assert_inspector_mutable();
        self.evm_send = false;
        self.inspector.take()
    }

    /// Removes the active execution inspector if it has type `I`.
    #[inline]
    pub fn clear_inspector_as<I: Inspector<T> + 'static>(&mut self) -> Option<Box<I>>
    where
        'a: 'static,
    {
        self.assert_inspector_mutable();
        let i = self.inspector.take_if(|i| i.is::<I>())?;
        Some(unsafe { Box::from_raw(Box::into_raw(i).cast::<I>()) })
    }

    /// Sets the optional external interpreter runner.
    #[inline]
    pub fn set_interpreter_runner<R: InterpreterRunner<T>>(&mut self, runner: R) {
        self.assert_interpreter_runner_mutable();
        self.interpreter_runner = Some(Arc::new(runner));
    }

    /// Sets the optional shared external interpreter runner.
    #[inline]
    pub fn set_shared_interpreter_runner(&mut self, runner: Arc<dyn InterpreterRunner<T>>) {
        self.assert_interpreter_runner_mutable();
        self.interpreter_runner = Some(runner);
    }

    /// Removes the optional external interpreter runner.
    #[inline]
    pub fn clear_interpreter_runner(&mut self) -> Option<Arc<dyn InterpreterRunner<T>>> {
        self.assert_interpreter_runner_mutable();
        self.interpreter_runner.take()
    }

    /// Returns the active EVM version.
    #[inline]
    pub const fn version(&self) -> &crate::Version {
        self.execution_config.version()
    }

    /// Returns `true` if the active EVM feature set contains `feature`.
    #[inline]
    pub const fn feature(&self, feature: EvmFeatures) -> bool {
        self.features.contains(feature)
    }

    #[inline]
    const fn store_error(&mut self, code: ErrorCode) -> InstrStop {
        self.set_error_code(code);
        InstrStop::FatalExternalError
    }

    /// Returns the active base specification ID.
    #[inline]
    pub fn spec_id(&self) -> SpecId {
        self.spec_id.into()
    }

    /// Returns the selector-specific runtime specification ID.
    #[inline]
    pub const fn config_spec_id(&self) -> T::SpecId {
        self.spec_id
    }
}

struct ExecutionGuard<'guard, 'evm, T: EvmTypesHost> {
    evm: &'guard mut Evm<'evm, T>,
    was_running: bool,
}

impl<'guard, 'evm, T: EvmTypesHost> Drop for ExecutionGuard<'guard, 'evm, T> {
    #[inline]
    fn drop(&mut self) {
        self.evm.running = self.was_running;
    }
}

#[cfg(feature = "async")]
struct SendEvmRef<'a, 'evm, T: EvmTypesHost> {
    evm: &'a mut Evm<'evm, T>,
}

#[cfg(feature = "async")]
// SAFETY: `SendEvmRef` is only constructed by async entrypoints after `Evm::evm_is_send` has
// verified the concrete erased field types as `Send`.
unsafe impl<T> Send for SendEvmRef<'_, '_, T>
where
    T: EvmTypesHost,
    T::SpecId: Send,
    T::Tx: Send,
    T::EvmExt: Send,
    T::MessageExt: Send,
    T::MessageResultExt: Send,
    T::TxEnvExt: Send,
    T::TxResultExt: Send,
    T::BlockEnvExt: Send,
{
}

impl<'a, T: EvmTypes<Tx: Typed2718>> Evm<'a, T> {
    /// Dispatches the transaction to its handler and returns an executed transaction handle.
    ///
    /// The returned [`ExecutedTx`] keeps post-finalization writes in the transaction scratch layer.
    /// Callers must resolve it with [`ExecutedTx::commit`], [`ExecutedTx::commit_to`],
    /// [`ExecutedTx::commit_with`], [`ExecutedTx::discard`], [`ExecutedTx::discard_with`], or
    /// [`ExecutedTx::detach`] before
    /// another transaction can be executed. Dropping the handle is equivalent to
    /// [`ExecutedTx::discard`].
    pub fn transact(&mut self, tx: &Recovered<T::Tx>) -> HandlerResult<ExecutedTx<'_, 'a, T>> {
        self.clear_top_level_error_state();
        let handler = self.registry.try_get_by_type(tx.ty())?;
        let result = handler.call(tx, self);
        if let Some(code) = self.error_code {
            self.state.clear_transaction_state();
            return Err(HandlerError::Fatal(code));
        };
        match result {
            Ok(result) => Ok(self.finish_executed_tx(result)),
            Err(err) => {
                self.state.clear_transaction_state();
                Err(err)
            }
        }
    }

    /// Executes a transaction for its outcome and discards its state changes.
    ///
    /// This is the cheapest convenience entrypoint for `eth_call`-style simulations: execution
    /// output and logs are returned, but transaction writes are not accepted and the transaction
    /// overlay is not detached.
    pub fn call_tx(&mut self, tx: &Recovered<T::Tx>) -> HandlerResult<TxResult<T>> {
        self.transact(tx).map(ExecutedTx::discard)
    }

    /// Dispatches the transaction to the handler registered for its EIP-2718 type byte on an async
    /// fiber.
    ///
    /// This must be used with an async database adapter such as
    /// [`evm::async::AsyncDb`](crate::evm::async::AsyncDb) to take
    /// advantage of yielding database I/O. With a synchronous database this is mostly equivalent to
    /// running the synchronous transaction on a fiber.
    ///
    /// This returns a local future and does not require the erased database, precompile provider,
    /// or optional inspector to be `Send`. Use [`Self::transact_async_send`] when the returned
    /// future must be `Send`.
    #[cfg(feature = "async")]
    pub fn transact_async<'fut>(
        &'fut mut self,
        tx: &'fut Recovered<T::Tx>,
    ) -> impl Future<Output = r#async::AsyncResult<ExecutedTx<'fut, 'a, T>, registry::HandlerError>> + 'fut
    where
        T::Tx: Sync,
    {
        let stack = self.async_stack();
        // SAFETY: The returned future owns the exclusive `&mut self` borrow, so nothing else can
        // access the EVM stack slot until that future is dropped.
        unsafe { r#async::on_local_fiber_result_with_stack(stack, move || self.transact(tx)) }
    }

    /// Dispatches the transaction to the handler registered for its EIP-2718 type byte on an async
    /// fiber and returns a `Send` future.
    ///
    /// Before calling it, the current erased database, precompile provider, and optional inspector
    /// must be verified with [`Self::evm_is_send`] or [`Self::evm_is_send_with_inspector`].
    #[cfg(feature = "async")]
    pub fn transact_async_send<'fut>(
        &'fut mut self,
        tx: &'fut Recovered<T::Tx>,
    ) -> impl Future<Output = r#async::AsyncResult<ExecutedTx<'fut, 'a, T>, registry::HandlerError>>
    + Send
    + 'fut
    where
        T::Tx: Sync,
    {
        self.assert_erased_send();
        let stack = self.async_stack();
        let evm = SendEvmRef { evm: self };
        // SAFETY: The returned future owns the exclusive `&mut self` borrow, so nothing else can
        // access the EVM stack slot until that future is dropped. The send marker checked above
        // requires all erased EVM fields to have been verified by `Evm::evm_is_send`.
        unsafe {
            r#async::on_fiber_result_with_stack(stack, move || {
                let SendEvmRef { evm } = evm;
                evm.transact(tx)
            })
        }
    }

    /// Dispatches each transaction to its registered EIP-2718 handler and commits it.
    ///
    /// Use [`Self::transact`] directly when the caller wants to choose between commit, discard,
    /// detach, and accumulator/sink commits for each transaction.
    pub fn transact_iter<'txs, I>(
        &'txs mut self,
        txs: I,
    ) -> impl Iterator<Item = HandlerResult<TxResult<T>>> + 'txs
    where
        I: IntoIterator<Item = &'txs Recovered<T::Tx>>,
        I::IntoIter: 'txs,
        T::Tx: 'txs,
        Self: 'txs,
    {
        txs.into_iter().map(move |tx| self.transact(tx).map(ExecutedTx::commit))
    }
}

impl<'a, T: EvmTypes> Evm<'a, T> {
    #[inline]
    fn execute_message_impl(
        &mut self,
        tx_env: &TxEnv<T>,
        message: &mut Message<T>,
    ) -> MessageResult<T> {
        let mut result = match message.kind {
            MessageKind::Create | MessageKind::Create2 => {
                self.execute_create_message(tx_env, message)
            }
            MessageKind::Call
            | MessageKind::CallCode
            | MessageKind::DelegateCall
            | MessageKind::StaticCall => self.execute_call_message(tx_env, message),
        };
        // Settle the returning frame's gas for its stop reason at this single exit,
        // rather than in each result builder, so every consumer (parent
        // `merge_child_gas`, top-level accounting, inspectors) reads the settled gas.
        result.gas.settle_gas(result.stop);
        result
    }

    /// Fires the inspector call/create hooks around message execution.
    ///
    /// This is invoked for every message when an inspector is installed; hook overrides skip
    /// execution entirely, including the call depth check.
    #[inline(never)]
    fn execute_message_inspected<'frame>(
        &mut self,
        tx_env: &'frame TxEnv<T>,
        message: &'frame mut Message<T>,
    ) -> MessageResult<T> {
        let guard = self.enter_execution();
        let Some(inspector) = guard.evm.inspector.as_deref_mut() else {
            return guard.evm.execute_message_impl(tx_env, message);
        };
        // SAFETY: The inspector is stored in `self`; the execution guard prevents inspector
        // replacement while the hooks are running.
        let inspector = unsafe { trustme::decouple_lt_mut(inspector) };

        // `destination` already holds the create's contract address (set when the message was
        // constructed), so the create hook observes it directly.
        let is_create = matches!(message.kind, MessageKind::Create | MessageKind::Create2);

        let mut top_frame: Option<Box<Interpreter<'frame, 'a, T>>> = None;
        let frame = match guard.evm.current_frame {
            // SAFETY: The parent frame is suspended on this call stack for the duration of the
            // message execution.
            Some(mut frame) => unsafe {
                core::mem::transmute::<
                    &mut Interpreter<'static, 'static, T>,
                    &mut Interpreter<'frame, 'a, T>,
                >(frame.as_mut())
            },
            None => {
                // SAFETY: The message outlives the frame, which is returned to the pool below.
                let frame_message = unsafe { trustme::decouple_lt(&*message) };
                let frame = top_frame.insert(guard.evm.interpreter_pool.pop(tx_env, frame_message));
                // SAFETY: `execution_config` points to a private field that host execution does
                // not replace or mutate, so the pointee remains valid for the lifetime of the
                // frame.
                let version = unsafe { trustme::decouple_lt(guard.evm.execution_config.version()) };
                frame.prepare_run(guard.evm.spec_id(), version, guard.evm);
                frame
            }
        };
        // SAFETY: The frame outlives the hook invocations below.
        let frame = unsafe { trustme::decouple_lt_mut(frame) };

        let inspected = if is_create {
            inspector.create(frame, message)
        } else {
            inspector.call(frame, message)
        };

        let mut result =
            inspected.unwrap_or_else(|| guard.evm.execute_message_impl(tx_env, message));

        if is_create {
            inspector.create_end(frame, message, &mut result);
        } else {
            inspector.call_end(frame, message, &mut result);
        }

        if let Some(frame) = top_frame {
            let _ = guard.evm.interpreter_pool.push(frame);
        }

        result
    }

    #[inline(never)]
    fn execute_create_message(
        &mut self,
        tx_env: &TxEnv<T>,
        message: &mut Message<T>,
    ) -> MessageResult<T> {
        if message.depth > CALL_DEPTH_LIMIT {
            return Self::error_message_result(
                InstrStop::CallTooDeep,
                message.gas_limit,
                message.reservoir,
            );
        }
        if let Err(stop) = self.prepare_create_message(message) {
            return Self::error_message_result(stop, message.gas_limit, message.reservoir);
        }
        let checkpoint = self.state.checkpoint();
        if let Err(stop) = self.create_message_account(message) {
            self.state.rollback(checkpoint, self.features);
            return Self::error_message_result(stop, message.gas_limit, message.reservoir);
        }
        message.code_address = message.destination;
        message.disable_precompiles = false;
        let input = core::mem::take(&mut message.input);

        let stop = self.run_interpreter(tx_env, message);
        message.input = input;

        self.finish_create_message_run(checkpoint, &message.destination, message.gas_limit, stop)
    }

    #[inline(never)]
    fn prepare_create_message(&mut self, message: &mut Message<T>) -> Result<(), InstrStop> {
        let info = if message.value > 0 || message.depth > 0 {
            self.state
                .account_info_untracked(&message.caller)
                .map_err(|code| self.store_error(code))?
        } else {
            None
        };

        if message.value > 0 && info.as_ref().is_none_or(|info| info.balance < message.value) {
            return Err(InstrStop::OutOfFunds);
        }

        // EIP-2681 caps account nonces at u64::MAX; CREATE/CREATE2 return zero instead of
        // wrapping or saturating the creator nonce.
        if message.depth > 0 && info.as_ref().is_some_and(|info| info.nonce == u64::MAX) {
            return Err(InstrStop::Return);
        }

        // `destination` already holds the contract address (derived when the message was
        // constructed); warm it before running the initcode.
        let _ = self.state.account(&message.destination, false).map(|mut a| a.warm());

        if message.depth > 0
            && let Err(code) =
                self.state.account(&message.caller, false).map(|mut a| a.bump_nonce())
        {
            return Err(self.store_error(code));
        }

        Ok(())
    }

    #[inline(never)]
    fn create_message_account(&mut self, message: &Message<T>) -> Result<(), InstrStop> {
        self.state
            .create_account(&message.caller, &message.destination, &message.value, self.features)
            .map_err(|code| self.store_error(code))??;

        self.log_eip7708_transfer(&message.caller, &message.destination, &message.value);
        Ok(())
    }

    #[inline(never)]
    fn finish_create_message_run(
        &mut self,
        checkpoint: StateCheckpoint,
        address: &Address,
        gas_limit: u64,
        stop: InstrStop,
    ) -> MessageResult<T> {
        let interp = self.interpreter_pool.last_mut().unwrap();
        let mut gas = interp.gas();
        let output = if stop.is_success() {
            let mut output = Bytes::copy_from_slice(interp.output());
            if let Err(stop) = self.validate_create_output(&mut gas, &mut output) {
                self.state.rollback(checkpoint, self.features);
                return MessageResultExt {
                    stop,
                    gas: *gas.tracker(),
                    output: Bytes::new(),
                    created_address: None,
                    ext: T::MessageResultExt::default(),
                    _non_exhaustive: (),
                };
            }

            if let Err(code) = self
                .state
                .account(address, false)
                .map(|mut a| a.set_code_slow(Bytecode::new_legacy(output.clone())))
            {
                self.state.rollback(checkpoint, self.features);
                let stop = self.store_error(code);
                return Self::error_message_result(stop, gas_limit, gas.reservoir());
            }

            output
        } else {
            self.state.rollback(checkpoint, self.features);
            Bytes::copy_from_slice(interp.output())
        };

        MessageResultExt {
            stop,
            gas: *gas.tracker(),
            output,
            created_address: stop.is_success().then_some(*address),
            ext: T::MessageResultExt::default(),
            _non_exhaustive: (),
        }
    }

    fn validate_create_output(&self, gas: &mut Gas, output: &mut Bytes) -> Result<(), InstrStop> {
        if self.feature(EvmFeatures::CODE_SIZE_CHECK) && output.len() > self.version().max_code_size
        {
            return Err(InstrStop::CreateContractSizeLimit);
        }
        if self.feature(EvmFeatures::EIP3541) && output.first().is_some_and(|byte| *byte == 0xef) {
            return Err(InstrStop::CreateContractStartingWithEF);
        }

        let code_deposit_gas = output
            .len()
            .saturating_mul(self.version().gas_params.get(GasId::CodeDepositCost) as usize);
        let code_deposit_gas = u64::try_from(code_deposit_gas).unwrap_or(u64::MAX);
        if gas.remaining() < code_deposit_gas {
            if self.feature(EvmFeatures::EIP2) {
                // EIP-2 makes code-deposit OOG fail contract creation; Frontier instead
                // creates the account with empty code.
                return Err(InstrStop::OutOfGas);
            }
            *output = Bytes::new();
            return Ok(());
        }
        gas.spend(code_deposit_gas)?;

        // EIP-8037: hashing the deployed bytecode to compute its code_hash costs
        // execution keccak word gas, and depositing the code costs state gas. The
        // state-gas charge must be the last spend before the journal commit so
        // that any 0→x→0 reservoir refills earlier in the frame are not disturbed.
        if self.feature(EvmFeatures::EIP8037) {
            let params = &self.version().gas_params;
            gas.spend(params.keccak256_word_cost(output.len()))?;
            let code_deposit_state_gas = params.code_deposit_state_gas(output.len());
            if code_deposit_state_gas > 0 {
                gas.spend_state(code_deposit_state_gas)?;
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn execute_call_message(
        &mut self,
        tx_env: &TxEnv<T>,
        message: &mut Message<T>,
    ) -> MessageResult<T> {
        if message.depth > CALL_DEPTH_LIMIT {
            return Self::error_message_result(
                InstrStop::CallTooDeep,
                message.gas_limit,
                message.reservoir,
            );
        }
        let checkpoint = self.state.checkpoint();
        // EIP-161 state clearing depends on zero-value direct call targets being touched.
        let transfers_balance = matches!(
            message.kind,
            MessageKind::Call | MessageKind::CallCode | MessageKind::StaticCall
        );
        let transfer_succeeded = !transfers_balance
            || match self.state.transfer(&message.caller, &message.destination, &message.value) {
                Ok(result) => result,
                Err(code) => {
                    let stop = self.store_error(code);
                    return Self::error_message_result(stop, message.gas_limit, message.reservoir);
                }
            };
        if transfers_balance && !transfer_succeeded {
            return Self::error_message_result(
                InstrStop::OutOfFunds,
                message.gas_limit,
                message.reservoir,
            );
        }
        if transfers_balance {
            self.log_eip7708_transfer(&message.caller, &message.destination, &message.value);
        }

        if self.contains_precompile(message) {
            return self.execute_call_precompile(checkpoint, message);
        }

        let stop = self.run_interpreter(tx_env, message);

        self.finish_call_message_run(checkpoint, stop)
    }

    #[inline(never)]
    fn execute_call_precompile(
        &mut self,
        checkpoint: StateCheckpoint,
        message: &Message<T>,
    ) -> MessageResult<T> {
        let mut gas =
            GasTracker::new_with_execution_gas_and_reservoir(message.gas_limit, message.reservoir);
        let logs_len = self.state.logs().len();
        let execution = self.execute_precompile(message, &mut gas);
        let logs = self.state.logs()[logs_len..].to_vec();
        for log in &logs {
            self.inspect_log(log);
        }
        let (stop, output) = match execution {
            Ok(output) => (InstrStop::Return, output.into_bytes()),
            Err(PrecompileError::Revert(output)) => (InstrStop::Revert, output),
            Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)) => {
                (InstrStop::PrecompileOOG, Bytes::new())
            }
            Err(PrecompileError::Halt(_)) => (InstrStop::PrecompileError, Bytes::new()),
            Err(PrecompileError::Fatal(error)) => {
                self.error = Some(error);
                self.set_error_code(ErrorCode::FATAL_PRECOMPILE);
                (InstrStop::FatalPrecompileError, Bytes::new())
            }
        };
        if !stop.is_success() {
            self.state.rollback(checkpoint, self.features);
        }
        MessageResultExt {
            stop,
            gas,
            output,
            created_address: None,
            ext: T::MessageResultExt::default(),
            _non_exhaustive: (),
        }
    }

    #[inline(never)]
    fn finish_call_message_run(
        &mut self,
        checkpoint: StateCheckpoint,
        stop: InstrStop,
    ) -> MessageResult<T> {
        let interp = self.interpreter_pool.last_mut().unwrap();
        let child_gas = interp.gas();
        let output = Bytes::copy_from_slice(interp.output());
        if !stop.is_success() {
            self.state.rollback(checkpoint, self.features);
        }

        MessageResultExt {
            stop,
            gas: *child_gas.tracker(),
            output,
            created_address: None,
            ext: T::MessageResultExt::default(),
            _non_exhaustive: (),
        }
    }

    #[inline]
    fn error_message_result(
        stop: InstrStop,
        gas_remaining: u64,
        reservoir: u64,
    ) -> MessageResult<T> {
        MessageResultExt {
            stop,
            gas: GasTracker::new_with_execution_gas_and_reservoir(gas_remaining, reservoir),
            ..MessageResultExt::default()
        }
    }

    #[inline(never)]
    fn run_interpreter<'frame>(
        &mut self,
        tx_env: &'frame TxEnv<T>,
        message: &'frame Message<T>,
    ) -> InstrStop {
        let guard = self.enter_execution();
        let mut interp: Box<Interpreter<'frame, 'a, T>> =
            guard.evm.interpreter_pool.pop(tx_env, message);
        let interp_ref = interp.as_mut();
        // SAFETY: `execution_config` points to a private field that host execution does not
        // replace or mutate, so the pointee remains valid here.
        let execution_config = unsafe { trustme::decouple_lt(&guard.evm.execution_config) };
        guard.evm.inspect_initialize_interp(interp_ref);
        let inspector = guard.evm.inspector.as_deref_mut().map(|inspector| {
            // SAFETY: The inspector is stored in `self` and remains alive for the duration of the
            // interpreter run.
            unsafe { trustme::decouple_lt_mut(inspector) }
        });
        let prev_frame = guard
            .evm
            .current_frame
            .replace(NonNull::from(&mut *interp_ref).cast::<Interpreter<'static, 'static, T>>());
        let interpreter_runner = guard.evm.interpreter_runner.clone();
        let stop = if let Some(inspector) = inspector {
            interp_ref.run_inspect(execution_config, guard.evm, inspector)
        } else if let Some(runner) = interpreter_runner
            && let Some(stop) = runner.run(execution_config, interp_ref, guard.evm)
        {
            stop
        } else {
            interp_ref.run(execution_config, guard.evm)
        };
        guard.evm.current_frame = prev_frame;
        guard.evm.interpreter_pool.push(interp);
        stop
    }

    fn inspect_initialize_interp(&mut self, interp: &mut Interpreter<'_, 'a, T>) {
        if let Some(inspector) = self.inspector.as_deref_mut() {
            // SAFETY: The inspector is stored in `self` and remains alive for the duration of the
            // hook.
            let inspector = unsafe { trustme::decouple_lt_mut(inspector) };
            // The host and spec are normally wired up by the interpreter run; set them up early so
            // that the hook can access them.
            // SAFETY: `execution_config` points to a private field that host execution does not
            // replace or mutate, so the pointee remains valid here.
            let version = unsafe { trustme::decouple_lt(self.execution_config.version()) };
            interp.prepare_run(self.spec_id(), version, self);
            inspector.initialize_interp(interp);
        }
    }
}

impl<'a, T: EvmTypes> Host<T> for Evm<'a, T> {
    fn spec_id(&self) -> SpecId {
        self.spec_id()
    }

    fn block_env(&mut self) -> &BlockEnv<T> {
        &self.block
    }

    fn load_account(
        &mut self,
        address: &Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountLoad, InstrStop> {
        let mut account = match self.state.account(address, skip_cold_load) {
            Ok(account) => account,
            Err(ErrorCode::COLD_LOAD_SKIPPED) => return Err(InstrStop::OutOfGas),
            Err(code) => return Err(store_error!(self, code)),
        };
        let is_cold = account.warm();

        let exists = account.exists();
        let info = account.get().cloned().unwrap_or_default();

        // load code
        let code = if load_code {
            account.load_code().map_err(|code| store_error!(self, code))?
        } else {
            Bytecode::default()
        };
        Ok(AccountLoad {
            balance: info.balance,
            nonce: info.nonce,
            code_hash: if exists { info.code_hash } else { B256::ZERO },
            code,
            exists,
            is_empty: info.is_empty(),
            is_cold,
            _non_exhaustive: (),
        })
    }

    fn target_is_empty_for_new_account_gas(
        &mut self,
        address: &Address,
        features: EvmFeatures,
    ) -> Result<bool, InstrStop> {
        match self.state.account(address, false) {
            Ok(account) => Ok(account.is_empty_for_new_account_gas(features)),
            Err(code) => Err(store_error!(self, code)),
        }
    }

    fn block_hash(&mut self, number: &Word) -> Result<B256, InstrStop> {
        self.state.block_hash(number).map_err(|code| self.store_error(code))
    }

    fn sload(
        &mut self,
        address: &Address,
        key: &Word,
        skip_cold_load: bool,
    ) -> Result<SLoad, InstrStop> {
        let eip2929 = self.feature(EvmFeatures::EIP2929);
        let mut slot = match self.state.storage(address).into_slot(*key, skip_cold_load) {
            Ok(slot) => slot,
            // SLOAD's out-of-gas is the cold-access charge itself, so the slot was never accessed
            // and is not recorded in the block access list (unlike SSTORE, which first pays the
            // warm-read cost, accessing the slot, before the cold/dynamic charge can run out).
            Err(ErrorCode::COLD_LOAD_SKIPPED) => return Err(InstrStop::OutOfGas),
            Err(code) => return Err(self.store_error(code)),
        };
        let is_cold = eip2929 && slot.warm();
        let value = slot.current();
        Ok(SLoad { value, is_cold, _non_exhaustive: () })
    }

    fn sstore(
        &mut self,
        address: &Address,
        key: &Word,
        value: &Word,
        skip_cold_load: bool,
    ) -> Result<SStore, InstrStop> {
        let eip2929 = self.feature(EvmFeatures::EIP2929);
        // EIP-8037: SSTORE must cover the slot's access cost before the
        // implicit storage read. When the cold access is unaffordable the read is skipped, so the
        // slot stays out of the EIP-7928 block access list (the warm-read cost has already been
        // paid by the instruction, so an affordable warm slot is still read on OOG).
        let mut slot = match self.state.storage(address).into_slot(*key, skip_cold_load) {
            Ok(slot) => slot,
            Err(ErrorCode::COLD_LOAD_SKIPPED) => return Err(InstrStop::OutOfGas),
            Err(code) => return Err(self.store_error(code)),
        };

        let is_cold = eip2929 && slot.warm();
        let (original_value, present_value) = slot.write(*value);
        Ok(SStore {
            original_value,
            present_value,
            new_value: *value,
            is_cold,
            _non_exhaustive: (),
        })
    }

    fn tload(&mut self, address: &Address, key: &Word) -> Word {
        self.state.tload(address, key)
    }

    fn tstore(&mut self, address: &Address, key: &Word, value: &Word) {
        self.state.tstore(address, key, value);
    }

    fn log(&mut self, log: Log) {
        self.emit_log(log);
    }

    #[inline]
    fn execute_message(&mut self, tx_env: &TxEnv<T>, message: &mut Message<T>) -> MessageResult<T> {
        if self.inspector.is_some() {
            return self.execute_message_inspected(tx_env, message);
        }
        self.execute_message_impl(tx_env, message)
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        skip_cold_load: bool,
    ) -> Result<SelfDestructResult, InstrStop> {
        let is_cold = if self.feature(EvmFeatures::EIP2929) {
            match self.state.account(target, skip_cold_load).map(|mut a| a.warm()) {
                Ok(is_cold) => is_cold,
                Err(ErrorCode::COLD_LOAD_SKIPPED) => return Err(InstrStop::OutOfGas),
                Err(code) => return Err(self.store_error(code)),
            }
        } else {
            if let Err(code) = self.state.account(target, false).map(|mut a| a.warm()) {
                return Err(self.store_error(code));
            }
            false
        };
        if skip_cold_load && is_cold {
            return Err(InstrStop::OutOfGas);
        }
        let target_is_empty_for_new_account_gas =
            self.target_is_empty_for_new_account_gas(target, self.features)?;
        let previously_destroyed = match self.state.account(contract, false) {
            Ok(account) => account.is_destructed(),
            Err(code) => return Err(store_error!(self, code)),
        };
        let balance = self
            .state
            .account_info_untracked(contract)
            .map_err(|code| self.store_error(code))?
            .map_or(Word::ZERO, |info| info.balance);
        let should_destroy = if self.feature(EvmFeatures::EIP6780) {
            match self.state.account(contract, false) {
                Ok(account) => account.is_created(),
                Err(code) => return Err(store_error!(self, code)),
            }
        } else {
            true
        };

        if contract != target {
            let transferred = self
                .state
                .transfer(contract, target, &balance)
                .map_err(|code| self.store_error(code))?;
            if transferred {
                self.log_eip7708_transfer(contract, target, &balance);
            }
        } else if should_destroy && !balance.is_zero() && !self.feature(EvmFeatures::EIP8246) {
            // Pre-EIP-8246: SELFDESTRUCT to self burns the contract's balance. EIP-8246 removes
            // this burn, leaving the balance untouched; finalization resets the account
            // to balance-only.
            let delta = Word::ZERO.wrapping_sub(balance);
            match self.state.account(contract, false) {
                Ok(mut account) => account.add_balance(delta),
                Err(code) => return Err(store_error!(self, code)),
            }
        }
        if should_destroy && let Ok(mut account) = self.state.account(contract, false) {
            account.mark_destructed();
        }
        Ok(SelfDestructResult {
            had_value: !balance.is_zero(),
            value: balance,
            target_is_empty: target_is_empty_for_new_account_gas,
            is_cold,
            previously_destroyed,
            _non_exhaustive: (),
        })
    }
}

/// Loaded account information.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AccountLoad {
    /// Account balance.
    pub balance: Word,
    /// Account nonce.
    pub nonce: u64,
    /// Account code hash.
    pub code_hash: B256,
    /// Account bytecode.
    pub code: Bytecode,
    /// Whether the account exists in state.
    pub exists: bool,
    /// Whether the account is empty.
    pub is_empty: bool,
    /// Whether the account access was cold.
    pub is_cold: bool,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

/// Result of an `SLOAD` host operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SLoad {
    /// Storage slot value.
    pub value: Word,
    /// Whether the storage slot access was cold.
    pub is_cold: bool,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

/// Result of an `SSTORE` host operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SStore {
    /// Storage value at the start of the transaction.
    pub original_value: Word,
    /// Storage value immediately before this `SSTORE`.
    pub present_value: Word,
    /// Storage value written by this `SSTORE`.
    pub new_value: Word,
    /// Whether the storage slot access was cold.
    pub is_cold: bool,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl SStore {
    /// Returns whether this `SSTORE` leaves the slot unchanged (`new == present`).
    #[inline]
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.new_value == self.present_value
    }

    /// Returns whether the slot is clean (`original == present`).
    #[inline]
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.original_value == self.present_value
    }

    /// Returns whether this `SSTORE` restores the slot to its original value (`new == original`).
    #[inline]
    #[must_use]
    pub fn resets_original(&self) -> bool {
        self.original_value == self.new_value
    }

    /// Returns whether the original value is zero.
    #[inline]
    #[must_use]
    pub fn original_is_zero(&self) -> bool {
        self.original_value.is_zero()
    }

    /// Returns whether the present value is zero.
    #[inline]
    #[must_use]
    pub fn present_is_zero(&self) -> bool {
        self.present_value.is_zero()
    }

    /// Returns whether the new value is zero.
    #[inline]
    #[must_use]
    pub fn new_is_zero(&self) -> bool {
        self.new_value.is_zero()
    }
}

/// Result of a `SELFDESTRUCT` host operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SelfDestructResult {
    /// Whether the destroyed account had non-zero value.
    pub had_value: bool,
    /// Balance transferred or cleared by the destruction.
    pub value: Word,
    /// Whether the beneficiary is empty/non-existent for new-account gas checks.
    pub target_is_empty: bool,
    /// Whether the beneficiary access was cold.
    pub is_cold: bool,
    /// Whether this account was already destroyed in this transaction.
    pub previously_destroyed: bool,

    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

fn eip7708_transfer_log(from: &Address, to: &Address, value: &Word) -> Option<Log> {
    if value.is_zero() || from == to {
        return None;
    }
    let topics = vec![
        EIP7708_TRANSFER_TOPIC,
        B256::left_padding_from(from.as_slice()),
        B256::left_padding_from(to.as_slice()),
    ];
    Some(Log {
        address: SYSTEM_ADDRESS,
        data: LogData::new_unchecked(topics, Bytes::copy_from_slice(&value.to_be_bytes::<32>())),
    })
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, string::ToString, sync::Arc, vec, vec::Vec};
    use core::{
        error::Error,
        fmt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use alloy_consensus::{TxLegacy, transaction::Recovered};
    use alloy_eip7928::{BlockAccessList, StorageChange};
    use alloy_primitives::{Address, Bytes, KECCAK256_EMPTY, Log, LogData, TxKind, U256};

    use super::*;
    use crate::{
        BaseEvmConfigSelector, BaseEvmTypes, NoopInspector, Precompiles, SpecId, Version,
        bytecode::Bytecode,
        env::{BlockEnvExt, TxEnvExt},
        ethereum::{RecoveredTxEnvelope, TxEnvelope, ethereum_tx_registry},
        interpreter::{GasTracker, Interpreter, Message, MessageExt, MessageKind, op},
        precompiles::{Precompile, PrecompileError, PrecompileId, PrecompileMap},
        registry::{HandlerError, TxRequest},
        test_utils::{legacy_bytecode, push_address},
    };

    const TEST_TX_TYPE: u8 = 0x00;
    const TEST_PRECOMPILE: Address = Address::with_last_byte(0x42);
    const INNER_TEST_PRECOMPILE: Address = Address::with_last_byte(0x43);

    struct ExtensionTestTypes;

    impl EvmTypesHost for ExtensionTestTypes {
        type ConfigSelector = BaseEvmConfigSelector;
        type SpecId = SpecId;
        type Tx = ();
        type EvmExt = u64;
        type MessageExt = ();
        type MessageResultExt = ();
        type TxEnvExt = ();
        type TxResultExt = ();
        type BlockEnvExt = ();
        type Host<'a> = Evm<'a, Self>;
    }

    #[test]
    fn stores_typed_evm_extension_state() {
        let mut evm = Evm::<ExtensionTestTypes>::new_with_ext(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompile::NoPrecompiles::default(),
            41,
        );

        assert_eq!(*evm.ext(), 41);
        *evm.ext_mut() += 1;
        assert_eq!(*evm.ext(), 42);
    }

    fn test_tx(value: u64) -> RecoveredTxEnvelope {
        Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy { nonce: value, ..TxLegacy::default() }),
            Address::ZERO,
        )
    }

    fn handle_test_tx(req: TxRequest<'_, '_, BaseEvmTypes, TxLegacy>) -> HandlerResult<TxResult> {
        let _ = req.host.spec_id();
        Ok(TxResultExt {
            status: true,
            total_gas_spent: req.tx.nonce + 1,
            ..TxResultExt::default()
        })
    }

    #[derive(Debug)]
    struct TestInterpreterRunner {
        stop: Option<InstrStop>,
        calls: Arc<AtomicUsize>,
    }

    impl InterpreterRunner<BaseEvmTypes> for TestInterpreterRunner {
        fn run<'frame, 'host>(
            &self,
            _config: &ExecutionConfig<BaseEvmTypes>,
            _interpreter: &mut Interpreter<'frame, 'host, BaseEvmTypes>,
            _host: &mut Evm<'host, BaseEvmTypes>,
        ) -> Option<InstrStop> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.stop
        }
    }

    fn run_with_test_interpreter_runner(stop: Option<InstrStop>, bytecode: &[u8]) -> InstrStop {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_interpreter_runner(TestInterpreterRunner { stop, calls: Arc::clone(&calls) });
        let tx_env = TxEnvExt::default();
        let message = MessageExt {
            gas_limit: 30_000,
            code: Bytecode::new_legacy(Bytes::copy_from_slice(bytecode)),
            ..Default::default()
        };

        let stop = evm.run_interpreter(&tx_env, &message);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        stop
    }

    const LIFECYCLE_ACCOUNT: Address = Address::with_last_byte(0x7a);
    const LIFECYCLE_STORAGE_KEY: Word = Word::from_limbs([1, 0, 0, 0]);

    fn handle_lifecycle_tx(
        req: TxRequest<'_, '_, BaseEvmTypes, TxLegacy>,
    ) -> HandlerResult<TxResult> {
        let value = Word::from(req.tx.nonce);
        req.host
            .state
            .storage(&LIFECYCLE_ACCOUNT)
            .into_slot(LIFECYCLE_STORAGE_KEY, false)
            .map_err(registry::HandlerError::Fatal)?
            .write(value);
        req.host.state.log(Log {
            address: LIFECYCLE_ACCOUNT,
            data: LogData::new_unchecked(vec![], Bytes::new()),
        });
        Ok(TxResultExt { status: true, total_gas_spent: req.tx.nonce, ..TxResultExt::default() })
    }

    fn empty_precompiles() -> Precompiles<BaseEvmTypes> {
        Precompiles::new(Cow::Owned(PrecompileMap::new()))
    }

    fn test_precompile(
        address: Address,
        f: crate::precompiles::PrecompileFn<BaseEvmTypes>,
    ) -> Precompile<BaseEvmTypes> {
        Precompile::new(address, PrecompileId::custom("test"), f)
    }

    fn precompiles_with(
        precompiles: impl IntoIterator<Item = Precompile<BaseEvmTypes>>,
    ) -> Precompiles<BaseEvmTypes> {
        let mut map = PrecompileMap::new();
        for precompile in precompiles {
            map.insert(precompile);
        }
        Precompiles::new(Cow::Owned(map))
    }

    fn precompile_message(address: Address) -> Message {
        MessageExt {
            kind: MessageKind::Call,
            depth: 0,
            gas_limit: 30_000,
            reservoir: 0,
            destination: address,
            caller: Address::ZERO,
            input: Bytes::new(),
            value: U256::ZERO,
            code: Bytecode::default(),
            code_address: address,
            disable_precompiles: false,
            caller_is_static: false,
            salt: B256::ZERO,
            ext: (),
            _non_exhaustive: (),
        }
    }

    #[test]
    fn precompile_logs_are_inspected() {
        #[derive(Default)]
        struct LogInspector(Vec<Log>);

        impl Inspector<BaseEvmTypes> for LogInspector {
            fn log(&mut self, log: &Log, _host: &mut Evm<'_, BaseEvmTypes>) {
                self.0.push(log.clone());
            }
        }

        let precompiles = precompiles_with([test_precompile(TEST_PRECOMPILE, |evm, _, _| {
            evm.state_mut().log(Log {
                address: TEST_PRECOMPILE,
                data: LogData::new_unchecked(Vec::new(), Bytes::from_static(b"precompile")),
            });
            Ok(PrecompileOutput::new(Bytes::new()))
        })]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        evm.set_inspector(LogInspector::default());
        let mut message = precompile_message(TEST_PRECOMPILE);

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::Return);
        let inspector = evm.clear_inspector_as::<LogInspector>().unwrap();
        assert_eq!(inspector.0.len(), 1);
        assert_eq!(inspector.0.as_slice(), evm.logs());
    }

    fn lifecycle_evm() -> Evm<'static, BaseEvmTypes> {
        let registry = TxRegistry::new().with_handler(
            TEST_TX_TYPE,
            TxEnvelope::as_legacy,
            handle_lifecycle_tx,
        );
        let mut database = InMemoryDB::default();
        database.insert_account_info(
            &LIFECYCLE_ACCOUNT,
            AccountInfo::default().with_balance(Word::from(1)),
        );
        database.insert_account_storage(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY, &Word::from(1));
        Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            registry,
            database,
            Precompiles::base(SpecId::OSAKA),
        )
    }

    #[test]
    fn fatal_custom_precompile_aborts_parent_call() {
        const FATAL_PRECOMPILE_ADDRESS: Address = Address::with_last_byte(0x43);

        #[derive(Debug)]
        struct TestPrecompileError;

        impl fmt::Display for TestPrecompileError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("test precompile error")
            }
        }

        impl Error for TestPrecompileError {}

        let contract = Address::from([0xbb; 20]);
        let mut code = vec![op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0];
        push_address(&mut code, &FATAL_PRECOMPILE_ADDRESS);
        code.extend([
            op::PUSH2,
            0x75,
            0x30,
            op::CALL,
            op::PUSH1,
            1,
            op::PUSH1,
            1,
            op::SSTORE,
            op::STOP,
        ]);
        let bytecode = Bytecode::new_legacy(Bytes::from(code));
        let mut precompiles = Precompiles::base(SpecId::OSAKA);
        precompiles.as_map_mut().insert(Precompile::new(
            FATAL_PRECOMPILE_ADDRESS,
            PrecompileId::custom("fatal-test"),
            |_, _, _| Err(PrecompileError::fatal(TestPrecompileError)),
        ));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 200_000,
            code: bytecode,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::FatalPrecompileError);
        assert_eq!(evm.error_code(), Some(ErrorCode::FATAL_PRECOMPILE));
        evm.state.finalize_transaction_(Version::base(SpecId::OSAKA));
        let pending = evm.state.take_pending_state();
        assert!(
            !pending
                .storage
                .get(&contract)
                .is_some_and(|overlay| overlay.slots.contains_key(&Word::from(1)))
        );
    }

    #[test]
    fn precompile_oog_halt_remains_recoverable_by_parent_call() {
        const OOG_PRECOMPILE_ADDRESS: Address = Address::with_last_byte(0x44);

        let contract = Address::from([0xbc; 20]);
        let mut code = vec![op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0, op::PUSH1, 0];
        push_address(&mut code, &OOG_PRECOMPILE_ADDRESS);
        code.extend([
            op::PUSH2,
            0x75,
            0x30,
            op::CALL,
            op::PUSH1,
            1,
            op::PUSH1,
            1,
            op::SSTORE,
            op::STOP,
        ]);
        let bytecode = Bytecode::new_legacy(Bytes::from(code));
        let mut precompiles = Precompiles::base(SpecId::OSAKA);
        precompiles.as_map_mut().insert(Precompile::new(
            OOG_PRECOMPILE_ADDRESS,
            PrecompileId::custom("oog-test"),
            |_, _, _| Err(PrecompileHalt::OutOfGas.into()),
        ));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 200_000,
            code: bytecode,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::Stop);
        assert!(evm.error_code().is_none());
        evm.state.finalize_transaction_(Version::base(SpecId::OSAKA));
        let pending = evm.state.take_pending_state();
        assert!(
            pending
                .storage
                .get(&contract)
                .is_some_and(|overlay| overlay.slots.contains_key(&Word::from(1)))
        );
    }

    #[test]
    fn fatal_custom_precompile_tx_error_can_be_recovered_multiple_times() {
        const FATAL_PRECOMPILE_ADDRESS: Address = Address::with_last_byte(0x43);

        #[derive(Debug)]
        struct TestPrecompileError;

        impl fmt::Display for TestPrecompileError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("test precompile error")
            }
        }

        impl Error for TestPrecompileError {}

        let caller = Address::from([0xaa; 20]);
        let tx = Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy {
                gas_limit: 50_000,
                to: TxKind::Call(FATAL_PRECOMPILE_ADDRESS),
                ..TxLegacy::default()
            }),
            caller,
        );
        let mut precompiles = Precompiles::base(SpecId::OSAKA);
        precompiles.as_map_mut().insert(Precompile::new(
            FATAL_PRECOMPILE_ADDRESS,
            PrecompileId::custom("fatal-test"),
            |_, _, _| Err(PrecompileError::fatal(TestPrecompileError)),
        ));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            ethereum_tx_registry(SpecId::OSAKA),
            InMemoryDB::default(),
            precompiles,
        );

        assert_eq!(
            evm.transact(&tx).map(ExecutedTx::discard),
            Err(HandlerError::Fatal(ErrorCode::FATAL_PRECOMPILE))
        );
        let code = evm.error_code().unwrap();
        assert_eq!(code, ErrorCode::FATAL_PRECOMPILE);
        assert_eq!(evm.error(code).to_string(), "test precompile error");
        assert_eq!(evm.error(code).to_string(), "test precompile error");
    }

    #[derive(Clone, Copy)]
    enum PrecompileAccess {
        Mut,
        AsMut,
        Set,
    }

    fn run_precompile_access(access: PrecompileAccess) {
        let precompiles = precompiles_with([test_precompile(
            TEST_PRECOMPILE,
            match access {
                PrecompileAccess::Mut => |evm, _, _| {
                    let _ = evm.precompiles_mut();
                    Ok(PrecompileOutput::new(Bytes::new()))
                },
                PrecompileAccess::AsMut => |evm, _, _| {
                    evm.assert_precompiles_downcast_mutable();
                    Ok(PrecompileOutput::new(Bytes::new()))
                },
                PrecompileAccess::Set => |evm, _, _| {
                    evm.set_precompiles(empty_precompiles());
                    Ok(PrecompileOutput::new(Bytes::new()))
                },
            },
        )]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let message = precompile_message(TEST_PRECOMPILE);
        let _ = evm.execute_precompile(&message, &mut GasTracker::new(30_000));
    }

    #[test]
    fn replacing_block_preserves_state_and_bal_context() {
        let mut evm = lifecycle_evm();
        let state = core::ptr::addr_of!(evm.state);
        let account = Address::with_last_byte(0x51);
        let storage_key = Word::from(0x52);
        let block_number = Word::from(0x53);
        let block_hash = B256::with_last_byte(0x54);

        evm.overlay_db_mut()
            .insert_account_info(&account, AccountInfo::default().with_balance(Word::from(55)));
        evm.overlay_db_mut().insert_account_storage(&account, &storage_key, &Word::from(56));
        evm.overlay_db_mut().insert_block_hash(&block_number, &block_hash);

        let bal = Arc::new(Bal::new());
        evm.state.set_bal(Arc::clone(&bal));
        evm.state.enable_bal_builder();
        evm.state.set_bal_index(BlockAccessIndex::new(57));

        let block = BlockEnvExt { number: Word::from(58), ..BlockEnvExt::default() };
        evm.set_block(block);

        assert!(core::ptr::eq(state, core::ptr::addr_of!(evm.state)));
        assert_eq!(evm.block(), &block);
        assert_eq!(evm.overlay_db().account_info(&account).unwrap().balance, Word::from(55));
        assert_eq!(
            evm.overlay_db()
                .cache
                .storage
                .get(&account)
                .and_then(|storage| storage.slots.get(&storage_key)),
            Some(&Word::from(56))
        );
        assert_eq!(evm.overlay_db().cache.block_hashes.get(&block_number), Some(&block_hash));
        assert!(Arc::ptr_eq(evm.state.bal().unwrap(), &bal));
        assert!(evm.state.bal_builder().is_some());
        assert_eq!(evm.state.bal_index(), BlockAccessIndex::new(57));
    }

    #[test]
    fn replacing_execution_config_preserves_state_and_rebuilds_fork_state() {
        let mut evm = lifecycle_evm();
        let state = core::ptr::addr_of!(evm.state);
        let account = Address::with_last_byte(0x61);
        let storage_key = Word::from(0x62);
        let block_number = Word::from(0x63);
        let block_hash = B256::with_last_byte(0x64);
        evm.overlay_db_mut()
            .insert_account_info(&account, AccountInfo::default().with_balance(Word::from(65)));
        evm.overlay_db_mut().insert_account_storage(&account, &storage_key, &Word::from(66));
        evm.overlay_db_mut().insert_block_hash(&block_number, &block_hash);

        let bal = Arc::new(Bal::new());
        evm.state.set_bal(Arc::clone(&bal));
        evm.state.enable_bal_builder();
        evm.state.set_bal_index(BlockAccessIndex::new(67));

        let config = ExecutionConfig::for_base_spec::<BaseEvmConfigSelector>(SpecId::PRAGUE);
        evm.set_execution_config(
            config,
            SpecId::PRAGUE,
            crate::ethereum::ethereum_tx_registry(SpecId::PRAGUE),
            Precompiles::base(SpecId::PRAGUE),
        );

        assert!(core::ptr::eq(state, core::ptr::addr_of!(evm.state)));
        assert_eq!(evm.spec_id(), SpecId::PRAGUE);
        assert_eq!(evm.config_spec_id(), SpecId::PRAGUE);
        assert_eq!(evm.version().features, config.version().features);
        assert!(evm.feature(EvmFeatures::EIP7702));
        assert!(evm.registry().contains(4));
        assert!(
            evm.precompiles_as::<Precompiles>()
                .unwrap()
                .as_map()
                .contains(&Address::with_last_byte(0x0b))
        );
        assert_eq!(evm.overlay_db().account_info(&account).unwrap().balance, Word::from(65));
        assert_eq!(evm.overlay_db().cache.block_hashes.get(&block_number), Some(&block_hash));
        assert!(Arc::ptr_eq(evm.state.bal().unwrap(), &bal));
        assert!(evm.state.bal_builder().is_some());
        assert_eq!(evm.state.bal_index(), BlockAccessIndex::new(67));
    }

    #[test]
    fn immutable_precompile_access_is_allowed_during_execution() {
        let precompiles = precompiles_with([test_precompile(TEST_PRECOMPILE, |evm, _, _| {
            let _ = evm.precompiles();
            Ok(PrecompileOutput::new(Bytes::new()))
        })]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let message = precompile_message(TEST_PRECOMPILE);

        evm.execute_precompile(&message, &mut GasTracker::new(30_000)).unwrap();
    }

    #[test]
    #[should_panic(expected = "precompile provider cannot be modified during EVM execution")]
    fn precompiles_mut_panics_during_execution() {
        run_precompile_access(PrecompileAccess::Mut);
    }

    #[test]
    #[should_panic(expected = "precompile provider cannot be modified during EVM execution")]
    fn precompiles_as_mut_panics_during_execution() {
        run_precompile_access(PrecompileAccess::AsMut);
    }

    #[test]
    #[should_panic(expected = "precompile provider cannot be modified during EVM execution")]
    fn set_precompiles_panics_during_execution() {
        run_precompile_access(PrecompileAccess::Set);
    }

    #[derive(Clone, Copy)]
    enum InspectorAccess {
        Mut,
        Set,
        SetBoxed,
        Clear,
    }

    fn run_inspector_access(access: InspectorAccess) {
        struct AccessingInspector {
            access: InspectorAccess,
        }

        impl Inspector<BaseEvmTypes> for AccessingInspector {
            fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
                let evm = interp.host();
                match self.access {
                    InspectorAccess::Mut => {
                        let _ = evm.inspector_mut();
                    }
                    InspectorAccess::Set => evm.set_inspector(NoopInspector::default()),
                    InspectorAccess::SetBoxed => {
                        evm.set_boxed_inspector(Box::<NoopInspector>::default());
                    }
                    InspectorAccess::Clear => {
                        let _ = evm.clear_inspector();
                    }
                }
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_inspector(AccessingInspector { access });
        let message = MessageExt {
            code: Bytecode::new_legacy(Bytes::from_static(&[op::STOP])),
            ..MessageExt::default()
        };
        let tx_env = TxEnvExt::default();
        let _ = evm.run_interpreter(&tx_env, &message);
    }

    #[test]
    fn immutable_inspector_access_is_allowed_during_execution() {
        struct ReadingInspector {}

        impl Inspector<BaseEvmTypes> for ReadingInspector {
            fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
                let _ = interp.host().inspector();
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_inspector(ReadingInspector {});
        let message = MessageExt {
            code: Bytecode::new_legacy(Bytes::from_static(&[op::STOP])),
            ..MessageExt::default()
        };
        let tx_env = TxEnvExt::default();

        let _ = evm.run_interpreter(&tx_env, &message);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn inspector_mut_panics_during_execution() {
        run_inspector_access(InspectorAccess::Mut);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn set_inspector_panics_during_execution() {
        run_inspector_access(InspectorAccess::Set);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn set_boxed_inspector_panics_during_execution() {
        run_inspector_access(InspectorAccess::SetBoxed);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn clear_inspector_panics_during_execution() {
        run_inspector_access(InspectorAccess::Clear);
    }

    #[test]
    fn set_block_during_execution_updates_live_context() {
        struct BlockReplacingInspector;

        impl Inspector<BaseEvmTypes> for BlockReplacingInspector {
            fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
                let host = interp.host();
                host.set_block(BlockEnvExt {
                    number: Word::from(123),
                    timestamp: Word::from(124),
                    ..BlockEnvExt::default()
                });
                assert_eq!(host.block().number, Word::from(123));
                assert_eq!(host.block().timestamp, Word::from(124));
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_inspector(BlockReplacingInspector);
        let message = MessageExt {
            code: Bytecode::new_legacy(Bytes::from_static(&[op::STOP])),
            ..MessageExt::default()
        };
        let tx_env = TxEnvExt::default();
        let _ = evm.run_interpreter(&tx_env, &message);
        assert_eq!(evm.block().number, Word::from(123));
        assert_eq!(evm.block().timestamp, Word::from(124));
    }

    #[test]
    #[should_panic(expected = "execution config cannot be modified during EVM execution")]
    fn set_execution_config_panics_during_execution() {
        struct ConfigReplacingInspector;

        impl Inspector<BaseEvmTypes> for ConfigReplacingInspector {
            fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
                let host = interp.host();
                host.set_execution_config(
                    ExecutionConfig::for_base_spec::<BaseEvmConfigSelector>(SpecId::PRAGUE),
                    SpecId::PRAGUE,
                    crate::ethereum::ethereum_tx_registry(SpecId::PRAGUE),
                    Precompiles::base(SpecId::PRAGUE),
                );
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_inspector(ConfigReplacingInspector);
        let message = MessageExt {
            code: Bytecode::new_legacy(Bytes::from_static(&[op::STOP])),
            ..MessageExt::default()
        };
        let tx_env = TxEnvExt::default();
        let _ = evm.run_interpreter(&tx_env, &message);
    }

    #[derive(Clone, Copy)]
    enum TopLevelInspectorHook {
        Log,
        Call,
        Create,
    }

    fn run_top_level_inspector_clear(hook: TopLevelInspectorHook) {
        struct ClearingInspector;

        impl Inspector<BaseEvmTypes> for ClearingInspector {
            fn log(&mut self, _log: &Log, host: &mut Evm<'_, BaseEvmTypes>) {
                let _ = host.clear_inspector();
            }

            fn call(
                &mut self,
                interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
                _message: &mut Message,
            ) -> Option<MessageResult> {
                let _ = interp.host().clear_inspector();
                None
            }

            fn create(
                &mut self,
                interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
                _message: &mut Message,
            ) -> Option<MessageResult> {
                let _ = interp.host().clear_inspector();
                None
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        evm.set_inspector(ClearingInspector);

        match hook {
            TopLevelInspectorHook::Log => Host::log(
                &mut evm,
                Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::new()) },
            ),
            TopLevelInspectorHook::Call => {
                let mut message = MessageExt { kind: MessageKind::Call, ..Default::default() };
                let _ = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
            }
            TopLevelInspectorHook::Create => {
                let mut message = MessageExt { kind: MessageKind::Create, ..Default::default() };
                let _ = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
            }
        }
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn top_level_log_inspector_clear_panics_during_execution() {
        run_top_level_inspector_clear(TopLevelInspectorHook::Log);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn top_level_call_inspector_clear_panics_during_execution() {
        run_top_level_inspector_clear(TopLevelInspectorHook::Call);
    }

    #[test]
    #[should_panic(expected = "inspector cannot be modified during EVM execution")]
    fn top_level_create_inspector_clear_panics_during_execution() {
        run_top_level_inspector_clear(TopLevelInspectorHook::Create);
    }

    #[test]
    fn passes_evm_to_precompile_provider() {
        let address = TEST_PRECOMPILE;
        let block = BlockEnvExt { number: U256::from(17), ..BlockEnvExt::default() };
        let precompiles = precompiles_with([test_precompile(address, |evm, message, _| {
            assert_eq!(message.kind, MessageKind::Call);
            assert_eq!(message.depth, 67);
            assert_eq!(message.destination, TEST_PRECOMPILE);
            assert_eq!(message.caller, Address::with_last_byte(0x7a));
            assert_eq!(message.input, Bytes::from_static(b"message input"));
            assert_eq!(message.value, U256::from(99));
            assert_eq!(message.code_address, TEST_PRECOMPILE);
            assert!(!message.disable_precompiles);
            Ok(PrecompileOutput::new(Bytes::copy_from_slice(&evm.block.number.to_be_bytes::<32>())))
        })]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            block,
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let message = MessageExt {
            kind: MessageKind::Call,
            depth: 67,
            gas_limit: 30_000,
            reservoir: 0,
            destination: address,
            caller: Address::with_last_byte(0x7a),
            input: Bytes::from_static(b"message input"),
            value: U256::from(99),
            code: Bytecode::default(),
            code_address: address,
            disable_precompiles: false,
            caller_is_static: false,
            salt: B256::ZERO,
            ext: (),
            _non_exhaustive: (),
        };
        let output = evm
            .execute_precompile(&message, &mut GasTracker::new(30_000))
            .expect("precompile succeeds");

        assert_eq!(U256::from_be_slice(output.bytes()), U256::from(17));
    }

    #[test]
    fn precompile_can_call_another_precompile() {
        let precompiles = precompiles_with([
            test_precompile(TEST_PRECOMPILE, |evm, _, gas| {
                let message = precompile_message(INNER_TEST_PRECOMPILE);
                evm.execute_precompile(&message, gas)
            }),
            test_precompile(INNER_TEST_PRECOMPILE, |_, _, _| {
                Ok(PrecompileOutput::new(Bytes::from_static(b"inner")))
            }),
        ]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );
        let message = precompile_message(TEST_PRECOMPILE);

        let output = evm
            .execute_precompile(&message, &mut GasTracker::new(30_000))
            .expect("precompile succeeds");

        assert_eq!(output.bytes(), b"inner");
    }

    #[test]
    fn dispatches_transaction_by_typed_2718_type() {
        let registry =
            TxRegistry::new().with_handler(TEST_TX_TYPE, TxEnvelope::as_legacy, handle_test_tx);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            registry,
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let tx = test_tx(41);

        assert_eq!(evm.transact(&tx).map(|executed| executed.discard().tx_gas_used()), Ok(42));
    }

    #[test]
    fn dispatches_transaction_without_evm_config() {
        let registry =
            TxRegistry::new().with_handler(TEST_TX_TYPE, TxEnvelope::as_legacy, handle_test_tx);
        let mut evm = Evm::<BaseEvmTypes>::new_with_execution_config(
            ExecutionConfig::for_base_spec::<BaseEvmConfigSelector>(SpecId::OSAKA),
            SpecId::OSAKA,
            BlockEnvExt::default(),
            registry,
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let tx = test_tx(41);

        assert_eq!(evm.transact(&tx).map(|executed| executed.discard().tx_gas_used()), Ok(42));
    }

    #[test]
    fn interpreter_runner_can_execute_frame() {
        assert_eq!(
            run_with_test_interpreter_runner(Some(InstrStop::Return), &[op::INVALID]),
            InstrStop::Return
        );
    }

    #[test]
    fn interpreter_runner_can_fallback_to_interpreter() {
        assert_eq!(run_with_test_interpreter_runner(None, &[op::STOP]), InstrStop::Stop);
    }

    #[test]
    fn dispatches_transaction_with_dynamic_version() {
        fn handle_test_tx_version(
            req: TxRequest<'_, '_, BaseEvmTypes, TxLegacy>,
        ) -> HandlerResult<TxResult> {
            Ok(TxResultExt {
                status: true,
                total_gas_spent: req.host.version().tx_gas_limit_cap,
                ..TxResultExt::default()
            })
        }

        let registry = TxRegistry::new().with_handler(
            TEST_TX_TYPE,
            TxEnvelope::as_legacy,
            handle_test_tx_version,
        );
        let mut version = crate::Version::new(SpecId::OSAKA);
        version.tx_gas_limit_cap = 42;
        let mut evm = Evm::<BaseEvmTypes>::new_with_execution_config(
            ExecutionConfig::for_spec_and_version(SpecId::OSAKA, version),
            SpecId::OSAKA,
            BlockEnvExt::default(),
            registry,
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let tx = test_tx(0);

        assert_eq!(evm.transact(&tx).map(|executed| executed.discard().tx_gas_used()), Ok(42));
    }

    #[test]
    fn dispatches_transaction_iter() {
        let registry =
            TxRegistry::new().with_handler(TEST_TX_TYPE, TxEnvelope::as_legacy, handle_test_tx);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            registry,
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let txs = [test_tx(1), test_tx(2)];
        let gas_used = evm
            .transact_iter(&txs)
            .map(|result| result.map(|result| result.tx_gas_used()))
            .collect::<HandlerResult<Vec<_>>>();

        assert_eq!(gas_used, Ok(vec![2, 3]));
    }

    #[test]
    fn executed_transaction_discard_drops_state_but_keeps_outcome_logs() {
        let mut evm = lifecycle_evm();
        let outcome =
            evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").discard();

        assert_eq!(outcome.tx_gas_used(), 7);
        assert_eq!(outcome.logs.len(), 1);
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(1)
        );
    }

    #[test]
    fn executed_transaction_discard_with_streams_without_committing() {
        let mut evm = lifecycle_evm();
        let mut sink = BlockStateAccumulator::new();

        let outcome = evm
            .transact(&test_tx(7))
            .expect("lifecycle transaction should execute")
            .discard_with(&mut sink)
            .expect("block accumulator is infallible");

        assert_eq!(outcome.tx_gas_used(), 7);
        assert_eq!(outcome.logs.len(), 1);
        let storage = sink.storage_sorted();
        assert_eq!(storage.len(), 1);
        assert_eq!(storage[0].1.original, Word::from(1));
        assert_eq!(storage[0].1.current, Word::from(7));
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(1)
        );
    }

    #[test]
    fn executed_transaction_detach_moves_pending_state_without_committing() {
        let mut evm = lifecycle_evm();
        let result =
            evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").detach();

        assert_eq!(result.result.logs.len(), 1);
        let overlay = result
            .pending_state
            .storage
            .get(&LIFECYCLE_ACCOUNT)
            .expect("storage change should be present");
        let slot =
            overlay.slots.get(&LIFECYCLE_STORAGE_KEY).expect("storage slot should be present");
        assert_eq!(slot.value.original, Word::from(1));
        assert_eq!(slot.value.current, Word::from(7));
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(1)
        );
    }

    #[test]
    fn executed_transaction_detach_matches_commit_fold() {
        // Commit with the builder enabled: the reference BAL fold.
        let mut evm = lifecycle_evm();
        evm.state.enable_bal_builder();
        evm.state.set_bal_index(BlockAccessIndex::new(1));
        let _ = evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").commit();
        let committed_bal = evm.state.take_bal_builder().expect("builder was enabled");

        // The same execution detached as a pending state.
        let mut evm = lifecycle_evm();
        let TxResultWithState { pending_state: pending, .. } =
            evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").detach();

        // Folding the pending state into a fresh BAL matches the commit-built BAL.
        let mut rebuilt = Bal::new();
        rebuilt.commit(BlockAccessIndex::new(1), pending);
        assert_eq!(rebuilt, committed_bal);
        // Detaching accepted nothing into this EVM's overlay.
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(1)
        );
    }

    #[test]
    fn executed_transaction_commit_updates_accepted_overlay() {
        let mut evm = lifecycle_evm();
        let outcome =
            evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").commit();

        assert_eq!(outcome.logs.len(), 1);
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(7)
        );

        let _ = evm.transact(&test_tx(9)).expect("lifecycle transaction should execute").commit();
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(9)
        );
    }

    #[test]
    fn executed_transaction_commit_to_accumulates_block_state() {
        let mut evm = lifecycle_evm();
        let mut block_state = BlockStateAccumulator::new();

        let _ = evm
            .transact(&test_tx(7))
            .expect("lifecycle transaction should execute")
            .commit_to(&mut block_state);
        let _ = evm
            .transact(&test_tx(9))
            .expect("lifecycle transaction should execute")
            .commit_to(&mut block_state);

        let storage = block_state.storage_sorted();
        assert_eq!(storage.len(), 1);
        assert_eq!(storage[0].1.original, Word::from(1));
        assert_eq!(storage[0].1.current, Word::from(9));
        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(9)
        );
    }

    #[test]
    fn committed_transactions_build_block_access_list_on_cache_db() {
        let mut evm = lifecycle_evm();
        evm.state.enable_bal_builder();
        evm.state.reset_bal_index();

        // Transaction 0 maps to block access index 1: it writes 7 to the lifecycle slot (was 1).
        evm.state.bump_bal_index();
        assert_eq!(evm.state.bal_index(), BlockAccessIndex::new(1));
        let _ = evm.transact(&test_tx(7)).expect("lifecycle transaction should execute").commit();

        // Transaction 1 maps to index 2: it overwrites the slot with 9.
        evm.state.bump_bal_index();
        let _ = evm.transact(&test_tx(9)).expect("lifecycle transaction should execute").commit();

        let bal = evm.state.bal_builder().expect("bal construction is enabled");
        let account =
            bal.accounts.get(&LIFECYCLE_ACCOUNT).expect("lifecycle account is in the bal");
        let slot = account
            .storage
            .storage
            .get(&LIFECYCLE_STORAGE_KEY)
            .expect("lifecycle slot is in the bal");
        assert_eq!(
            slot.changes,
            vec![
                StorageChange::new(BlockAccessIndex::new(1), Word::from(7)),
                StorageChange::new(BlockAccessIndex::new(2), Word::from(9)),
            ]
        );
        // The account's info never changed, so it is recorded as reads (no info writes).
        assert!(account.account_info.balance.is_empty());
        assert!(account.account_info.nonce.is_empty());

        // Taking the BAL yields a canonical EIP-7928 list and resets the index.
        let alloy = BlockAccessList::from(evm.state.take_bal_builder().expect("bal is present"));
        assert_eq!(alloy.len(), 1);
        assert_eq!(alloy[0].address, LIFECYCLE_ACCOUNT);
        assert_eq!(alloy[0].storage_changes.len(), 1);
        assert_eq!(alloy[0].storage_changes[0].slot, LIFECYCLE_STORAGE_KEY);
        assert_eq!(alloy[0].storage_changes[0].changes.len(), 2);
        assert_eq!(evm.state.bal_index(), BlockAccessIndex::PRE_EXECUTION);
        assert!(evm.state.bal_builder().is_none());
    }

    #[test]
    fn executed_transaction_commit_with_tee_fans_out_changes() {
        let mut evm = lifecycle_evm();
        let mut left = BlockStateAccumulator::new();
        let mut right = BlockStateAccumulator::new();
        let mut tee = Tee::new(&mut left, &mut right);

        let _ = evm
            .transact(&test_tx(7))
            .expect("lifecycle transaction should execute")
            .commit_with(&mut tee)
            .expect("block accumulators are infallible");

        assert_eq!(left.storage_sorted()[0].1.current, Word::from(7));
        assert_eq!(right.storage_sorted()[0].1.current, Word::from(7));
    }

    #[test]
    fn dropped_executed_transaction_discards_state() {
        let mut evm = lifecycle_evm();
        drop(evm.transact(&test_tx(7)).expect("lifecycle transaction should execute"));

        assert_eq!(
            evm.state.storage_slot_untracked(&LIFECYCLE_ACCOUNT, &LIFECYCLE_STORAGE_KEY).unwrap(),
            Word::from(1)
        );
    }

    #[test]
    fn host_executes_message() {
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let contract = Address::from([0x11; 20]);
        let bytecode = Bytecode::new_legacy(Bytes::from_static(&[op::ADDRESS, op::STOP]));
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 50_000,
            code: bytecode,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert!(result.stop.is_success());
    }

    #[test]
    fn host_records_error_code() {
        #[derive(Debug)]
        struct FailingDbError;

        impl fmt::Display for FailingDbError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("storage read failed")
            }
        }

        impl Error for FailingDbError {}

        #[derive(Debug, Default)]
        struct FailingStorageDb;

        impl Database for FailingStorageDb {
            type Error = FailingDbError;

            fn get_account(
                &mut self,
                _address: &Address,
            ) -> Result<Option<AccountInfo>, Self::Error> {
                Ok(Some(AccountInfo::default()))
            }

            fn get_code_by_hash(&mut self, _code_hash: &B256) -> Result<Bytecode, Self::Error> {
                Ok(Bytecode::default())
            }

            fn get_storage(
                &mut self,
                _address: &Address,
                _key: &Word,
            ) -> Result<Word, Self::Error> {
                Err(FailingDbError)
            }

            fn get_block_hash(&mut self, _number: &Word) -> Result<B256, Self::Error> {
                Ok(B256::ZERO)
            }
        }

        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            Db::new(FailingStorageDb),
            Precompiles::base(SpecId::OSAKA),
        );
        let contract = Address::from([0x11; 20]);
        let bytecode = Bytecode::new_legacy(Bytes::from_static(&[op::PUSH0, op::SLOAD]));
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 50_000,
            code: bytecode,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::FatalExternalError);
        let error_code = evm.error_code().unwrap();
        assert_eq!(evm.database_mut().error(error_code).to_string(), "storage read failed");
        assert_eq!(evm.database_mut().error(error_code).to_string(), "storage read failed");
    }

    #[test]
    fn cold_storage_oog_rolls_back_warmth() {
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );
        let contract = Address::from([0x11; 20]);
        let key = Word::ZERO;
        let bytecode =
            Bytecode::new_legacy(Bytes::from_static(&[op::PUSH1, 0, op::SLOAD, op::STOP]));
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 500,
            code: bytecode,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::OutOfGas);
        assert!(!evm.state.storage(&contract).is_warm(&key));
    }

    #[test]
    fn unaffordable_cold_target_selfdestruct_does_not_load_target_account() {
        #[derive(Clone, Debug)]
        struct SelfdestructTargetLoadDb {
            target: Address,
            target_reads: Arc<AtomicUsize>,
        }

        #[derive(Clone, Debug, PartialEq, Eq)]
        struct SelfdestructTargetLoadError;

        impl fmt::Display for SelfdestructTargetLoadError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("selfdestruct target account was loaded")
            }
        }

        impl Error for SelfdestructTargetLoadError {}

        impl Database for SelfdestructTargetLoadDb {
            type Error = SelfdestructTargetLoadError;

            fn get_account(
                &mut self,
                address: &Address,
            ) -> Result<Option<AccountInfo>, Self::Error> {
                if *address == self.target {
                    self.target_reads.fetch_add(1, Ordering::SeqCst);
                    return Err(SelfdestructTargetLoadError);
                }
                Ok(Some(AccountInfo::default().with_balance(Word::from(1))))
            }

            fn get_code_by_hash(&mut self, _code_hash: &B256) -> Result<Bytecode, Self::Error> {
                Ok(Bytecode::default())
            }

            fn get_storage(
                &mut self,
                _address: &Address,
                _key: &Word,
            ) -> Result<Word, Self::Error> {
                Ok(Word::ZERO)
            }

            fn get_block_hash(&mut self, _number: &Word) -> Result<B256, Self::Error> {
                Ok(B256::ZERO)
            }
        }

        fn selfdestruct_to_code(target: &Address) -> Bytecode {
            let mut code = Vec::new();
            push_address(&mut code, target);
            code.push(op::SELFDESTRUCT);
            legacy_bytecode(code)
        }

        let contract = Address::from([0xbb; 20]);
        let target = Address::from([0xcc; 20]);
        let target_reads = Arc::new(AtomicUsize::new(0));
        let database = SelfdestructTargetLoadDb { target, target_reads: Arc::clone(&target_reads) };
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::BERLIN,
            BlockEnvExt::default(),
            TxRegistry::new(),
            Db::new(database),
            Precompiles::base(SpecId::BERLIN),
        );
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: contract,
            code_address: contract,
            gas_limit: 6_000,
            code: selfdestruct_to_code(&target),
            ..MessageExt::default()
        };
        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::OutOfGas);
        assert_eq!(target_reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frontier_code_deposit_oog_creates_empty_contract() {
        let caller = Address::from([0x11; 20]);
        let created = Address::from([0x22; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&caller, AccountInfo::default().with_balance(Word::from(1)));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::FRONTIER,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::FRONTIER),
        );
        let code =
            Bytecode::new_legacy(Bytes::from_static(&[op::PUSH1, 1, op::PUSH1, 0, op::RETURN]));
        let mut message = MessageExt {
            kind: MessageKind::Create,
            destination: created,
            caller,
            gas_limit: 50,
            code,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert!(result.stop.is_success());

        evm.state.finalize_transaction_(Version::base(SpecId::FRONTIER));
        let pending = evm.state.take_pending_state();
        let account =
            pending.accounts.get(&created).and_then(|entry| entry.present.as_ref()).unwrap();
        assert_eq!(account.code_hash, KECCAK256_EMPTY);
    }

    #[test]
    fn frontier_materialized_empty_account_is_created() {
        let target = Address::from([0x22; 20]);
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::FRONTIER,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::FRONTIER),
        );
        evm.state.account(&target, false).unwrap().touch();

        evm.state.finalize_transaction_(Version::base(SpecId::FRONTIER));
        let pending = evm.state.take_pending_state();

        assert!(pending.accounts.get(&target).unwrap().is_created());
    }

    #[test]
    fn homestead_code_deposit_oog_fails_create() {
        let caller = Address::from([0x11; 20]);
        let created = Address::from([0x22; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&caller, AccountInfo::default().with_balance(Word::from(1)));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::HOMESTEAD,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::HOMESTEAD),
        );
        let code =
            Bytecode::new_legacy(Bytes::from_static(&[op::PUSH1, 1, op::PUSH1, 0, op::RETURN]));
        let mut message = MessageExt {
            kind: MessageKind::Create,
            destination: created,
            caller,
            gas_limit: 50,
            code,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert_eq!(result.stop, InstrStop::OutOfGas);
        assert!(result.output.is_empty());

        evm.state.finalize_transaction_(Version::base(SpecId::HOMESTEAD));
        let pending = evm.state.take_pending_state();
        assert!(pending.accounts.get(&created).is_none_or(|entry| entry.present.is_none()));
    }

    #[test]
    fn create_validation_failure_clears_output() {
        let caller = Address::from([0x11; 20]);
        let created = Address::from([0x22; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&caller, AccountInfo::default().with_balance(Word::from(1)));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::LONDON,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::LONDON),
        );
        let code = Bytecode::new_legacy(Bytes::from_static(&[
            op::PUSH1,
            0xef,
            op::PUSH1,
            0,
            op::MSTORE8,
            op::PUSH1,
            1,
            op::PUSH1,
            0,
            op::RETURN,
        ]));
        let mut message = MessageExt {
            kind: MessageKind::Create,
            destination: created,
            caller,
            gas_limit: 50_000,
            code,
            ..MessageExt::default()
        };
        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::CreateContractStartingWithEF);
        assert!(result.output.is_empty());
    }

    #[test]
    fn create_size_limit_failure_clears_output() {
        let caller = Address::from([0x11; 20]);
        let created = Address::from([0x22; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&caller, AccountInfo::default().with_balance(Word::from(1)));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::SPURIOUS_DRAGON,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::SPURIOUS_DRAGON),
        );
        let code = Bytecode::new_legacy(Bytes::from_static(&[
            op::PUSH2,
            0x60,
            0x01,
            op::PUSH1,
            0,
            op::RETURN,
        ]));
        let mut message = MessageExt {
            kind: MessageKind::Create,
            destination: created,
            caller,
            gas_limit: 100_000,
            code,
            ..MessageExt::default()
        };
        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);

        assert_eq!(result.stop, InstrStop::CreateContractSizeLimit);
        assert!(result.output.is_empty());
    }

    #[test]
    fn staticcall_touches_empty_existing_destination() {
        let target = Address::from([0x11; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&target, AccountInfo::default());
        database.insert_account_storage(&target, &Word::ZERO, &Word::from(1));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::SPURIOUS_DRAGON,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::SPURIOUS_DRAGON),
        );
        let mut message = MessageExt {
            kind: MessageKind::StaticCall,
            destination: target,
            code_address: target,
            gas_limit: 50_000,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert!(result.stop.is_success());

        evm.state.finalize_transaction_(Version::base(SpecId::SPURIOUS_DRAGON));
        let pending = evm.state.take_pending_state();
        let account = pending.accounts.get(&target).expect("empty destination should be deleted");
        assert!(account.original.is_some());
        assert_eq!(account.present, None);
        assert!(pending.storage.get(&target).is_some_and(|overlay| overlay.wiped));
    }

    #[test]
    fn delegatecall_does_not_touch_empty_code_address() {
        let destination = Address::from([0x11; 20]);
        let code_address = Address::from([0x22; 20]);
        let mut database = InMemoryDB::default();
        database
            .insert_account_info(&destination, AccountInfo::default().with_balance(Word::from(1)));
        database.insert_account_info(&code_address, AccountInfo::default());
        database.insert_account_storage(&code_address, &Word::ZERO, &Word::from(1));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::SPURIOUS_DRAGON,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::SPURIOUS_DRAGON),
        );
        let mut message = MessageExt {
            kind: MessageKind::DelegateCall,
            destination,
            code_address,
            gas_limit: 50_000,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert!(result.stop.is_success());

        evm.state.finalize_transaction_(Version::base(SpecId::SPURIOUS_DRAGON));
        let pending = evm.state.take_pending_state();
        assert!(pending.accounts.get(&code_address).is_none_or(|entry| !entry.is_changed()));
    }

    #[test]
    fn account_info_with_code_sets_hash() {
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let info = AccountInfo::default().with_code(code.clone());

        assert_eq!(info.code_hash, code.hash_slow());
    }

    #[test]
    fn transfer_moves_value() {
        let from = Address::from([0x01; 20]);
        let to = Address::from([0x02; 20]);
        let mut state = State::new(InMemoryDB::default());
        state.account(&from, false).unwrap().add_balance(U256::from(10));

        assert!(state.transfer(&from, &to, &U256::from(7)).unwrap());
        assert_eq!(
            state
                .account_info_untracked(&from)
                .expect("sender account should exist")
                .unwrap()
                .balance,
            U256::from(3)
        );
        assert_eq!(
            state
                .account_info_untracked(&to)
                .expect("recipient account should exist")
                .unwrap()
                .balance,
            U256::from(7)
        );
    }

    #[test]
    fn amsterdam_call_value_emits_eip7708_log() {
        let caller = Address::from([0x01; 20]);
        let target = Address::from([0x02; 20]);
        let mut database = InMemoryDB::default();
        database.insert_account_info(&caller, AccountInfo::default().with_balance(U256::from(10)));
        let mut evm = Evm::<BaseEvmTypes>::new(
            SpecId::AMSTERDAM,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::AMSTERDAM),
        );
        let mut message = MessageExt {
            kind: MessageKind::Call,
            destination: target,
            caller,
            value: U256::from(7),
            // Covers the EIP-2780 depth-0 `new_account_state_gas` charge for the
            // value transfer to the empty target account.
            gas_limit: 300_000,
            ..MessageExt::default()
        };

        let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
        assert!(result.stop.is_success());

        let version = *evm.version();
        evm.state.finalize_transaction_(&version);
        let logs = evm.state.take_logs();
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        assert_eq!(log.address, SYSTEM_ADDRESS);
        assert_eq!(
            log.topics(),
            &[
                EIP7708_TRANSFER_TOPIC,
                B256::left_padding_from(caller.as_slice()),
                B256::left_padding_from(target.as_slice()),
            ]
        );
        assert_eq!(log.data.data, Bytes::copy_from_slice(&U256::from(7).to_be_bytes::<32>()));
    }

    #[test]
    fn eip8246_selfdestruct_preserves_balance_at_finalization() {
        let contract = Address::from([0x11; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(
            &contract,
            AccountInfo::default().with_balance(U256::from(5)).with_code(code),
        );
        database.insert_account_storage(&contract, &Word::ZERO, &Word::from(9));
        let mut state = State::new(database);

        state.account(&contract, false).unwrap().mark_destructed();
        state.finalize_transaction_(Version::base(SpecId::AMSTERDAM));

        // EIP-8246: the balance is preserved and the account becomes balance-only (nonce 0, no
        // code).
        let info = state
            .account_info_untracked(&contract)
            .unwrap()
            .expect("balance-only account should remain");
        assert_eq!(info.balance, U256::from(5));
        assert_eq!(info.nonce, 0);
        assert_eq!(info.code_hash, KECCAK256_EMPTY);

        let pending = state.take_pending_state();
        assert!(pending.accounts.contains_key(&contract), "account change recorded");
        assert!(pending.selfdestructs.contains(&contract));
    }

    #[test]
    fn commit_transaction_clears_detached_selfdestructs() {
        let contract = Address::from([0x44; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let mut state = State::new(database);

        state.account(&contract, false).unwrap().mark_destructed();
        state.finalize_transaction_(Version::base(SpecId::AMSTERDAM));
        let pending = state.take_pending_state();

        state.set_pending_state(pending);
        state.commit_transaction();

        assert!(!state.account(&contract, false).unwrap().is_destructed());
    }

    #[test]
    fn eip8246_selfdestruct_zero_balance_is_deleted() {
        let contract = Address::from([0x22; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let mut state = State::new(database);

        state.account(&contract, false).unwrap().mark_destructed();
        state.finalize_transaction_(Version::base(SpecId::AMSTERDAM));

        // A zero-balance balance-only account is empty and deleted by EIP-161.
        assert!(state.account_info_untracked(&contract).unwrap().is_none());
    }

    #[test]
    fn pre_eip8246_selfdestruct_burns_balance_by_deleting_account() {
        let contract = Address::from([0x33; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(
            &contract,
            AccountInfo::default().with_balance(U256::from(5)).with_code(code),
        );
        let mut state = State::new(database);

        state.account(&contract, false).unwrap().mark_destructed();
        state.finalize_transaction_(Version::base(SpecId::PRAGUE));

        // Before EIP-8246 the self-destructed account (and its balance) is deleted at finalization.
        assert!(state.account_info_untracked(&contract).unwrap().is_none());
    }
}
