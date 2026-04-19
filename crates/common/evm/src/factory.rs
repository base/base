use alloy_evm::{Database, EvmEnv, EvmFactory, precompiles::PrecompilesMap};
use revm::{
    Context, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use crate::{
    BaseEvm, BasePrecompiles, Builder, DefaultOp, OpContext, OpHaltReason, OpSpecId, OpTransaction,
    OpTransactionError,
};

/// Factory that produces [`BaseEvm`] instances backed by a [`PrecompilesMap`].
///
/// [`BasePrecompiles`] are eagerly flattened into a [`PrecompilesMap`] on construction
/// so that precompile dispatch is a single hash-map lookup rather than a spec-aware
/// branch on every call.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BaseEvmFactory;

impl EvmFactory for BaseEvmFactory {
    type Evm<DB: Database, I: Inspector<OpContext<DB>>> = BaseEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = OpContext<DB>;
    type Tx = OpTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        Context::op()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_op()
            .with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(
                BasePrecompiles::new_with_spec(spec_id).precompiles(),
            ))
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        Context::op()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector(inspector)
            .with_precompiles(PrecompilesMap::from_static(
                BasePrecompiles::new_with_spec(spec_id).precompiles(),
            ))
    }
}
