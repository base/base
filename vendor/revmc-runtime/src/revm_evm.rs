//! Generic `revm` JIT EVM.
//!
//! Provides [`JitEvm`] which wraps any mainnet-shaped [`EvmTr`]-based EVM and overrides
//! execution to look up compiled functions via [`JitBackend`] before falling
//! back to the interpreter.

use std::sync::Arc;

use alloy_primitives::{Address, Bytes};
use revm_context::{Host, JournalEntry};
use revm_context_interface::{
    Cfg, ContextSetters, ContextTr, Database,
    journaled_state::JournalTr,
    result::{EVMError, HaltReason, InvalidTransaction, ResultAndState},
};
use revm_database_interface::DatabaseCommit;
use revm_handler::{
    EthFrame, EvmTr, ExecuteCommitEvm, ExecuteEvm, FrameInitOrResult, FrameTr, Handler,
    ItemOrResult, MainnetHandler, PrecompileProvider, SystemCallCommitEvm, SystemCallEvm,
    SystemCallTx, evm::ContextDbError,
};
use revm_inspector::{
    InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorEvmTr, InspectorFrame,
    InspectorHandler, JournalExt,
};
use revm_interpreter::{InstructionResult, InterpreterAction, InterpreterResult, InterpreterTypes};
use revm_primitives::{B256Map, hardfork::SpecId};
use revm_state::EvmState;

use crate::runtime::{JitBackend, LookupDecision, LookupRequest, RuntimeCacheKey};

/// Mainnet EVM with JIT-compiled function dispatch.
///
/// Wraps an inner [`EvmTr`] and uses [`JitBackend`] to look up compiled
/// functions before falling back to the interpreter.
///
/// Implements [`ExecuteEvm`] so it can be used as a drop-in replacement for
/// the standard mainnet EVM.
#[derive(derive_more::Debug)]
pub struct JitEvm<EVM> {
    inner: EVM,
    backend: JitBackend,
    /// Cached lookup decisions keyed by `code_hash` alone.
    /// Invalidated when the `spec_id` changes.
    #[debug(skip)]
    lookup_cache: B256Map<LookupDecision>,
    /// The `spec_id` the cache was built for; cleared on mismatch.
    lookup_cache_spec_id: SpecId,
}

impl<EVM> JitEvm<EVM> {
    /// Returns a reference to the inner EVM.
    pub const fn inner(&self) -> &EVM {
        &self.inner
    }

    /// Returns a mutable reference to the inner EVM.
    pub fn inner_mut(&mut self) -> &mut EVM {
        &mut self.inner
    }

    /// Consumes the JIT EVM and returns the inner EVM.
    pub fn into_inner(self) -> EVM {
        self.inner
    }

    /// Returns a reference to the JIT backend.
    pub const fn backend(&self) -> &JitBackend {
        &self.backend
    }

    /// Returns a mutable reference to the JIT backend.
    pub fn backend_mut(&mut self) -> &mut JitBackend {
        &mut self.backend
    }

    /// Replaces the JIT backend and clears the lookup cache.
    pub fn set_backend(&mut self, backend: JitBackend) {
        self.backend = backend;
        self.lookup_cache.clear();
    }
}

impl<EVM: EvmTr> JitEvm<EVM>
where
    EVM::Context: ContextTr,
{
    /// Creates a new JIT EVM with a disabled backend.
    ///
    /// Equivalent to `JitEvm::new(inner, JitBackend::disabled())`.
    pub fn disabled(inner: EVM) -> Self {
        Self::new(inner, JitBackend::disabled())
    }

    /// Creates a new JIT EVM from an inner EVM and backend.
    pub fn new(inner: EVM, backend: JitBackend) -> Self {
        let spec_id: SpecId = inner.ctx_ref().cfg().spec().into();
        Self {
            inner,
            backend,
            lookup_cache: B256Map::with_capacity_and_hasher(16, Default::default()),
            lookup_cache_spec_id: spec_id,
        }
    }

    fn invalidate_cache(&mut self) {
        let spec_id: SpecId = self.inner.ctx_ref().cfg().spec().into();
        if spec_id != self.lookup_cache_spec_id {
            self.lookup_cache.clear();
            self.lookup_cache_spec_id = spec_id;
            // } else {
            //     self.lookup_cache.retain(|_, v| matches!(v, LookupDecision::Compiled(_)));
        }
    }
}

