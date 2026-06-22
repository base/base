use core::ops::{Deref, DerefMut};

use alloy_evm::{Database as AlloyDatabase, Evm, EvmEnv};
use alloy_primitives::{Address, Bytes};
use revm::{
    Database as RevmDatabase, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm,
    InspectEvm, InspectSystemCallEvm, Inspector, SystemCallEvm,
    context::{
        BlockEnv, CfgEnv, ContextError, ContextSetters, Evm as RevmEvm, FrameStack, TxEnv,
        result::ExecResultAndState,
    },
    context_interface::{
        ContextTr, JournalTr,
        result::{EVMError, ExecutionResult, ResultAndState},
    },
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, Handler, ItemOrResult, PrecompileProvider,
        SystemCallTx, evm::FrameTr, instructions::EthInstructions,
    },
    inspector::{InspectorEvmTr, InspectorHandler, JournalExt},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    state::EvmState,
};

#[cfg(feature = "std")]
use crate::Eip8130Executor;
use crate::{
    BaseContext, BaseHaltReason, BasePrecompiles, BaseSpecId, BaseTransaction,
    BaseTransactionError, BaseTxTr, handler::BaseHandler,
};

/// Type alias for the inner [`RevmEvm`] parameterized with Base-specific context and fixed
/// [`EthInstructions`] / [`EthFrame`], keeping [`BaseEvm`] field and constructor signatures tidy.
type InnerEvm<DB, I, P> = RevmEvm<
    BaseContext<DB>,
    I,
    EthInstructions<EthInterpreter, BaseContext<DB>>,
    P,
    EthFrame<EthInterpreter>,
>;

/// The Base EVM, wrapping [`RevmEvm`] with a [`BaseContext`] and an optional [`Inspector`].
///
/// Parameterized over a database [`DB`], inspector [`I`], and precompile set [`P`]
/// (defaulting to [`BasePrecompiles`]). All Base-specific context configuration —
/// [`BaseSpecId`], [`BaseTransaction`], and [`crate::L1BlockInfo`] — is fixed by [`BaseContext`].
///
/// The `inspect` flag controls whether [`Inspector`] callbacks are invoked during
/// [`Evm::transact`]. When `false`, the inspector is present in the type but silent,
/// enabling zero-cost tracing toggling at runtime without type changes.
#[allow(missing_debug_implementations)] // revm::Context does not implement Debug
pub struct BaseEvm<DB: RevmDatabase, I, P = BasePrecompiles> {
    /// Inner revm EVM with Base-specific context, fixed [`EthInstructions`] and
    /// [`EthFrame`], and generic precompile set [`P`].
    pub(crate) inner: InnerEvm<DB, I, P>,
    /// Whether to invoke the [`Inspector`] on each [`Evm::transact`] call.
    pub(crate) inspect: bool,
}

impl<DB: RevmDatabase, I, P> BaseEvm<DB, I, P> {
    /// Constructs a [`BaseEvm`] from a pre-built [`RevmEvm`] and an inspect flag.
    ///
    /// Prefer [`crate::Builder::build_base`] or [`crate::Builder::build_with_inspector`]
    /// to construct from a [`BaseContext`] directly.
    pub const fn new(inner: InnerEvm<DB, I, P>, inspect: bool) -> Self {
        Self { inner, inspect }
    }

    /// Returns a reference to the underlying [`BaseContext`].
    pub const fn ctx(&self) -> &BaseContext<DB> {
        &self.inner.ctx
    }

    /// Returns a mutable reference to the underlying [`BaseContext`].
    pub const fn ctx_mut(&mut self) -> &mut BaseContext<DB> {
        &mut self.inner.ctx
    }

    /// Consumes `self` and returns the underlying [`BaseContext`].
    pub fn into_context(self) -> BaseContext<DB> {
        self.inner.ctx
    }

    /// Consumes `self` and returns the inspector.
    pub fn into_inspector(self) -> I {
        self.inner.inspector
    }

    /// Consumes `self` and returns a new [`BaseEvm`] with the given inspector, preserving
    /// the inspect flag. Used to swap inspectors without rebuilding from context.
    pub fn with_inspector<J>(self, inspector: J) -> BaseEvm<DB, J, P> {
        BaseEvm { inner: self.inner.with_inspector(inspector), inspect: self.inspect }
    }

