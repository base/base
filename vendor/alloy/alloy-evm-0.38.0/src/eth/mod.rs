//! Ethereum EVM implementation.

pub use env::NextEvmEnvAttributes;

use crate::{env::EvmEnv, evm::EvmFactory, precompiles::PrecompilesMap, Database, Evm};
use alloy_primitives::{Address, Bytes};
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use revm::{
    context::{BlockEnv, CfgEnv, DBErrorMarker, Evm as RevmEvm, TxEnv},
    context_interface::result::{EVMError, HaltReason, ResultAndState},
    handler::{instructions::EthInstructions, EthFrame, EthPrecompiles, PrecompileProvider},
    inspector::NoOpInspector,
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    precompile::{PrecompileSpecId, Precompiles},
    primitives::hardfork::SpecId,
    Context, ExecuteEvm, InspectEvm, Inspector, MainBuilder, MainContext, SystemCallEvm,
};

mod block;
pub use block::*;

pub mod dao_fork;
pub mod eip6110;
pub mod receipt_builder;
pub mod spec;

mod env;
pub(crate) mod spec_id;

/// The Ethereum EVM context type.
pub type EthEvmContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB>;

/// Helper builder to construct `EthEvm` instances in a unified way.
#[derive(Debug)]
pub struct EthEvmBuilder<DB: Database, I = NoOpInspector> {
    db: DB,
    block_env: BlockEnv,
    cfg_env: CfgEnv,
    inspector: I,
    inspect: bool,
    precompiles: Option<PrecompilesMap>,
}

impl<DB: Database> EthEvmBuilder<DB, NoOpInspector> {
    /// Creates a builder from the provided `EvmEnv` and database.
    pub fn new(db: DB, env: EvmEnv) -> Self {
        Self {
            db,
            block_env: env.block_env,
            cfg_env: env.cfg_env,
            inspector: NoOpInspector {},
            inspect: false,
            precompiles: None,
        }
    }
}

impl<DB: Database, I> EthEvmBuilder<DB, I> {
    /// Sets a custom inspector
    pub fn inspector<J>(self, inspector: J) -> EthEvmBuilder<DB, J> {
        EthEvmBuilder {
            db: self.db,
            block_env: self.block_env,
            cfg_env: self.cfg_env,
            inspector,
            inspect: self.inspect,
            precompiles: self.precompiles,
        }
    }

    /// Sets a custom inspector and enables invoking it during transaction execution.
    pub fn activate_inspector<J>(self, inspector: J) -> EthEvmBuilder<DB, J> {
        self.inspector(inspector).inspect()
    }

    /// Sets whether to invoke the inspector during transaction execution.
    pub const fn set_inspect(mut self, inspect: bool) -> Self {
        self.inspect = inspect;
        self
    }

    /// Enables invoking the inspector during transaction execution.
    pub const fn inspect(self) -> Self {
        self.set_inspect(true)
    }

    /// Overrides the precompiles map. If not provided, it will be derived from the `SpecId` in
    /// `CfgEnv`.
    pub fn precompiles(mut self, precompiles: PrecompilesMap) -> Self {
        self.precompiles = Some(precompiles);
        self
    }

    /// Builds the `EthEvm` instance.
    pub fn build(self) -> EthEvm<DB, I, PrecompilesMap>
    where
        I: Inspector<EthEvmContext<DB>>,
    {
        let precompiles = match self.precompiles {
            Some(p) => p,
            None => PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(
                self.cfg_env.spec,
            ))),
        };

        let inner = Context::mainnet()
            .with_block(self.block_env)
            .with_cfg(self.cfg_env)
            .with_db(self.db)
            .build_mainnet_with_inspector(self.inspector)
            .with_precompiles(precompiles);

        EthEvm { inner, inspect: self.inspect }
    }
}

/// Ethereum EVM implementation.
///
/// This is a wrapper type around the `revm` ethereum evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`RevmEvm`] type.
#[expect(missing_debug_implementations)]
pub struct EthEvm<DB: Database, I, PRECOMPILE = EthPrecompiles> {
    inner: RevmEvm<
        EthEvmContext<DB>,
        I,
        EthInstructions<EthInterpreter, EthEvmContext<DB>>,
        PRECOMPILE,
        EthFrame,
    >,
    inspect: bool,
}