impl<EVM> core::ops::Deref for JitEvm<EVM> {
    type Target = EVM;

    #[inline]
    fn deref(&self) -> &EVM {
        &self.inner
    }
}

impl<EVM> core::ops::DerefMut for JitEvm<EVM> {
    #[inline]
    fn deref_mut(&mut self) -> &mut EVM {
        &mut self.inner
    }
}

impl<EVM> EvmTr for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        >,
{
    type Context = EVM::Context;
    type Instructions = EVM::Instructions;
    type Precompiles = EVM::Precompiles;
    type Frame = EVM::Frame;

    #[inline]
    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &revm_context_interface::FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut revm_context_interface::FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    #[inline]
    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        self.inner.frame_init(frame_input)
    }

    #[inline]
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let spec_id: SpecId = self.inner.ctx_ref().cfg().spec().into();
        let frame = self.inner.frame_stack().get();
        let code_hash = frame.interpreter.bytecode.get_or_calculate_hash();

        let decision = self.lookup_cache.entry(code_hash).or_insert_with(|| {
            let code = frame.interpreter.bytecode.original_bytes();
            self.backend.lookup(LookupRequest { key: RuntimeCacheKey { code_hash, spec_id }, code })
        });

        Ok(match decision {
            LookupDecision::Compiled(program) => {
                let (ctx, _, _, frame_stack) = self.inner.all_mut();
                let frame = frame_stack.get();
                let action =
                    unsafe { program.func.call_with_interpreter(&mut frame.interpreter, ctx) };
                frame.process_next_action::<_, ContextDbError<Self::Context>>(ctx, action).inspect(
                    |i| {
                        if i.is_result() {
                            frame.set_finished(true);
                        }
                    },
                )?
            }
            LookupDecision::Interpret(_) => self.inner.frame_run()?,
        })
    }

    #[inline]
    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<Option<<Self::Frame as FrameTr>::FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<EVM> ExecuteEvm for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Context: ContextTr<Journal: JournalTr<State = EvmState>> + ContextSetters,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        >,
{
    type ExecutionResult = revm_context_interface::result::ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = EVMError<<<EVM::Context as ContextTr>::Db as Database>::Error, InvalidTransaction>;
    type Tx = <EVM::Context as ContextTr>::Tx;
    type Block = <EVM::Context as ContextTr>::Block;

    #[inline]
    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx_mut().set_tx(tx);
        self.invalidate_cache();
        MainnetHandler::default().run(self)
    }

    #[inline]
    fn finalize(&mut self) -> Self::State {
        self.ctx_mut().journal_mut().finalize()
    }

    #[inline]
    fn set_block(&mut self, block: Self::Block) {
        self.ctx_mut().set_block(block);
    }

    #[inline]
    fn replay(&mut self) -> Result<ResultAndState<HaltReason>, Self::Error> {
        self.invalidate_cache();
        MainnetHandler::default().run(self).map(|result| {
            let state = self.ctx_mut().journal_mut().finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<EVM> ExecuteCommitEvm for JitEvm<EVM>
where
    Self: ExecuteEvm<State = EvmState>,
    EVM: EvmTr<Context: ContextTr<Db: DatabaseCommit>>,
{
    #[inline]
    fn commit(&mut self, state: Self::State) {
        self.ctx_mut().db_mut().commit(state);
    }
}

impl<EVM> SystemCallEvm for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Context: ContextTr<Journal: JournalTr<State = EvmState>, Tx: SystemCallTx>
                         + ContextSetters,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        >,
{
    #[inline]
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx_mut().set_tx(
            <<EVM::Context as ContextTr>::Tx as SystemCallTx>::new_system_tx_with_caller(
                caller,
                system_contract_address,
                data,
            ),
        );
        self.invalidate_cache();
        MainnetHandler::default().run_system_call(self)
    }
}

impl<EVM> SystemCallCommitEvm for JitEvm<EVM>
where
    Self: SystemCallEvm<State = EvmState> + ExecuteCommitEvm,
    EVM: EvmTr<Context: ContextTr<Db: DatabaseCommit>>,
{
    #[inline]
    fn system_call_with_caller_commit(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.system_call_with_caller(caller, system_contract_address, data).map(|output| {
            self.ctx_mut().db_mut().commit(output.state);
            output.result
        })
    }
}

impl<EVM> InspectorEvmTr for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        > + InspectorEvmTr,
{
    type Inspector = <EVM as InspectorEvmTr>::Inspector;

    #[inline]
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &revm_context_interface::FrameStack<Self::Frame>,
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
        &mut revm_context_interface::FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }

    #[inline]
    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let spec_id: SpecId = self.inner.ctx_ref().cfg().spec().into();
        let frame = self.inner.frame_stack().get();
        let code_hash = frame.interpreter.bytecode.get_or_calculate_hash();

        let decision = self.lookup_cache.entry(code_hash).or_insert_with(|| {
            let code = frame.interpreter.bytecode.original_bytes();
            self.backend.lookup(LookupRequest { key: RuntimeCacheKey { code_hash, spec_id }, code })
        });

        let program = match decision {
            LookupDecision::Compiled(program) => Arc::clone(program),
            LookupDecision::Interpret(_) => return self.inner.inspect_frame_run(),
        };

        // Set up the on_log callback to forward logs to the inspector during JIT execution.
        //
        // SAFETY: We dereference raw pointers to the inspector and context inside the closure.
        // This creates aliasing &mut references with `ecx.host` (which borrows the context),
        // but the inspector's `log()` implementation only accesses the inspector itself,
        // not the journaled state already borrowed by the host.
        let (ctx, inspector) = self.ctx_inspector();
        let ctx_ptr: *mut EVM::Context = ctx;
        let inspector_ptr: *mut <EVM as InspectorEvmTr>::Inspector = inspector;
        let mut on_log = move |log: &revm_primitives::Log| unsafe {
            (*inspector_ptr).log(&mut *ctx_ptr, log.clone());
        };

        let (ctx, _, _, frame_stack) = self.inner.all_mut();
        let frame = frame_stack.get();
        let action = unsafe {
            program.func.call_with_interpreter_with(&mut frame.interpreter, ctx, |ecx| {
                // SAFETY: `on_log` lives on the stack and outlives the JIT call.
                // The closure captures raw pointers whose types may not be
                // `'static`, so we erase the lifetime via pointer cast.
                ecx.on_log = Some(core::mem::transmute::<
                    &mut dyn FnMut(&revm_primitives::Log),
                    &mut (dyn FnMut(&revm_primitives::Log) + '_),
                >(&mut on_log));
            })
        };

        // Handle selfdestruct.
        let (ctx, inspector) = self.ctx_inspector();
        if let InterpreterAction::Return(result) = &action
            && result.result == InstructionResult::SelfDestruct
        {
            inspect_selfdestruct(ctx, inspector);
        }

        let (ctx, _, _, frame_stack) = self.inner.all_mut();
        let frame = frame_stack.get();
        let mut result = frame.process_next_action::<_, ContextDbError<Self::Context>>(ctx, action);

        if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
            let (ctx, inspector, frame) = self.ctx_inspector_frame();
            if let Some(frame) = frame.eth_frame() {
                revm_inspector::handler::frame_end(ctx, inspector, &frame.input, frame_result);
                frame.set_finished(true);
            }
        }
        result
    }
}

