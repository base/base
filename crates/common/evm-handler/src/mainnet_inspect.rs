use base_evm_context::{ContextSetters, ContextTr, FrameStack, JournalTr};
use base_evm_handler::EthInstructions;
use base_evm_handler::EvmMachine;
use base_evm_handler::{
    EthFrame, EvmTr, EvmTrError, Handler, MainnetHandler, PrecompileProvider, SystemCallTx,
};
use base_state::DatabaseCommit;
use revm_interpreter::InterpreterResult;
use revm_primitives::{Address, Bytes};
use revm_state::EvmState;

use crate::{
    Inspector, InspectorEvmTr, InspectorHandler, JournalExt,
    inspect::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm},
};

// Implementing InspectorHandler for MainnetHandler.
impl<EVM, ERROR> InspectorHandler for MainnetHandler<EVM, ERROR, EthFrame>
where
    EVM: InspectorEvmTr<
            Context: ContextTr<Journal: JournalTr<State = EvmState>>,
            Frame = EthFrame,
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context>,
        >,
    ERROR: EvmTrError<EVM>,
{
}

// Implementing InspectEvm for EvmMachine
impl<CTX, INSP, PRECOMPILES> InspectEvm for EvmMachine<CTX, INSP, PRECOMPILES, EthFrame>
where
    CTX: ContextSetters + ContextTr<Journal: JournalTr<State = EvmState> + JournalExt>,
    INSP: Inspector<CTX>,
    PRECOMPILES: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.set_tx(tx);
        MainnetHandler::default().inspect_run(self)
    }
}

// Implementing InspectCommitEvm for EvmMachine
impl<CTX, INSP, PRECOMPILES> InspectCommitEvm for EvmMachine<CTX, INSP, PRECOMPILES, EthFrame>
where
    CTX: ContextSetters
        + ContextTr<Journal: JournalTr<State = EvmState> + JournalExt, Db: DatabaseCommit>,
    INSP: Inspector<CTX>,
    PRECOMPILES: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}

// Implementing InspectSystemCallEvm for EvmMachine
impl<CTX, INSP, PRECOMPILES> InspectSystemCallEvm for EvmMachine<CTX, INSP, PRECOMPILES, EthFrame>
where
    CTX: ContextSetters
        + ContextTr<Journal: JournalTr<State = EvmState> + JournalExt, Tx: SystemCallTx>,
    INSP: Inspector<CTX>,
    PRECOMPILES: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        // Set system call transaction fields similar to transact_system_call_with_caller
        self.set_tx(CTX::Tx::new_system_tx_with_caller(caller, system_contract_address, data));
        // Use inspect_run_system_call instead of run_system_call for inspection
        MainnetHandler::default().inspect_run_system_call(self)
    }
}

// Implementing InspectorEvmTr for EvmMachine
impl<CTX, INSP, P> InspectorEvmTr for EvmMachine<CTX, INSP, P, EthFrame>
where
    CTX: ContextTr<Journal: JournalExt> + ContextSetters,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
    INSP: Inspector<CTX>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &EthInstructions<Self::Context>,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        let ctx = &self.ctx;
        let frame = &self.frame_stack;
        let instructions = &self.instruction;
        let precompiles = &self.precompiles;
        let inspector = &self.inspector;
        (ctx, instructions, precompiles, frame, inspector)
    }
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut EthInstructions<Self::Context>,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        let ctx = &mut self.ctx;
        let frame = &mut self.frame_stack;
        let instructions = &mut self.instruction;
        let precompiles = &mut self.precompiles;
        let inspector = &mut self.inspector;
        (ctx, instructions, precompiles, frame, inspector)
    }
}
