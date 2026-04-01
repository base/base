//! Implementation of the [`ExecuteEvm`] trait for the [`OpRevmEvm`].
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, result::ExecResultAndState},
    context_interface::{
        Cfg, ContextTr, Database, JournalTr,
        result::{EVMError, ExecutionResult},
    },
    handler::{
        EthFrame, Handler, PrecompileProvider, SystemCallTx, instructions::EthInstructions,
        system_call::SystemCallEvm,
    },
    inspector::{
        InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorHandler, JournalExt,
    },
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    primitives::{Address, Bytes},
    state::EvmState,
};

use crate::{
    L1BlockInfo, OpHaltReason, OpSpecId, OpTransactionError, core_evm::OpRevmEvm,
    handler::OpHandler, transaction::OpTxTr,
};

/// Type alias for Base context
pub trait OpContextTr:
    ContextTr<
        Journal: JournalTr<State = EvmState>,
        Tx: OpTxTr,
        Cfg: Cfg<Spec = OpSpecId>,
        Chain = L1BlockInfo,
    >
{
}

impl<T> OpContextTr for T where
    T: ContextTr<
            Journal: JournalTr<State = EvmState>,
            Tx: OpTxTr,
            Cfg: Cfg<Spec = OpSpecId>,
            Chain = L1BlockInfo,
        >
{
}

/// Type alias for the error type of the `OpRevmEvm`.
pub type OpError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, OpTransactionError>;

impl<CTX, INSP, PRECOMPILE> ExecuteEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Tx = <CTX as ContextTr>::Tx;
    type Block = <CTX as ContextTr>::Block;
    type State = EvmState;
    type Error = OpError<CTX>;
    type ExecutionResult = ExecutionResult<OpHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.0.ctx.journal_mut().finalize()
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

impl<CTX, INSP, PRECOMPILE> ExecuteCommitEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr<Db: DatabaseCommit> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.0.ctx.db_mut().commit(state);
    }
}

impl<CTX, INSP, PRECOMPILE> InspectEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr<Journal: JournalExt> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectCommitEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}

impl<CTX, INSP, PRECOMPILE> SystemCallEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr<Tx: SystemCallTx> + ContextSetters,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.0.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.run_system_call(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectSystemCallEvm
    for OpRevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: OpContextTr<Journal: JournalExt, Tx: SystemCallTx> + ContextSetters,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = OpHandler::<_, _, EthFrame<EthInterpreter>>::new();

        // load caller account into the journal (necessary for Geth proofs compatibility)
        // remove once https://github.com/bluealloy/revm/issues/3484 is fixed
        self.0.ctx.journal_mut().load_account_with_code_mut(caller)?;

        h.inspect_run_system_call(self)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use revm::{
        ExecuteEvm, SystemCallEvm,
        database::{InMemoryDB, State},
    };

    use crate::{DefaultOp, OpBuilder, OpContext};

    /// Verifies that the system call caller is loaded into the EVM state cache so it appears in the
    /// execution witness.
    ///
    /// The state cache (`State.cache.accounts`) is exactly what `ExecutionWitnessRecord` reads to
    /// build the `hashed_state` fed to `state_provider.witness(...)`. Without the
    /// `load_account_with_code_mut` call in `system_call_one_with_caller`, the caller account
    /// would not be cached and would be absent from the generated witness, breaking Geth proof
    /// compatibility.
    ///
    /// See: <https://github.com/bluealloy/revm/issues/3484>
    #[test]
    fn system_call_caller_appears_in_witness() {
        let caller = Address::repeat_byte(0xCA);
        let contract = Address::repeat_byte(0xAB);

        // Use State with bundle tracking, mirroring the witness generation path in
        // OpBuilder::witness and debug_executionWitness.
        let state =
            State::builder().with_database(InMemoryDB::default()).with_bundle_update().build();

        let ctx = OpContext::op().with_db(state);
        let mut evm = ctx.build_op();

        // Execute a system call. This internally calls `load_account_with_code_mut(caller)`,
        // causing the State DB to load and cache the caller's account in `State.cache.accounts`.
        let _ = evm.system_call_one_with_caller(caller, contract, Default::default());

        // Finalize to flush the journal, then inspect the underlying State cache.
        // `ExecutionWitnessRecord::from_executed_state` iterates `State.cache.accounts` to build
        // the hashed state, so the caller must appear here to be included in the witness.
        let _ = evm.finalize();
        let state = evm.into_context().journaled_state.database;

        assert!(
            state.cache.accounts.contains_key(&caller),
            "system call caller must be in state cache for Geth proof compatibility"
        );
    }
}