impl<EVM> InspectEvm for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Context: ContextTr<Journal: JournalTr<State = EvmState> + JournalExt> + ContextSetters,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        > + InspectorEvmTr,
{
    type Inspector = <EVM as InspectorEvmTr>::Inspector;

    #[inline]
    fn set_inspector(&mut self, inspector: Self::Inspector) {
        *self.inner.inspector() = inspector;
    }

    #[inline]
    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx_mut().set_tx(tx);
        self.invalidate_cache();
        MainnetHandler::default().inspect_run(self)
    }
}

impl<EVM> InspectCommitEvm for JitEvm<EVM> where Self: InspectEvm + ExecuteCommitEvm {}

impl<EVM> InspectSystemCallEvm for JitEvm<EVM>
where
    EVM: EvmTr<
            Frame = EthFrame,
            Context: ContextTr<
                Journal: JournalTr<State = EvmState> + JournalExt,
                Tx: SystemCallTx,
            > + ContextSetters,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
        > + InspectorEvmTr,
{
    #[inline]
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx_mut().set_tx(
            <<EVM::Context as ContextTr>::Tx as SystemCallTx>::new_system_tx_with_caller(
                caller,
                system_contract_address,
                data,
            ),
        );
        self.invalidate_cache();
        MainnetHandler::default().inspect_run_system_call(self)
    }
}