    /// Consumes `self` and returns a new [`BaseEvm`] with the given precompile set,
    /// preserving the inspect flag. Used to substitute [`BasePrecompiles`] with
    /// custom implementations such as FPVM-accelerated precompiles in the proof system.
    pub fn with_precompiles<Q>(self, precompiles: Q) -> BaseEvm<DB, I, Q> {
        BaseEvm { inner: self.inner.with_precompiles(precompiles), inspect: self.inspect }
    }
}

impl<DB: RevmDatabase, I, P> Deref for BaseEvm<DB, I, P> {
    type Target = BaseContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: RevmDatabase, I, P> DerefMut for BaseEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> EvmTr for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    type Context = BaseContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, BaseContext<DB>>;
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    #[inline]
    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        self.inner.all()
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<DB::Error>,
    > {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(&mut self) -> Result<FrameInitOrResult<Self::Frame>, ContextError<DB::Error>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<Option<<Self::Frame as FrameTr>::FrameResult>, ContextError<DB::Error>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, I, P> InspectorEvmTr for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    BaseContext<DB>: ContextTr<Journal: JournalExt> + ContextSetters,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    type Inspector = I;

    #[inline]
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    #[inline]
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }
}

impl<DB, I, P> ExecuteEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    BaseContext<DB>: crate::BaseContextTr
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    type Tx = BaseTransaction<TxEnv>;
    type Block = BlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, BaseTransactionError>;
    type ExecutionResult = ExecutionResult<BaseHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        // EIP-8130 transactions must run through the enshrined pre-call pipeline
        // (`Evm::transact_raw`); the standard single-frame handler would
        // mis-execute the placeholder `TxEnv`. Reject them here so the failure
        // is explicit rather than silent for any direct `ExecuteEvm` caller.
        if tx.is_eip8130() {
            return Err(EVMError::Transaction(BaseTransactionError::eip8130(
                "EIP-8130 transactions must be executed via Evm::transact_raw",
            )));
        }
        self.inner.ctx.set_tx(tx);
        let mut h = BaseHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = BaseHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<DB, I, P> ExecuteCommitEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase + DatabaseCommit,
    BaseContext<DB>: crate::BaseContextTr
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, I, P> InspectEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    BaseContext<DB>: crate::BaseContextTr<Journal: JournalExt>
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    type Inspector = I;

    fn set_inspector(&mut self, inspector: I) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        // See `transact_one`: EIP-8130 transactions are not handled by the
        // single-frame (inspector) handler and must use `Evm::transact_raw`.
        if tx.is_eip8130() {
            return Err(EVMError::Transaction(BaseTransactionError::eip8130(
                "EIP-8130 transactions must be executed via Evm::transact_raw",
            )));
        }
        self.inner.ctx.set_tx(tx);
        let mut h = BaseHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<DB, I, P> InspectCommitEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase + DatabaseCommit,
    BaseContext<DB>: crate::BaseContextTr<Journal: JournalExt>
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
}

impl<DB, I, P> SystemCallEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    BaseContext<DB>: crate::BaseContextTr<Tx: SystemCallTx>
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(<BaseContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = BaseHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.inner.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.run_system_call(self)
    }
}

impl<DB, I, P> InspectSystemCallEvm for BaseEvm<DB, I, P>
where
    DB: RevmDatabase,
    BaseContext<DB>: crate::BaseContextTr<Journal: JournalExt, Tx: SystemCallTx>
        + ContextSetters
        + ContextTr<Db = DB, Tx = BaseTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(<BaseContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = BaseHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.inner.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.inspect_run_system_call(self)
    }
}

