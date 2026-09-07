//! [`alloy_evm`] bindings for the revmc runtime.
//!
//! Provides [`JitEvm`] and [`JitEvmFactory`] which wrap the standard Ethereum EVM with
//! JIT-compiled function dispatch via [`JitBackend`].

use alloy_evm::{
    Database,
    env::EvmEnv,
    eth::{EthEvmBuilder, EthEvmContext},
    evm::{Evm, EvmFactory},
    precompiles::PrecompilesMap,
};
use alloy_primitives::{Address, Bytes};
use revm_context::{BlockEnv, Evm as RevmEvm, TxEnv};
use revm_context_interface::{
    DBErrorMarker,
    result::{EVMError, HaltReason, ResultAndState},
};
use revm_handler::{
    EthFrame, ExecuteEvm, PrecompileProvider, SystemCallEvm, instructions::EthInstructions,
};
use revm_inspector::{InspectEvm, InspectSystemCallEvm, Inspector, NoOpInspector};
use revm_interpreter::{InterpreterResult, interpreter::EthInterpreter};
use revm_primitives::hardfork::SpecId;

use crate::{revm_evm, runtime::JitBackend};

type InnerEvm<DB, I, P> =
    RevmEvm<EthEvmContext<DB>, I, EthInstructions<EthInterpreter, EthEvmContext<DB>>, P, EthFrame>;

/// Ethereum EVM with JIT-compiled function dispatch.
///
/// Wraps the standard revm EVM inside a [`revm_evm::JitEvm`] which overrides `frame_run`
/// to look up compiled functions via [`JitBackend`] before falling back to the interpreter.
#[derive(derive_more::Debug)]
pub struct JitEvm<DB: Database, I, PRECOMPILE = PrecompilesMap> {
    #[debug(skip)]
    inner: revm_evm::JitEvm<InnerEvm<DB, I, PRECOMPILE>>,
    inspect: bool,
}

impl<DB, I, P> JitEvm<DB, I, P>
where
    DB: Database,
    P: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    /// Creates a new JIT EVM from an inner revm EVM, inspect flag, and backend.
    pub fn new(inner: InnerEvm<DB, I, P>, inspect: bool, backend: JitBackend) -> Self {
        Self { inner: revm_evm::JitEvm::new(inner, backend), inspect }
    }
}

impl<DB: Database, I, P> JitEvm<DB, I, P> {
    /// Returns a reference to the JIT backend.
    pub const fn backend(&self) -> &JitBackend {
        self.inner.backend()
    }
}

impl<DB: Database, I, P> core::ops::Deref for JitEvm<DB, I, P> {
    type Target = EthEvmContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner.ctx
    }
}

impl<DB: Database, I, P> core::ops::DerefMut for JitEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.ctx
    }
}

impl<DB, I, P> Evm for JitEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
    P: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.inner.ctx.block
    }

    fn cfg_env(&self) -> &revm_context::CfgEnv<Self::Spec> {
        &self.inner.cfg
    }

    fn chain_id(&self) -> u64 {
        self.inner.ctx.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_system_call_with_caller(caller, contract, data)
        } else {
            self.inner.system_call_with_caller(caller, contract, data)
        }
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let revm_context::Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.into_inner().ctx;
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let inner = self.inner.inner();
        (&inner.ctx.journaled_state.database, &inner.inspector, &inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let inner = self.inner.inner_mut();
        (&mut inner.ctx.journaled_state.database, &mut inner.inspector, &mut inner.precompiles)
    }
}

/// Factory producing [`JitEvm`] instances.
#[derive(Clone, Debug)]
pub struct JitEvmFactory {
    backend: JitBackend,
}

impl JitEvmFactory {
    /// Creates a new factory from a JIT backend.
    pub const fn new(backend: JitBackend) -> Self {
        Self { backend }
    }

    /// Returns a reference to the JIT backend.
    pub const fn backend(&self) -> &JitBackend {
        &self.backend
    }
}