impl<DB: Database, I, PRECOMPILE> EthEvm<DB, I, PRECOMPILE> {
    /// Creates a new Ethereum EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`RevmEvm`] should be invoked on [`Evm::transact`].
    pub const fn new(
        evm: RevmEvm<
            EthEvmContext<DB>,
            I,
            EthInstructions<EthInterpreter, EthEvmContext<DB>>,
            PRECOMPILE,
            EthFrame,
        >,
        inspect: bool,
    ) -> Self {
        Self { inner: evm, inspect }
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(
        self,
    ) -> RevmEvm<
        EthEvmContext<DB>,
        I,
        EthInstructions<EthInterpreter, EthEvmContext<DB>>,
        PRECOMPILE,
        EthFrame,
    > {
        self.inner
    }

    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &EthEvmContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut EthEvmContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I, PRECOMPILE> Deref for EthEvm<DB, I, PRECOMPILE> {
    type Target = EthEvmContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, PRECOMPILE> DerefMut for EthEvm<DB, I, PRECOMPILE> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, PRECOMPILE> Evm for EthEvm<DB, I, PRECOMPILE>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
    PRECOMPILE: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PRECOMPILE;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.cfg
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.inspect_tx(tx)
        } else {
            self.inner.transact(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.ctx;

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

/// Factory producing [`EthEvm`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct EthEvmFactory;

impl EvmFactory for EthEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = EthEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = Context<BlockEnv, TxEnv, CfgEnv, DB>;
    type Tx = TxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        EthEvmBuilder::new(db, input).build()
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        EthEvmBuilder::new(db, input).activate_inspector(inspector).build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloy_primitives::address;
    use revm::{database_interface::EmptyDB, primitives::hardfork::SpecId};

    struct TestEvm<DB: Database, I> {
        inner: EthEvm<Box<DB>, I, PrecompilesMap>,
    }

    impl<DB, I> Evm for TestEvm<DB, I>
    where
        DB: Database,
        I: Inspector<EthEvmContext<Box<DB>>>,
    {
        type DB = DB;
        type Tx = TxEnv;
        type Error = EVMError<DB::Error>;
        type HaltReason = HaltReason;
        type Spec = SpecId;
        type BlockEnv = BlockEnv;
        type Precompiles = PrecompilesMap;
        type Inspector = I;

        fn block(&self) -> &Self::BlockEnv {
            self.inner.block()
        }

        fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
            self.inner.cfg_env()
        }

        fn chain_id(&self) -> u64 {
            self.inner.chain_id()
        }

        fn transact_raw(
            &mut self,
            tx: Self::Tx,
        ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
            self.inner.transact_raw(tx)
        }

        fn transact_system_call(
            &mut self,
            caller: Address,
            contract: Address,
            data: Bytes,
        ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
            self.inner.transact_system_call(caller, contract, data)
        }

        fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
            let (db, env) = self.inner.finish();
            (*db, env)
        }

        fn set_inspector_enabled(&mut self, enabled: bool) {
            self.inner.set_inspector_enabled(enabled);
        }

        fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
            let (db, inspector, precompiles) = self.inner.components();
            (db.as_ref(), inspector, precompiles)
        }

        fn components_mut(
            &mut self,
        ) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
            let (db, inspector, precompiles) = self.inner.components_mut();
            (db.as_mut(), inspector, precompiles)
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct TestFactory;

    impl EvmFactory for TestFactory {
        type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = TestEvm<DB, I>;
        type Context<DB: Database> = EthEvmContext<Box<DB>>;
        type Tx = TxEnv;
        type Error<DBError: DBErrorMarker> = EVMError<DBError>;
        type HaltReason = HaltReason;
        type Spec = SpecId;
        type BlockEnv = BlockEnv;
        type Precompiles = PrecompilesMap;

        fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
            TestEvm { inner: EthEvmBuilder::new(Box::new(db), input).build() }
        }

        fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
            &self,
            db: DB,
            input: EvmEnv,
            inspector: I,
        ) -> Self::Evm<DB, I> {
            TestEvm {
                inner: EthEvmBuilder::new(Box::new(db), input)
                    .activate_inspector(inspector)
                    .build(),
            }
        }
    }

    #[test]
    fn factory_context_can_adapt_database() {
        let input = EvmEnv::default();
        let mut evm = TestFactory.create_evm(EmptyDB::default(), input.clone());
        let _: &EmptyDB = evm.db();
        let _: &mut EmptyDB = evm.db_mut();

        let evm = TestFactory.create_evm_with_inspector(EmptyDB::default(), input, NoOpInspector);
        let (db, _) = evm.finish();
        let _: EmptyDB = db;
    }

    #[test]
    fn test_precompiles_with_correct_spec() {
        // create tests where precompile should be available for later specs but not earlier ones
        let specs_to_test = [
            // MODEXP (0x05) was added in Byzantium, should not exist in Frontier
            (
                address!("0x0000000000000000000000000000000000000005"),
                SpecId::FRONTIER,  // Early spec - should NOT have this precompile
                SpecId::BYZANTIUM, // Later spec - should have this precompile
                "MODEXP",
            ),
            // BLAKE2F (0x09) was added in Istanbul, should not exist in Byzantium
            (
                address!("0x0000000000000000000000000000000000000009"),
                SpecId::BYZANTIUM, // Early spec - should NOT have this precompile
                SpecId::ISTANBUL,  // Later spec - should have this precompile
                "BLAKE2F",
            ),
        ];

        for (precompile_addr, early_spec, later_spec, name) in specs_to_test {
            let mut early_cfg_env = CfgEnv::default();
            early_cfg_env.spec = early_spec;
            early_cfg_env.chain_id = 1;

            let early_env = EvmEnv { block_env: BlockEnv::default(), cfg_env: early_cfg_env };
            let factory = EthEvmFactory;
            let mut early_evm = factory.create_evm(EmptyDB::default(), early_env);

            // precompile should NOT be available in early spec
            assert!(
                early_evm.precompiles_mut().get(&precompile_addr).is_none(),
                "{name} precompile at {precompile_addr:?} should NOT be available for early spec {early_spec:?}"
            );

            let mut later_cfg_env = CfgEnv::default();
            later_cfg_env.spec = later_spec;
            later_cfg_env.chain_id = 1;

            let later_env = EvmEnv { block_env: BlockEnv::default(), cfg_env: later_cfg_env };
            let mut later_evm = factory.create_evm(EmptyDB::default(), later_env);

            // precompile should be available in later spec
            assert!(
                later_evm.precompiles_mut().get(&precompile_addr).is_some(),
                "{name} precompile at {precompile_addr:?} should be available for later spec {later_spec:?}"
            );
        }
    }
}
