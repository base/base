//! [`Builder`] trait for constructing a [`BaseEvm`] directly from a [`BaseContext`].
use alloy_evm::{Database, precompiles::PrecompilesMap};
use alloy_primitives::Address;
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

    /// Returns the active [`BaseSpecId`] for this builder.
    fn spec(&self) -> BaseSpecId;

    /// Builds a [`BaseEvm`] with a `()` inspector. The inspect flag is `false`,
    /// so [`Inspector`][revm::Inspector] callbacks are never invoked via
    /// [`alloy_evm::Evm::transact`].
    fn build_base(self) -> BaseEvm<Self::Db, (), PrecompilesMap> {
        self.build_base_with_activation_admin_address(None)
    }

    /// Builds a [`BaseEvm`] with a `()` inspector and an activation registry admin address.
    ///
    /// The inspect flag is `false`, so [`Inspector`][revm::Inspector] callbacks are never invoked
    /// via [`alloy_evm::Evm::transact`].
    fn build_base_with_activation_admin_address(
        self,
        activation_admin_address: Option<Address>,
    ) -> BaseEvm<Self::Db, (), PrecompilesMap> {
        let spec = self.spec();
        let precompiles = BasePrecompiles::new_with_spec(spec)
            .with_activation_admin_address(activation_admin_address)
            .install();
        self.build_base_with_precompiles(precompiles)
    }

    /// Builds a [`BaseEvm`] with a `()` inspector and caller-supplied precompiles.
    ///
    /// The inspect flag is `false`, so [`Inspector`][revm::Inspector] callbacks are never invoked
    /// via [`alloy_evm::Evm::transact`].
    fn build_base_with_precompiles<P>(self, precompiles: P) -> BaseEvm<Self::Db, (), P>;

    /// Builds a [`BaseEvm`] with the given inspector. The inspect flag is `true`,
    /// so [`Inspector`][revm::Inspector] callbacks are invoked on every
    /// [`alloy_evm::Evm::transact`] call.
    fn build_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> BaseEvm<Self::Db, INSP, PrecompilesMap> {
        self.build_with_inspector_and_activation_admin_address(inspector, None)
    }

    /// Builds a [`BaseEvm`] with the given inspector and activation registry admin address.
    ///
    /// The inspect flag is `true`, so [`Inspector`][revm::Inspector] callbacks are invoked on every
    /// [`alloy_evm::Evm::transact`] call.
    fn build_with_inspector_and_activation_admin_address<INSP>(
        self,
        inspector: INSP,
        activation_admin_address: Option<Address>,
    ) -> BaseEvm<Self::Db, INSP, PrecompilesMap> {
        let spec = self.spec();
        let precompiles = BasePrecompiles::new_with_spec(spec)
            .with_activation_admin_address(activation_admin_address)
            .install();
        self.build_with_inspector_and_precompiles(inspector, precompiles)
    }

    /// Builds a [`BaseEvm`] with the given inspector and caller-supplied precompiles.
    ///
    /// The inspect flag is `true`, so [`Inspector`][revm::Inspector] callbacks are invoked on every
    /// [`alloy_evm::Evm::transact`] call.
    fn build_with_inspector_and_precompiles<INSP, P>(
        self,
        inspector: INSP,
        precompiles: P,
    ) -> BaseEvm<Self::Db, INSP, P>;
}

impl<DB: Database> Builder for BaseContext<DB> {
    type Db = DB;

    fn spec(&self) -> BaseSpecId {
        self.cfg.spec
    }

    fn build_base_with_precompiles<P>(self, precompiles: P) -> BaseEvm<DB, (), P> {
        let spec: BaseSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector: (),
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles,
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            false,
        )
    }

    fn build_with_inspector_and_precompiles<INSP, P>(
        self,
        inspector: INSP,
        precompiles: P,
    ) -> BaseEvm<DB, INSP, P> {
        let spec: BaseSpecId = self.cfg.spec;
        BaseEvm::new(
            revm::context::Evm {
                ctx: self,
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec.into()),
                precompiles,
                frame_stack: FrameStack::<EthFrame<EthInterpreter>>::new_prealloc(8),
            },
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use alloy_sol_types::SolCall;
    use base_common_precompiles::{
        ActivationRegistryStorage, B20FactoryStorage, B20Variant, IActivationRegistry,
        PolicyRegistryStorage,
    };
    use revm::{
        Context, ExecuteEvm,
        context::{CfgEnv, TxEnv},
        handler::EvmTr,
        inspector::NoOpInspector,
        primitives::{Bytes, TxKind},
    };

    use super::*;
    use crate::{BaseTransaction, BaseUpgrade, DefaultBase};

    fn b20_token_address() -> Address {
        B20Variant::Asset.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22)).0
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

    #[test]
    fn build_base_with_activation_admin_address_configures_activation_registry() {
        let admin = Address::repeat_byte(0xaa);
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)));
        let mut evm = ctx.build_base_with_activation_admin_address(Some(admin));

        let tx = BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .kind(TxKind::Call(ActivationRegistryStorage::ADDRESS))
                    .data(Bytes::from(IActivationRegistry::adminCall {}.abi_encode()))
                    .gas_limit(100_000),
            )
            .build_fill();

        let result = evm.transact_one(tx).unwrap();
        let output = result.output().unwrap();
        let actual = IActivationRegistry::adminCall::abi_decode_returns(output).unwrap();

        assert_eq!(actual, admin);
    }
}