#[inline(never)]
#[cold]
fn inspect_selfdestruct<CTX, IT>(context: &mut CTX, inspector: &mut impl Inspector<CTX, IT>)
where
    CTX: ContextTr<Journal: JournalExt> + Host,
    IT: InterpreterTypes,
{
    if let Some(
        JournalEntry::AccountDestroyed {
            address: contract, target: to, had_balance: balance, ..
        }
        | JournalEntry::BalanceTransfer { from: contract, to, balance, .. },
    ) = context.journal_mut().journal().last()
    {
        inspector.selfdestruct(*contract, *to, *balance);
    }
}

#[cfg(test)]
#[cfg(feature = "llvm")]
mod tests {
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use revm_bytecode::opcode as op;
    use revm_context::TxEnv;
    use revm_context_interface::result::{ExecutionResult, Output};
    use revm_database::{CacheDB, EmptyDB};
    use revm_handler::MainBuilder;

    use super::*;
    use crate::runtime::{JitBackend, RuntimeConfig};

    fn blocking_backend() -> JitBackend {
        JitBackend::new(RuntimeConfig { blocking: true, ..Default::default() }).unwrap()
    }

    type TestInnerEvm = revm_handler::MainnetEvm<
        revm_context::Context<
            revm_context::BlockEnv,
            TxEnv,
            revm_context::CfgEnv,
            CacheDB<EmptyDB>,
        >,
    >;

    fn test_jit_evm_with_spec(backend: JitBackend, spec_id: SpecId) -> JitEvm<TestInnerEvm> {
        let inner = revm_context::Context::new(CacheDB::default(), spec_id).build_mainnet();
        JitEvm::new(inner, backend)
    }

    fn test_jit_evm(backend: JitBackend) -> JitEvm<TestInnerEvm> {
        test_jit_evm_with_spec(backend, SpecId::CANCUN)
    }

