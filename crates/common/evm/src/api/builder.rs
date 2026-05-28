//! [`Builder`] trait for constructing a [`BaseEvm`] directly from a [`BaseContext`].
use alloy_evm::{Database, precompiles::PrecompilesMap};
use revm::{
    context::FrameStack,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
};

use crate::{BaseContext, BaseEvm, BasePrecompiles, BaseSpecId};

/// Trait that allows constructing a [`BaseEvm`] from a [`BaseContext`].
///
/// Implemented for [`BaseContext<DB>`] of any database. The resulting [`BaseEvm`]
/// installs the full [`BasePrecompiles`] map for the active [`BaseSpecId`]; call
/// [`BaseEvm::with_precompiles`] afterwards to substitute a custom precompile set.
pub trait Builder: Sized {
    /// The database type of the context.
    type Db: Database;

    /// Builds a [`BaseEvm`] with a `()` inspector. The inspect flag is `false`,
    /// so [`Inspector`][revm::Inspector] callbacks are never invoked via
    /// [`alloy_evm::Evm::transact`].
    fn build_base(self) -> BaseEvm<Self::Db, (), PrecompilesMap>;

    /// Builds a [`BaseEvm`] with the given inspector. The inspect flag is `true`,
    /// so [`Inspector`][revm::Inspector] callbacks are invoked on every
    /// [`alloy_evm::Evm::transact`] call.
    fn build_with_inspector<INSP>(self, inspector: INSP)
    -> BaseEvm<Self::Db, INSP, PrecompilesMap>;
}

impl<DB: Database> Builder for BaseContext<DB> {
    type Db = DB;

    fn build_base(self) -> BaseEvm<DB, (), PrecompilesMap> {
        let spec: BaseSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector: (),
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles: BasePrecompiles::new_with_spec(spec).install(),
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            false,
        )
    }

    fn build_with_inspector<INSP>(self, inspector: INSP) -> BaseEvm<DB, INSP, PrecompilesMap> {
        let spec: BaseSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles: BasePrecompiles::new_with_spec(spec).install(),
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use base_common_precompiles::{
        ActivationRegistryStorage, B20FactoryStorage, B20Variant, PolicyRegistryStorage,
    };
    use revm::{Context, context::CfgEnv, handler::EvmTr, inspector::NoOpInspector};

    use super::*;
    use crate::{BaseUpgrade, DefaultBase};

    fn b20_token_address() -> Address {
        B20Variant::B20.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22)).0
    }

    #[test]
    fn build_base_installs_dynamic_beryl_precompiles() {
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)));
        let evm = ctx.build_base();
        let (_, _, precompiles, _) = evm.all();

        assert!(precompiles.get(&B20FactoryStorage::ADDRESS).is_some());
        assert!(precompiles.get(&b20_token_address()).is_some());
        assert!(precompiles.get(&PolicyRegistryStorage::ADDRESS).is_some());
        assert!(precompiles.get(&ActivationRegistryStorage::ADDRESS).is_some());
    }

    #[test]
    fn build_base_does_not_install_beryl_precompiles_before_beryl() {
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Azul)));
        let evm = ctx.build_base();
        let (_, _, precompiles, _) = evm.all();

        assert!(precompiles.get(&B20FactoryStorage::ADDRESS).is_none());
        assert!(precompiles.get(&b20_token_address()).is_none());
        assert!(precompiles.get(&PolicyRegistryStorage::ADDRESS).is_none());
        assert!(precompiles.get(&ActivationRegistryStorage::ADDRESS).is_none());
    }

    #[test]
    fn build_with_inspector_installs_dynamic_beryl_precompiles() {
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)));
        let evm = ctx.build_with_inspector(NoOpInspector {});
        let (_, _, precompiles, _) = evm.all();

        assert!(precompiles.get(&B20FactoryStorage::ADDRESS).is_some());
        assert!(precompiles.get(&b20_token_address()).is_some());
    }
}
