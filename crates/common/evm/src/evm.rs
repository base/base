use core::ops::{Deref, DerefMut};

use alloy_evm::{Database, Evm, EvmEnv};
use alloy_primitives::{Address, Bytes};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm,
    InspectSystemCallEvm, Inspector, SystemCallEvm,
    context::{
        BlockEnv, ContextError, ContextSetters, Evm as RevmEvm, FrameStack, TxEnv,
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

use crate::{
    BasePrecompiles, OpContext, OpHaltReason, OpSpecId, OpTransaction, OpTransactionError,
    handler::OpHandler,
};

/// Type alias for the inner [`RevmEvm`] parameterized with Base-specific context and fixed
/// [`EthInstructions`] / [`EthFrame`], keeping [`BaseEvm`] field and constructor signatures tidy.
type InnerEvm<DB, I, P> = RevmEvm<
    OpContext<DB>,
    I,
    EthInstructions<EthInterpreter, OpContext<DB>>,
    P,
    EthFrame<EthInterpreter>,
>;

/// The Base EVM, wrapping [`RevmEvm`] with an [`OpContext`] and an optional [`Inspector`].
///
/// Parameterized over a database [`DB`], inspector [`I`], and precompile set [`P`]
/// (defaulting to [`BasePrecompiles`]). All Base-specific context configuration —
/// [`OpSpecId`], [`OpTransaction`], and [`crate::L1BlockInfo`] — is fixed by [`OpContext`].
///
/// The `inspect` flag controls whether [`Inspector`] callbacks are invoked during
/// [`Evm::transact`]. When `false`, the inspector is present in the type but silent,
/// enabling zero-cost tracing toggling at runtime without type changes.
#[allow(missing_debug_implementations)] // revm::Context does not implement Debug
pub struct BaseEvm<DB: Database, I, P = BasePrecompiles> {
    /// Inner revm EVM with Base-specific context, fixed [`EthInstructions`] and
    /// [`EthFrame`], and generic precompile set [`P`].
    pub(crate) inner: InnerEvm<DB, I, P>,
    /// Whether to invoke the [`Inspector`] on each [`Evm::transact`] call.
    pub(crate) inspect: bool,
}

impl<DB: Database, I, P> BaseEvm<DB, I, P> {
    /// Constructs a [`BaseEvm`] from a pre-built [`RevmEvm`] and an inspect flag.
    ///
    /// Prefer [`crate::Builder::build_op`] or [`crate::Builder::build_op_with_inspector`]
    /// to construct from an [`OpContext`] directly.
    pub const fn new(inner: InnerEvm<DB, I, P>, inspect: bool) -> Self {
        Self { inner, inspect }
    }

    /// Returns a reference to the underlying [`OpContext`].
    pub const fn ctx(&self) -> &OpContext<DB> {
        &self.inner.ctx
    }

    /// Returns a mutable reference to the underlying [`OpContext`].
    pub const fn ctx_mut(&mut self) -> &mut OpContext<DB> {
        &mut self.inner.ctx
    }

    /// Consumes `self` and returns the underlying [`OpContext`].
    pub fn into_context(self) -> OpContext<DB> {
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

impl<DB: Database, I, P> Deref for BaseEvm<DB, I, P> {
    type Target = OpContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for BaseEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> EvmTr for BaseEvm<DB, I, P>
where
    DB: Database,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    type Context = OpContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, OpContext<DB>>;
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
    DB: Database,
    OpContext<DB>: ContextTr<Journal: JournalExt> + ContextSetters,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
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
    DB: Database,
    OpContext<DB>: crate::OpContextTr
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    type Tx = OpTransaction<TxEnv>;
    type Block = BlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type ExecutionResult = ExecutionResult<OpHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<DB, I, P> ExecuteCommitEvm for BaseEvm<DB, I, P>
where
    DB: Database + DatabaseCommit,
    OpContext<DB>: crate::OpContextTr
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, I, P> InspectEvm for BaseEvm<DB, I, P>
where
    DB: Database,
    OpContext<DB>: crate::OpContextTr<Journal: JournalExt>
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    type Inspector = I;

    fn set_inspector(&mut self, inspector: I) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<DB, I, P> InspectCommitEvm for BaseEvm<DB, I, P>
where
    DB: Database + DatabaseCommit,
    OpContext<DB>: crate::OpContextTr<Journal: JournalExt>
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
}

impl<DB, I, P> SystemCallEvm for BaseEvm<DB, I, P>
where
    DB: Database,
    OpContext<DB>: crate::OpContextTr<Tx: SystemCallTx>
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(<OpContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.inner.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.run_system_call(self)
    }
}

impl<DB, I, P> InspectSystemCallEvm for BaseEvm<DB, I, P>
where
    DB: Database,
    OpContext<DB>: crate::OpContextTr<Journal: JournalExt, Tx: SystemCallTx>
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv>,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(<OpContext<DB> as ContextTr>::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.inner.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.inspect_run_system_call(self)
    }
}

impl<DB, I, P> Evm for BaseEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
    OpContext<DB>: crate::OpContextTr
        + ContextSetters
        + ContextTr<Db = DB, Tx = OpTransaction<TxEnv>, Block = BlockEnv, Journal: JournalExt>,
{
    type DB = DB;
    type Tx = OpTransaction<TxEnv>;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    /// Executes `tx`, invoking the [`Inspector`] iff `self.inspect` is `true`.
    /// Uses [`InspectEvm::inspect_tx`] for the instrumented path and [`ExecuteEvm::transact`]
    /// for the uninstrumented path; both finalize the journal and return [`ResultAndState`].
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
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
    use revm::{context::CfgEnv, database::EmptyDB};
    use rstest::rstest;

    use super::*;
    use crate::{
        BaseEvmFactory, OpSpecId,
        precompiles::{bls12_381, bn254_pair},
    };

    #[rstest]
    #[case::bn254_pair(*bn254_pair::JOVIAN.address(), bn254_pair::JOVIAN_MAX_INPUT_SIZE)]
    #[case::bls12_g1_msm(*bls12_381::JOVIAN_G1_MSM.address(), bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_g2_msm(*bls12_381::JOVIAN_G2_MSM.address(), bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_pairing(*bls12_381::JOVIAN_PAIRING.address(), bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE)]
    fn precompile_jovian_at_max_input(#[case] address: Address, #[case] max_size: usize) {
        let mut evm = BaseEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(CfgEnv::new_with_spec(OpSpecId::JOVIAN), BlockEnv::default()),
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
            internals: EvmInternals::from_context(ctx),
        });
        assert!(result.is_ok(), "precompile {address} should succeed at max input size");
    }

    #[rstest]
    #[case::bn254_pair(*bn254_pair::JOVIAN.address(), bn254_pair::JOVIAN_MAX_INPUT_SIZE)]
    #[case::bls12_g1_msm(*bls12_381::JOVIAN_G1_MSM.address(), bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_g2_msm(*bls12_381::JOVIAN_G2_MSM.address(), bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE)]
    #[case::bls12_pairing(*bls12_381::JOVIAN_PAIRING.address(), bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE)]
    fn precompile_jovian_over_max_input(#[case] address: Address, #[case] max_size: usize) {
        let mut evm = BaseEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(CfgEnv::new_with_spec(OpSpecId::JOVIAN), BlockEnv::default()),
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
            internals: EvmInternals::from_context(ctx),
        });
        assert!(result.is_err(), "precompile {address} should fail over max input size");
    }
}