// The `Journal: core::fmt::Debug` bound is required by the EIP-8130 path:
// `Eip8130Executor::execute` borrows the journal through
// `EvmInternals::from_context`, whose bounds require `Debug`. This is a public
// API surface addition over the prior `JournalExt`-only bound, but revm's
// journal types derive `Debug`, so it is satisfied by every in-tree consumer.
impl<DB, I, P> Evm for BaseEvm<DB, I, P>
where
    DB: AlloyDatabase,
    I: Inspector<BaseContext<DB>>,
    P: PrecompileProvider<BaseContext<DB>, Output = InterpreterResult>,
    BaseContext<DB>: crate::BaseContextTr
        + ContextSetters
        + ContextTr<
            Db = DB,
            Tx = BaseTransaction<TxEnv>,
            Block = BlockEnv,
            Journal: JournalExt + core::fmt::Debug,
        >,
{
    type DB = DB;
    type Tx = BaseTransaction<TxEnv>;
    type Error = EVMError<DB::Error, BaseTransactionError>;
    type HaltReason = BaseHaltReason;
    type Spec = BaseSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.cfg
    }

    /// Executes `tx`, invoking the [`Inspector`] iff `self.inspect` is `true`.
    /// Uses [`InspectEvm::inspect_tx`] for the instrumented path and [`ExecuteEvm::transact`]
    /// for the uninstrumented path; both finalize the journal and return [`ResultAndState`].
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // EIP-8130 transactions are executed by the enshrined pre-call pipeline,
        // around (not inside) an EVM call frame, so they bypass both the mainnet
        // single-frame handler and the inspector frame loop. The pipeline lives
        // behind the `std` feature; `no_std` builds reject 8130 transactions
        // rather than mis-executing the placeholder `TxEnv`.
        //
        // The inspector is intentionally not driven here: there is no call frame
        // to step through, and the `Inspector` trait has no transaction-level
        // start/end hook to emit instead. Tracing integration for 8130 is
        // deferred until the path is reachable via RPC; until then 8130 txns
        // produce no inspector output.
        if tx.is_eip8130() {
            #[cfg(feature = "std")]
            {
                self.inner.ctx.set_tx(tx);
                let result = Eip8130Executor::execute(self.ctx_mut())?;
                let state = self.inner.ctx.journal_mut().finalize();
                return Ok(ResultAndState::new(result, state));
            }
            #[cfg(not(feature = "std"))]
            {
                let _ = tx;
                return Err(EVMError::Transaction(BaseTransactionError::eip8130(
                    "EIP-8130 execution is unavailable in no_std builds",
                )));
            }
        }
        if self.inspect { InspectEvm::inspect_tx(self, tx) } else { ExecuteEvm::transact(self, tx) }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        SystemCallEvm::system_call_with_caller(self, caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let revm::Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (&self.inner.ctx.journaled_state.database, &self.inner.inspector, &self.inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_evm::{
        EvmFactory, EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::{Address, U256};
    use base_common_precompiles::{
        JOVIAN, JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE, JOVIAN_G2_MSM,
        JOVIAN_G2_MSM_MAX_INPUT_SIZE, JOVIAN_MAX_INPUT_SIZE, JOVIAN_PAIRING,
        JOVIAN_PAIRING_MAX_INPUT_SIZE,
    };
    use revm::{context::CfgEnv, database::EmptyDB};
    use rstest::rstest;

    use super::*;
    use crate::{BaseEvmFactory, BaseSpecId, BaseUpgrade};

    #[rstest]
    #[case::bn254_pair(*JOVIAN.address(), JOVIAN_MAX_INPUT_SIZE)]
    #[case::bls12_g1_msm(*JOVIAN_G1_MSM.address(), JOVIAN_G1_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_g2_msm(*JOVIAN_G2_MSM.address(), JOVIAN_G2_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_pairing(*JOVIAN_PAIRING.address(), JOVIAN_PAIRING_MAX_INPUT_SIZE)]
    fn precompile_jovian_at_max_input(#[case] address: Address, #[case] max_size: usize) {
        let mut evm = BaseEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian)),
                BlockEnv::default(),
            ),
        );
        let (precompiles, ctx) = (&mut evm.inner.precompiles, &mut evm.inner.ctx);
        let precompile = precompiles.get(&address).unwrap();
        let result = precompile.call(PrecompileInput {
            data: &vec![0; max_size],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            reservoir: 0,
            internals: EvmInternals::from_context(ctx),
        });
        assert!(result.is_ok(), "precompile {address} should succeed at max input size");
    }

    #[rstest]
    #[case::bn254_pair(*JOVIAN.address(), JOVIAN_MAX_INPUT_SIZE)]
    #[case::bls12_g1_msm(*JOVIAN_G1_MSM.address(), JOVIAN_G1_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_g2_msm(*JOVIAN_G2_MSM.address(), JOVIAN_G2_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_pairing(*JOVIAN_PAIRING.address(), JOVIAN_PAIRING_MAX_INPUT_SIZE)]
    fn precompile_jovian_over_max_input(#[case] address: Address, #[case] max_size: usize) {
        let mut evm = BaseEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Jovian)),
                BlockEnv::default(),
            ),
        );
        let (precompiles, ctx) = (&mut evm.inner.precompiles, &mut evm.inner.ctx);
        let precompile = precompiles.get(&address).unwrap();
        let result = precompile.call(PrecompileInput {
            data: &vec![0; max_size + 1],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            reservoir: 0,
            internals: EvmInternals::from_context(ctx),
        });
        assert!(
            matches!(&result, Ok(output) if output.halt_reason().is_some()),
            "precompile {address} should halt over max input size, got {result:?}"
        );
    }
}
