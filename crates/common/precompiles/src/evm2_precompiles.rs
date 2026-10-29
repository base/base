//! evm2 dispatch for Base native precompiles.

use alloc::{borrow::Cow, vec::Vec};

use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{
    Evm2PrecompileStorageProvider, Evm2TxOrigin, IntoEvm2PrecompileResult, StorageCtx,
};
use evm2::{
    Evm, EvmTypes, PrecompileError, Precompiles, SpecId, Version,
    evm::{
        PrewarmSet,
        precompile::{PrecompileOutput, PrecompileProvider},
    },
    interpreter::{GasTracker, Message},
    precompiles::{MovePrecompileError, PrecompileId},
};

use crate::{
    ActivationAdminConfig, ActivationRegistry, ActivationRegistryStorage, B20AssetPrecompile,
    B20Factory, B20FactoryStorage, B20StablecoinPrecompile, B20Variant, NonceManager,
    NonceManagerStorage, NoopPrecompileCallObserver, PolicyRegistryPrecompile,
    PolicyRegistryStorage, PrecompileCallObserver, TxContext, TxContextStorage,
    UpgradeGatedStorageFeatures,
};

/// A Base native precompile, identified by the address it is installed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseNativePrecompile {
    /// The `B20Factory` precompile.
    B20Factory,
    /// The `PolicyRegistry` precompile.
    PolicyRegistry,
    /// The activation registry precompile.
    ActivationRegistry,
    /// The EIP-8130 transaction context precompile.
    TxContext,
    /// The EIP-8130 2D nonce manager precompile.
    NonceManager,
    /// A B-20 token of the given variant, resolved from its address.
    B20Token(B20Variant),
}

impl BaseNativePrecompile {
    /// Fixed-address native precompiles installed from Beryl.
    const BERYL: [(Address, Self); 3] = [
        (B20FactoryStorage::ADDRESS, Self::B20Factory),
        (PolicyRegistryStorage::ADDRESS, Self::PolicyRegistry),
        (ActivationRegistryStorage::ADDRESS, Self::ActivationRegistry),
    ];

    /// Fixed-address native precompiles added at Cobalt.
    const COBALT: [(Address, Self); 2] = [
        (TxContextStorage::ADDRESS, Self::TxContext),
        (NonceManagerStorage::ADDRESS, Self::NonceManager),
    ];

    /// Returns the fixed-address native precompiles installed at `upgrade`.
    pub fn fixed_at(upgrade: BaseUpgrade) -> impl Iterator<Item = (Address, Self)> {
        let beryl: &[_] = if upgrade >= BaseUpgrade::Beryl { &Self::BERYL } else { &[] };
        let cobalt: &[_] = if upgrade >= BaseUpgrade::Cobalt { &Self::COBALT } else { &[] };
        beryl.iter().chain(cobalt).copied()
    }

    /// Returns the precompile identifier reported for a fixed-address native precompile.
    pub const fn id(self) -> &'static str {
        match self {
            Self::B20Factory => B20Factory::ID,
            Self::PolicyRegistry => PolicyRegistryPrecompile::ID,
            Self::ActivationRegistry => ActivationRegistry::ID,
            Self::TxContext => TxContext::ID,
            Self::NonceManager => NonceManager::ID,
            Self::B20Token(B20Variant::Stablecoin) => "B20StablecoinToken",
            Self::B20Token(B20Variant::Asset) => "B20AssetToken",
        }
    }
}

/// evm2 [`PrecompileProvider`] serving Base native precompiles on top of a static precompile set.
///
/// Installs the same precompiles at the same addresses as the revm
/// `BasePrecompiles::install_with_observer`:
///
/// - From **Beryl**: `B20Factory`, `PolicyRegistry`, the activation registry, and every B-20 token
///   address (resolved on lookup, like the revm dynamic lookup, so token addresses are neither
///   listed nor pre-warmed).
/// - From **Cobalt**: `TxContext` and `NonceManager`; the activation registry consults its admin
///   storage slot.
///
/// Every other address is served by the wrapped static set. Native precompiles price storage with
/// the Osaka gas schedule, matching the revm dispatch, and reject `DELEGATECALL`/`CALLCODE`.
/// They cannot be relocated with [`PrecompileProvider::move_precompiles`].
#[derive(Debug, Clone)]
pub struct BaseEvm2Precompiles<T: EvmTypes, O = NoopPrecompileCallObserver> {
    inner: Precompiles<T>,
    upgrade: BaseUpgrade,
    activation_admin_address: Option<Address>,
    observer: O,
}

impl<T: EvmTypes> BaseEvm2Precompiles<T> {
    /// Serves Base native precompiles active at `upgrade` on top of the static set `inner`.
    pub const fn new(inner: Precompiles<T>, upgrade: BaseUpgrade) -> Self {
        Self {
            inner,
            upgrade,
            activation_admin_address: None,
            observer: NoopPrecompileCallObserver,
        }
    }
}

