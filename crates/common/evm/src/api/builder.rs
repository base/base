//! [`Builder`] trait for constructing a [`BaseEvm`] directly from a [`BaseContext`].
use alloy_evm::Database;
use revm::{
    context::FrameStack,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
};

use crate::{BaseContext, BaseEvm, BasePrecompiles, OpSpecId};

/// Trait that allows constructing a [`BaseEvm`] from a [`BaseContext`].
///
/// Implemented for [`BaseContext<DB>`] of any database. The resulting [`BaseEvm`]
/// uses [`BasePrecompiles`] for the active [`OpSpecId`]; call
/// [`BaseEvm::with_precompiles`] afterwards to substitute a custom precompile set.
pub trait Builder: Sized {
    /// The database type of the context.
    type Db: Database;

    /// Builds a [`BaseEvm`] with a `()` inspector. The inspect flag is `false`,
    /// so [`Inspector`][revm::Inspector] callbacks are never invoked via
    /// [`alloy_evm::Evm::transact`].
    fn build_base(self) -> BaseEvm<Self::Db, ()>;

    /// Builds a [`BaseEvm`] with the given inspector. The inspect flag is `true`,
    /// so [`Inspector`][revm::Inspector] callbacks are invoked on every
    /// [`alloy_evm::Evm::transact`] call.
    fn build_with_inspector<INSP>(self, inspector: INSP) -> BaseEvm<Self::Db, INSP>;
}

impl<DB: Database> Builder for BaseContext<DB> {
    type Db = DB;

    fn build_base(self) -> BaseEvm<DB, ()> {
        let spec: OpSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector: (),
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles: BasePrecompiles::new_with_spec(spec),
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            false,
        )
    }

    fn build_with_inspector<INSP>(self, inspector: INSP) -> BaseEvm<DB, INSP> {
        let spec: OpSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles: BasePrecompiles::new_with_spec(spec),
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            true,
        )
    }
}