    fn deploy_contract_with_nonce(
        evm: &mut JitEvm<TestInnerEvm>,
        bytecode: &[u8],
        nonce: u64,
    ) -> Address {
        let len = bytecode.len();
        assert!(len <= 255);
        let offset = 10u8;
        let mut deploy_code = vec![
            op::PUSH1,
            len as u8,
            op::PUSH1,
            offset,
            op::PUSH0,
            op::CODECOPY,
            op::PUSH1,
            len as u8,
            op::PUSH0,
            op::RETURN,
        ];
        deploy_code.extend_from_slice(bytecode);

        let tx = TxEnv {
            kind: TxKind::Create,
            nonce,
            data: Bytes::copy_from_slice(&deploy_code),
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact_commit(tx).unwrap();
        match result {
            ExecutionResult::Success { output, .. } => match output {
                Output::Create(_, Some(addr)) => addr,
                other => panic!("expected Create output, got: {other:?}"),
            },
            other => panic!("expected Success, got: {other:?}"),
        }
    }

    fn deploy_contract(evm: &mut JitEvm<TestInnerEvm>, bytecode: &[u8]) -> Address {
        deploy_contract_with_nonce(evm, bytecode, 0)
    }

    fn call_contract(backend: JitBackend, spec_id: SpecId, bytecode: &[u8]) -> ExecutionResult {
        let mut evm = test_jit_evm_with_spec(backend, spec_id);
        let contract_addr = deploy_contract(&mut evm, bytecode);
        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        evm.transact(tx).unwrap().result
    }

    #[test]
    fn jit_evm_simple_return() {
        let backend = blocking_backend();

        // PUSH1 0x42 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] = &[0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        let mut evm = test_jit_evm(backend.clone());
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact(tx).unwrap();
        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = match &output {
                    Output::Call(bytes) => bytes,
                    other => panic!("expected Call output, got: {other:?}"),
                };
                assert_eq!(U256::from_be_slice(data), U256::from(0x42));
            }
            other => panic!("expected Success, got: {other:?}"),
        }