impl EvmFactory for JitEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = JitEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let inner = EthEvmBuilder::new(db, input).build().into_inner();
        JitEvm::new(inner, false, self.backend.clone())
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let inner =
            EthEvmBuilder::new(db, input).activate_inspector(inspector).build().into_inner();
        JitEvm::new(inner, true, self.backend.clone())
    }
}

#[cfg(test)]
#[cfg(feature = "llvm")]
mod tests {
    use alloy_evm::{env::EvmEnv, eth::EthEvmFactory};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use revm_bytecode::opcode as op;
    use revm_context::TxEnv;
    use revm_context_interface::result::{ExecutionResult, Output};
    use revm_database::{CacheDB, EmptyDB};
    use revm_database_interface::DatabaseCommit;

    use super::*;
    use crate::runtime::{JitBackend, RuntimeConfig};

    fn blocking_backend() -> JitBackend {
        JitBackend::new(RuntimeConfig { blocking: true, ..Default::default() }).unwrap()
    }

    fn deploy_contract<
        E: alloy_evm::Evm<DB = CacheDB<EmptyDB>, Tx = TxEnv, Error: std::fmt::Debug>,
    >(
        evm: &mut E,
        bytecode: &[u8],
    ) -> Address
    where
        E::HaltReason: std::fmt::Debug,
    {
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
            data: Bytes::copy_from_slice(&deploy_code),
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact_raw(tx).unwrap();
        evm.db_mut().commit(result.state);
        match result.result {
            ExecutionResult::Success { output, .. } => match output {
                Output::Create(_, Some(addr)) => addr,
                other => panic!("expected Create output, got: {other:?}"),
            },
            other => panic!("expected Success, got: {other:?}"),
        }
    }

    #[test]
    fn jit_evm_simple_return() {
        let backend = blocking_backend();

        // PUSH1 0x42 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] = &[0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        let factory = JitEvmFactory::new(backend.clone());
        let mut evm = factory.create_evm(CacheDB::<EmptyDB>::default(), EvmEnv::default());
        let contract_addr = deploy_contract(&mut evm, runtime_code);

        let tx = TxEnv {
            kind: TxKind::Call(contract_addr),
            nonce: 1,
            gas_limit: 1_000_000,
            ..Default::default()
        };

        let result = evm.transact_raw(tx).unwrap();
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
    fn jit_evm_vs_eth_evm() {
        let backend = blocking_backend();

        // PUSH1 1 PUSH1 2 ADD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN
        let runtime_code: &[u8] =
            &[0x60, 0x01, 0x60, 0x02, 0x01, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

        // Run via JitEvmFactory.
        let jit_result = {
            let factory = JitEvmFactory::new(backend);
            let mut evm = factory.create_evm(CacheDB::<EmptyDB>::default(), EvmEnv::default());
            let addr = deploy_contract(&mut evm, runtime_code);
            evm.transact_raw(TxEnv {
                kind: TxKind::Call(addr),
                nonce: 1,
                gas_limit: 1_000_000,
                ..Default::default()
            })
            .unwrap()
        };

        // Run via standard EthEvmFactory.
        let eth_result = {
            let factory = EthEvmFactory::default();
            let mut evm = factory.create_evm(CacheDB::<EmptyDB>::default(), EvmEnv::default());
            let addr = deploy_contract(&mut evm, runtime_code);
            evm.transact_raw(TxEnv {
                kind: TxKind::Call(addr),
                nonce: 1,
                gas_limit: 1_000_000,
                ..Default::default()
            })
            .unwrap()
        };

        // Both should produce the same output.
        assert_eq!(format!("{:?}", jit_result.result), format!("{:?}", eth_result.result),);
    }

    #[test]
    fn jit_evm_factory_roundtrip() {
        let backend = blocking_backend();
        let factory = JitEvmFactory::new(backend);

        let mut evm = factory.create_evm(CacheDB::<EmptyDB>::default(), EvmEnv::default());
        assert_eq!(evm.chain_id(), 1);

        let tx = TxEnv {
            kind: TxKind::Call(Address::with_last_byte(0xDD)),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact_raw(tx).unwrap();
        assert!(
            matches!(result.result, ExecutionResult::Success { .. }),
            "expected Success, got: {:?}",
            result.result,
        );
    }
}
