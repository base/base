//! [`Builder`] trait for constructing a [`BaseEvm`] directly from a [`BaseContext`].
use alloy_evm::precompiles::PrecompilesMap;
use alloy_primitives::Address;
use revm::{
    Database,
    context::FrameStack,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
};

use crate::{BaseContext, BaseEvm, BasePrecompiles, BaseSpecId, BerylPrecompileMetricsObserver};

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

    /// Installs Base precompiles for node execution with the production Beryl metrics observer.
    fn precompiles_for_node(&self, activation_admin_address: Option<Address>) -> PrecompilesMap {
        BasePrecompiles::new_with_spec(self.spec())
            .with_activation_admin_address(activation_admin_address)
            .install_with_observer(BerylPrecompileMetricsObserver)
    }

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
        let precompiles = self.precompiles_for_node(activation_admin_address);
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
        let precompiles = self.precompiles_for_node(activation_admin_address);
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
    use core::convert::Infallible;

    use alloy_primitives::{Address, B256};
    use alloy_sol_types::SolCall;
    use base_common_precompiles::{
        ActivationFeature, ActivationRegistryStorage, B20FactoryStorage, B20Variant,
        IActivationRegistry, PolicyRegistryStorage,
    };
    use revm::{
        Context, DatabaseRef, ExecuteEvm,
        bytecode::Bytecode,
        context::{CfgEnv, TxEnv},
        handler::EvmTr,
        inspector::NoOpInspector,
        primitives::{Bytes, StorageKey, StorageValue, TxKind},
        state::AccountInfo,
    };

    use super::*;
    use crate::{BaseTransaction, BaseUpgrade, BerylPrecompileMetricsObserver, DefaultBase};

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
        BerylPrecompileMetricsObserver::reset_recorded_calls_for_test();

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
        assert!(BerylPrecompileMetricsObserver::recorded_calls_for_test() > 0);
    }

    #[test]
    fn cobalt_activation_admin_rotates_through_registry_state() {
        let admin = Address::repeat_byte(0xaa);
        let new_admin = Address::repeat_byte(0xbb);
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Cobalt)));
        let mut evm = ctx.build_base_with_activation_admin_address(Some(admin));

        let set_admin = activation_registry_tx(
            admin,
            0,
            Bytes::from(IActivationRegistry::setAdminCall { newAdmin: new_admin }.abi_encode()),
        );
        assert!(evm.transact_one(set_admin).unwrap().is_success());

        let admin_result = evm
            .transact_one(activation_registry_tx(
                new_admin,
                0,
                Bytes::from(IActivationRegistry::adminCall {}.abi_encode()),
            ))
            .unwrap();
        let actual_admin =
            IActivationRegistry::adminCall::abi_decode_returns(admin_result.output().unwrap())
                .unwrap();
        assert_eq!(actual_admin, new_admin);

        let feature = ActivationFeature::B20Asset.id();
        let old_admin_activate = activation_registry_tx(
            admin,
            1,
            Bytes::from(IActivationRegistry::activateCall { feature }.abi_encode()),
        );
        assert!(!evm.transact_one(old_admin_activate).unwrap().is_success());

        let new_admin_activate = activation_registry_tx(
            new_admin,
            1,
            Bytes::from(IActivationRegistry::activateCall { feature }.abi_encode()),
        );
        assert!(evm.transact_one(new_admin_activate).unwrap().is_success());
    }

    #[test]
    fn beryl_activation_admin_rejects_state_rotation() {
        let admin = Address::repeat_byte(0xaa);
        let new_admin = Address::repeat_byte(0xbb);
        let ctx =
            Context::base().with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)));
        let mut evm = ctx.build_base_with_activation_admin_address(Some(admin));

        let set_admin = activation_registry_tx(
            admin,
            0,
            Bytes::from(IActivationRegistry::setAdminCall { newAdmin: new_admin }.abi_encode()),
        );
        assert!(!evm.transact_one(set_admin).unwrap().is_success());

        let admin_result = evm
            .transact_one(activation_registry_tx(
                admin,
                1,
                Bytes::from(IActivationRegistry::adminCall {}.abi_encode()),
            ))
            .unwrap();
        let actual_admin =
            IActivationRegistry::adminCall::abi_decode_returns(admin_result.output().unwrap())
                .unwrap();
        assert_eq!(actual_admin, admin);
    }

    fn activation_registry_tx(caller: Address, nonce: u64, data: Bytes) -> BaseTransaction<TxEnv> {
        BaseTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(caller)
                    .nonce(nonce)
                    .kind(TxKind::Call(ActivationRegistryStorage::ADDRESS))
                    .data(data)
                    .gas_limit(500_000),
            )
            .build_fill()
    }

    struct ReadOnlyDbAdapter;

    impl DatabaseRef for ReadOnlyDbAdapter {
        type Error = Infallible;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::new())
        }

        fn storage_ref(
            &self,
            _address: Address,
            _index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            Ok(StorageValue::ZERO)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn build_base_accepts_read_only_database_adapters_without_debug() {
        let _ = BaseContext::base().with_ref_db(ReadOnlyDbAdapter).build_base();
    }
}