        let stats = backend.stats();
        assert!(stats.compilations_succeeded > 0, "expected compilations, got: {stats:?}");
        assert!(stats.resident_entries > 0, "expected resident entries, got: {stats:?}");
    }

    #[test]
    fn jit_evm_add() {
        let backend = blocking_backend();

        // PUSH1 1 PUSH1 2 ADD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] =
            &[0x60, 0x01, 0x60, 0x02, 0x01, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        let mut evm = test_jit_evm(backend.clone());
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact(tx).unwrap();
        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = match &output {
                    Output::Call(bytes) => bytes,
                    other => panic!("expected Call output, got: {other:?}"),
                };
                assert_eq!(U256::from_be_slice(data), U256::from(3));
            }
            other => panic!("expected Success, got: {other:?}"),
        }

        let stats = backend.stats();
        assert!(stats.compilations_succeeded > 0, "expected compilations, got: {stats:?}");
        assert!(stats.resident_entries > 0, "expected resident entries, got: {stats:?}");
    }

    #[test]
    fn jit_evm_fallback_empty_code() {
        let backend = blocking_backend();
        let mut evm = test_jit_evm(backend);

        let tx = TxEnv {
            kind: TxKind::Call(Address::with_last_byte(0xEE)),
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact(tx).unwrap();
        assert!(
            matches!(result.result, ExecutionResult::Success { .. }),
            "expected Success for empty-code call, got: {:?}",
            result.result,
        );
    }

    #[test]
    fn jit_evm_amsterdam_state_gas_failure_matches_interpreter_gas() {
        // PUSH1 1 PUSH1 0xea SSTORE ADD
        //
        // In Amsterdam, the SSTORE charges EIP-8037 state gas. The following ADD
        // stack-underflows. Compiled failures may collapse to a single halt code,
        // but final transaction gas accounting still has to match the interpreter.
        let bytecode = &[op::PUSH1, 0x01, op::PUSH1, 0xea, op::SSTORE, op::ADD];

        let interpreter = call_contract(JitBackend::disabled(), SpecId::AMSTERDAM, bytecode);
        let jit = call_contract(blocking_backend(), SpecId::AMSTERDAM, bytecode);

        let (
            ExecutionResult::Halt { gas: jit_gas, .. },
            ExecutionResult::Halt { gas: interpreter_gas, .. },
        ) = (&jit, &interpreter)
        else {
            panic!("expected halt results: jit={jit:?}, interpreter={interpreter:?}");
        };
        assert_eq!(jit_gas, interpreter_gas);
    }

    /// Non-blocking mode: JIT compiles in background and results eventually appear.
    #[test]
    fn jit_evm_non_blocking() {
        use crate::runtime::RuntimeTuning;

        let config = RuntimeConfig {
            enabled: true,
            tuning: RuntimeTuning {
                jit_hot_threshold: 1,
                jit_worker_count: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let backend = JitBackend::new(config).unwrap();

        let runtime_code: &[u8] = &[0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        let mut evm = test_jit_evm(backend.clone());
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        // First call: should succeed (interpreter fallback or JIT).
        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).unwrap();
        assert!(matches!(result.result, ExecutionResult::Success { .. }));

        // Wait for JIT to compile in background.
        let code_hash = alloy_primitives::keccak256(runtime_code);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if backend.get_compiled(code_hash, SpecId::CANCUN).is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for JIT");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Second call: should use JIT-compiled function.
        let mut evm = test_jit_evm(backend);
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).unwrap();
        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = match &output {
                    Output::Call(bytes) => bytes,
                    other => panic!("expected Call output, got: {other:?}"),
                };
                assert_eq!(U256::from_be_slice(data), U256::from(0x42));
            }
            other => panic!("expected Success, got: {other:?}"),
        }
    }

    /// CALL into another contract: JIT handles the nested call frame.
    #[test]
    fn jit_evm_nested_call() {
        let backend = blocking_backend();
        let mut evm = test_jit_evm(backend);

        // Inner contract: PUSH1 0x42 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let inner_code: &[u8] = &[0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];
        let inner_addr = deploy_contract_with_nonce(&mut evm, inner_code, 0);

        // Outer contract: CALL inner, then copy return data and return it.
        let mut outer_code = vec![
            op::PUSH0, // retLen
            op::PUSH0, // retOff
            op::PUSH0, // argsLen
            op::PUSH0, // argsOff
            op::PUSH0, // value
        ];
        // PUSH20 <inner_addr>
        outer_code.push(op::PUSH20);
        outer_code.extend_from_slice(inner_addr.as_slice());
        outer_code.extend_from_slice(&[
            op::PUSH2,
            0xFF,
            0xFF, // gas
            op::CALL,
            op::RETURNDATASIZE,
            op::PUSH0,
            op::PUSH0,
            op::RETURNDATACOPY,
            op::RETURNDATASIZE,
            op::PUSH0,
            op::RETURN,
        ]);
        let outer_addr = deploy_contract_with_nonce(&mut evm, &outer_code, 1);

        let tx = TxEnv {
            kind: TxKind::Call(outer_addr),
            nonce: 2,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).unwrap();
        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = match &output {
                    Output::Call(bytes) => bytes,
                    other => panic!("expected Call output, got: {other:?}"),
                };
                assert_eq!(U256::from_be_slice(data), U256::from(0x42));
            }
            other => panic!("expected Success, got: {other:?}"),
        }
    }

    /// CREATE2 factory: deploy the same runtime code multiple times with different salts.
    /// All deployments share the same code hash and should hit the JIT cache.
    #[test]
    fn jit_evm_create2_factory() {
        let backend = blocking_backend();
        let mut evm = test_jit_evm(backend.clone());

        // Runtime code: PUSH1 0xAB PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] = &[0x60, 0xAB, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        // Build init code that deploys `runtime_code` (same pattern as deploy_contract_with_nonce).
        let rt_len = runtime_code.len();
        let init_offset = 10u8;
        let mut init_code = vec![
            op::PUSH1,
            rt_len as u8,
            op::PUSH1,
            init_offset,
            op::PUSH0,
            op::CODECOPY,
            op::PUSH1,
            rt_len as u8,
            op::PUSH0,
            op::RETURN,
        ];
        init_code.extend_from_slice(runtime_code);
        let init_len = init_code.len();

        // Factory contract: copies embedded init_code to memory, reads salt from
        // calldata, then CREATE2. Returns the deployed address.
        let mut factory_code = vec![
            op::PUSH1,
            0, // placeholder for init_len
            op::PUSH1,
            0, // placeholder for code_offset
            op::PUSH0,
            op::CODECOPY, // mem[0..init_len] = init_code
            op::PUSH0,
            op::CALLDATALOAD, // salt from calldata[0..32]
            op::PUSH1,
            0,         // placeholder for init_len
            op::PUSH0, // offset
            op::PUSH0, // value
            op::CREATE2,
            op::PUSH0,
            op::MSTORE,
            op::PUSH1,
            0x20,
            op::PUSH0,
            op::RETURN,
        ];
        let code_offset = factory_code.len() as u8;
        factory_code[1] = init_len as u8;
        factory_code[3] = code_offset;
        factory_code[9] = init_len as u8;
        factory_code.extend_from_slice(&init_code);

        let factory_addr = deploy_contract(&mut evm, &factory_code);

        // Deploy 3 instances via the factory with different salts.
        let mut deployed_addrs = Vec::new();
        for (i, salt) in (1u64..=3).enumerate() {
            let mut calldata = [0u8; 32];
            calldata[24..].copy_from_slice(&salt.to_be_bytes());

            let tx = TxEnv {
                kind: TxKind::Call(factory_addr),
                nonce: 1 + i as u64,
                data: Bytes::copy_from_slice(&calldata),
                gas_limit: 1_000_000,
                ..Default::default()
            };
            let result = evm.transact_commit(tx).unwrap();
            match result {
                ExecutionResult::Success { output, .. } => {
                    let data = match &output {
                        Output::Call(bytes) => bytes,
                        other => panic!("expected Call output, got: {other:?}"),
                    };
                    let addr = Address::from_slice(&data[12..32]);
                    assert_ne!(addr, Address::ZERO, "CREATE2 should return a non-zero address");
                    deployed_addrs.push(addr);
                }
                other => panic!("expected Success, got: {other:?}"),
            }
        }

        // All deployed addresses should be different (different salts).
        deployed_addrs.sort();
        deployed_addrs.dedup();
        assert_eq!(
            deployed_addrs.len(),
            3,
            "CREATE2 with different salts should yield different addresses"
        );

        // Call each deployed contract — all should return 0xAB.
        for (i, &addr) in deployed_addrs.iter().enumerate() {
            let tx = TxEnv {
                kind: TxKind::Call(addr),
                nonce: 4 + i as u64,
                gas_limit: 1_000_000,
                ..Default::default()
            };
            let result = evm.transact_commit(tx).unwrap();
            match result {
                ExecutionResult::Success { output, .. } => {
                    let data = match &output {
                        Output::Call(bytes) => bytes,
                        other => panic!("expected Call output, got: {other:?}"),
                    };
                    assert_eq!(U256::from_be_slice(data), U256::from(0xABu64));
                }
                other => panic!("expected Success, got: {other:?}"),
            }
        }

        let stats = backend.stats();
        assert!(stats.compilations_succeeded > 0, "expected compilations, got: {stats:?}");
    }

    /// CREATE deploys a contract whose runtime code gets JIT-compiled and called.
    #[test]
    fn jit_evm_create_and_call() {
        let backend = blocking_backend();
        let mut evm = test_jit_evm(backend);

        // Runtime code: PUSH1 0x99 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] = &[0x60, 0x99, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        // Deploy via helper (uses CREATE internally).
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        // Call the deployed contract.
        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).unwrap();
        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = match &output {
                    Output::Call(bytes) => bytes,
                    other => panic!("expected Call output, got: {other:?}"),
                };
                assert_eq!(U256::from_be_slice(data), U256::from(0x99u64));
            }
            other => panic!("expected Success, got: {other:?}"),
        }
    }
}
