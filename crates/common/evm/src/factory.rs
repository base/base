use alloy_evm::{Database, EvmEnv, EvmFactory, precompiles::PrecompilesMap};
use base_common_chains::BaseUpgrade;
use revm::{
    Context, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use crate::{
    BaseContext, BaseEvm, BaseHaltReason, BasePrecompiles, BaseSpecId, BaseTransaction,
    BaseTransactionError, Builder, DefaultBase,
    precompiles::counter::CounterPrecompile,
};

/// Factory that produces [`BaseEvm`] instances backed by a [`PrecompilesMap`].
///
/// [`BasePrecompiles`] are eagerly flattened into a [`PrecompilesMap`] on construction
/// so that precompile dispatch is a single hash-map lookup rather than a spec-aware
/// branch on every call.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BaseEvmFactory;

impl BaseEvmFactory {
    /// Builds a [`PrecompilesMap`] for the given spec, injecting fork-gated native precompiles.
    ///
    /// Simple (pure) precompiles live in the static [`BasePrecompiles`] table.
    /// Stateful native precompiles are inserted as [`DynPrecompile`] entries via
    /// [`PrecompilesMap::set_precompile_lookup`], keyed by their activation fork.
    ///
    /// To add a new native precompile gated at fork `F`:
    /// 1. Add its address/`DynPrecompile` inside `if spec.is_enabled_in(BaseUpgrade::F)`.
    /// 2. Add `pub mod <name>;` to `crates/common/evm/src/precompiles/mod.rs`.
    fn precompiles(input: &EvmEnv<BaseSpecId>) -> PrecompilesMap {
        let spec = input.cfg_env.spec;
        let mut precompiles =
            PrecompilesMap::from_static(BasePrecompiles::new_with_spec(spec).precompiles());

        if spec.is_enabled_in(BaseUpgrade::Beryl) {
            precompiles.set_precompile_lookup(|address: &alloy_primitives::Address| {
                if *address == crate::precompiles::counter::ADDRESS {
                    Some(CounterPrecompile::precompile())
                } else {
                    None
                }
            });
        }

        precompiles
    }
}

impl EvmFactory for BaseEvmFactory {
    type Evm<DB: Database, I: Inspector<BaseContext<DB>>> = BaseEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = BaseContext<DB>;
    type Tx = BaseTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, BaseTransactionError>;
    type HaltReason = BaseHaltReason;
    type Spec = BaseSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let precompiles = Self::precompiles(&input);
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_base()
            .with_inspector(NoOpInspector {})
            .with_precompiles(precompiles)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let precompiles = Self::precompiles(&input);
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector(inspector)
            .with_precompiles(precompiles)
    }
}