impl<T: EvmTypes, O> BaseEvm2Precompiles<T, O> {
    /// The gas schedule native precompiles price storage with, independent of the active spec.
    ///
    /// The revm dispatch prices with revm's default `GasParams`, which is the Osaka schedule.
    pub const NATIVE_GAS_SPEC: SpecId = SpecId::OSAKA;

    /// Replaces the observer notified of native precompile calls.
    pub fn with_observer<P: PrecompileCallObserver>(
        self,
        observer: P,
    ) -> BaseEvm2Precompiles<T, P> {
        BaseEvm2Precompiles {
            inner: self.inner,
            upgrade: self.upgrade,
            activation_admin_address: self.activation_admin_address,
            observer,
        }
    }

    /// Sets the static activation registry admin address.
    #[must_use = "with_activation_admin_address returns a new BaseEvm2Precompiles value"]
    pub const fn with_activation_admin_address(
        mut self,
        activation_admin_address: Option<Address>,
    ) -> Self {
        self.activation_admin_address = activation_admin_address;
        self
    }

    /// Returns the native precompile served at `address`, if any.
    ///
    /// Fixed-address native precompiles take precedence over the static set, which takes
    /// precedence over B-20 token addresses.
    pub fn native(&self, address: &Address) -> Option<BaseNativePrecompile> {
        if let Some((_, native)) =
            BaseNativePrecompile::fixed_at(self.upgrade).find(|(fixed, _)| fixed == address)
        {
            return Some(native);
        }
        if self.upgrade < BaseUpgrade::Beryl || self.inner.as_map().contains(address) {
            return None;
        }
        B20Variant::from_address(*address).map(BaseNativePrecompile::B20Token)
    }
}

impl<T: EvmTypes, O: PrecompileCallObserver> BaseEvm2Precompiles<T, O> {
    fn run_native(
        &self,
        native: BaseNativePrecompile,
        evm: &mut Evm<'_, T>,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> Result<PrecompileOutput, PrecompileError>
    where
        T::EvmExt: Evm2TxOrigin,
    {
        let upgrade = self.upgrade;
        let observer = self.observer.clone();
        let calldata = message.input.as_ref();
        let mut provider = Evm2PrecompileStorageProvider::new(
            evm,
            message,
            Version::base(Self::NATIVE_GAS_SPEC).gas_params,
            UpgradeGatedStorageFeatures::from_upgrade(upgrade),
        );
        StorageCtx::enter_direct_call(&mut provider, message.is_direct_call(), |ctx| match native {
            BaseNativePrecompile::B20Factory => B20Factory::run(ctx, calldata, upgrade, observer),
            BaseNativePrecompile::PolicyRegistry => {
                PolicyRegistryPrecompile::run(ctx, calldata, upgrade, observer)
            }
            BaseNativePrecompile::ActivationRegistry => ActivationRegistry::run(
                ctx,
                calldata,
                ActivationAdminConfig::new(
                    self.activation_admin_address,
                    upgrade >= BaseUpgrade::Cobalt,
                ),
                upgrade,
                observer,
            ),
            BaseNativePrecompile::TxContext => TxContext::run(ctx, calldata),
            BaseNativePrecompile::NonceManager => NonceManager::run(ctx, calldata),
            BaseNativePrecompile::B20Token(B20Variant::Stablecoin) => {
                B20StablecoinPrecompile::run(ctx, calldata, message.call_target, upgrade, observer)
            }
            BaseNativePrecompile::B20Token(B20Variant::Asset) => {
                B20AssetPrecompile::run(ctx, calldata, message.call_target, upgrade, observer)
            }
        })
        .into_evm2(gas)
    }
}

impl<T, O> PrecompileProvider<T> for BaseEvm2Precompiles<T, O>
where
    T: EvmTypes,
    T::EvmExt: Evm2TxOrigin,
    O: PrecompileCallObserver,
{
    fn addresses(&self) -> Vec<Address> {
        let mut addresses = self.inner.addresses();
        addresses.extend(BaseNativePrecompile::fixed_at(self.upgrade).map(|(address, _)| address));
        addresses
    }

    fn warm_addresses(&self, warm: &mut PrewarmSet) {
        self.inner.warm_addresses(warm);
        for (address, _) in BaseNativePrecompile::fixed_at(self.upgrade) {
            warm.warm(&address);
        }
    }

    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        let mut ids = self.inner.precompile_ids();
        ids.extend(
            BaseNativePrecompile::fixed_at(self.upgrade).map(|(address, native)| {
                (address, PrecompileId::Custom(Cow::Borrowed(native.id())))
            }),
        );
        ids
    }

    fn contains(&self, address: &Address) -> bool {
        self.native(address).is_some() || self.inner.as_map().contains(address)
    }

    fn move_precompiles(
        &mut self,
        moves: &[(Address, Address)],
    ) -> Result<(), MovePrecompileError> {
        self.inner.move_precompiles(moves)
    }

    fn execute(
        &mut self,
        evm: &mut Evm<'_, T>,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        match self.native(&message.call_target) {
            Some(native) => Some(self.run_native(native, evm, message, gas)),
            None => self.inner.execute(evm, message, gas),
        }
    }
}
