//! [`Builder`] trait for constructing a [`BaseEvm`] directly from a [`BaseContext`].
use alloy_evm::{Database, precompiles::PrecompilesMap};
use revm::{
    context::FrameStack,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
};

use crate::{BaseContext, BaseEvm, BasePrecompileInstaller, BaseSpecId};

/// Trait that allows constructing a [`BaseEvm`] from a [`BaseContext`].
///
/// Implemented for [`BaseContext<DB>`] of any database. The resulting [`BaseEvm`]
/// uses [`BasePrecompileInstaller`] for the active [`BaseSpecId`]; call
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
                precompiles: BasePrecompileInstaller::new(spec).install(),
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
                precompiles: BasePrecompileInstaller::new(spec).install(),
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use base_common_precompiles::DefaultToken;
    use revm::Context;

    use super::*;
    use crate::{BaseUpgrade, DefaultBase};

    #[test]
    fn build_base_installs_beryl_default_token() {
        let ctx = Context::base().modify_cfg_chained(|cfg| {
            cfg.spec = BaseSpecId::new(BaseUpgrade::Beryl);
        });
        let evm = ctx.build_base();

        assert!(evm.inner.precompiles.get(&DefaultToken::ADDRESS).is_some());
    }

    #[test]
    fn build_base_does_not_install_default_token_before_beryl() {
        let ctx = Context::base().modify_cfg_chained(|cfg| {
            cfg.spec = BaseSpecId::new(BaseUpgrade::Azul);
        });
        let evm = ctx.build_base();

        assert!(evm.inner.precompiles.get(&DefaultToken::ADDRESS).is_none());
    }
}
