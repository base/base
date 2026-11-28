//! The Base native precompile set for one fork, and the single entry point that runs a call.
//!
//! This is the engine-neutral replacement for base-reth's `BasePrecompiles::install` and the
//! `base_precompile!` wrapper: [`NativePrecompiles::new`] decides which native precompiles a fork
//! installs, [`NativePrecompiles::lookup`] resolves a call target (fixed addresses first, then
//! B-20 token addresses, exactly as alloy-evm's `PrecompilesMap` consults its static entries
//! before the dynamic lookup), and [`NativePrecompiles::run`] executes the call against any
//! [`PrecompileStorageProvider`].

use alloc::vec::Vec;

use alloy_primitives::{Address, Bytes};
use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_tiger_precompile_storage::{
    BasePrecompileError, DelegateCallNotAllowed, PrecompileOutput, PrecompileResult,
    PrecompileStorageProvider, StorageCtx, StorageFeatures,
};

use crate::{
    ActivationAdminConfig, ActivationRegistryStorage, B20AssetStorage, B20Token,
    B20FactoryStorage, B20StablecoinStorage, B20Variant, NonceManagerStorage,
    PolicyRegistryStorage, PolicyVersions, TxContextStorage, UpgradeGatedStorageFeatures,
};

/// One Base native precompile, as resolved from a call target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePrecompile {
    /// Activation registry (Beryl).
    ActivationRegistry,
    /// B-20 token factory (Beryl).
    B20Factory,
    /// Policy registry (Beryl).
    PolicyRegistry,
    /// A B-20 asset token at a prefix-encoded address (Beryl).
    B20Asset,
    /// A B-20 stablecoin token at a prefix-encoded address (Beryl).
    B20Stablecoin,
    /// Transaction context (Cobalt).
    TxContext,
    /// Nonce manager (Cobalt).
    NonceManager,
}

/// The native precompiles a Base fork installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePrecompiles {
    upgrade: BaseUpgrade,
    admin_config: ActivationAdminConfig,
}

impl NativePrecompiles {
    /// Most fixed-address native precompiles any fork installs.
    pub const FIXED_COUNT_MAX: usize = 5;

    /// Returns the native set of `upgrade`. `activation_admin` is the chain's configured
    /// activation registry admin (base-reth's `activation_admin_address`), the fallback admin
    /// before the registry stores its own.
    pub const fn new(upgrade: BaseUpgrade, activation_admin: Option<Address>) -> Self {
        let admin_config = ActivationAdminConfig::new(
            activation_admin,
            upgrade.is_feature_enabled(BaseFeature::StatefulActivationAdmin),
        );
        Self { upgrade, admin_config }
    }

    /// Returns the fork this set was built for.
    pub const fn upgrade(&self) -> BaseUpgrade {
        self.upgrade
    }

    /// Returns the storage features native precompiles run with at this fork.
    pub const fn storage_features(&self) -> StorageFeatures {
        UpgradeGatedStorageFeatures::from_upgrade(self.upgrade)
    }

    /// Returns the fixed-address native precompiles installed at this fork with their
    /// addresses. These are the addresses base-reth pre-warms; B-20 token addresses are resolved
    /// dynamically and are not pre-warmed.
    pub fn fixed(&self) -> Vec<(Address, NativePrecompile)> {
        let fixed: Vec<_> = Self::FIXED
            .into_iter()
            .filter(|(feature, _, _)| self.upgrade.is_feature_enabled(*feature))
            .map(|(_, address, precompile)| (address, precompile))
            .collect();
        assert!(fixed.len() <= Self::FIXED_COUNT_MAX);
        fixed
    }

    /// The fixed-address native precompiles, each with the feature that installs it.
    const FIXED: [(BaseFeature, Address, NativePrecompile); 5] = [
        (
            BaseFeature::TokenFactoryPrecompile,
            B20FactoryStorage::ADDRESS,
            NativePrecompile::B20Factory,
        ),
        (
            BaseFeature::PolicyPrecompile,
            PolicyRegistryStorage::ADDRESS,
            NativePrecompile::PolicyRegistry,
        ),
        (
            BaseFeature::ActivationRegistry,
            ActivationRegistryStorage::ADDRESS,
            NativePrecompile::ActivationRegistry,
        ),
        (
            BaseFeature::TransactionContextPrecompile,
            TxContextStorage::ADDRESS,
            NativePrecompile::TxContext,
        ),
        (
            BaseFeature::NonceManagerPrecompile,
            NonceManagerStorage::ADDRESS,
            NativePrecompile::NonceManager,
        ),
    ];

    /// Resolves `address` to the native precompile installed there at this fork, if any.
    pub fn lookup(&self, address: Address) -> Option<NativePrecompile> {
        // Runs on every call, so without collecting `fixed()`.
        if let Some((_, _, precompile)) = Self::FIXED
            .iter()
            .find(|(feature, fixed, _)| *fixed == address && self.upgrade.is_feature_enabled(*feature))
        {
            return Some(*precompile);
        }
        if !self.upgrade.is_feature_enabled(BaseFeature::TokenPrecompiles) {
            return None;
        }
        match B20Variant::from_address(address)? {
            B20Variant::Asset => Some(NativePrecompile::B20Asset),
            B20Variant::Stablecoin => Some(NativePrecompile::B20Stablecoin),
        }
    }

    /// Runs one call to `precompile` at `address`.
    ///
    /// `is_direct_call` is false for `DELEGATECALL` and `CALLCODE` (the executing account is not
    /// the precompile), which every native precompile rejects with a zero-gas
    /// `DelegateCallNotAllowed()` revert. Gas, logs, and state changes go through `provider`;
    /// the returned output reports the gas used and refund the caller applies.
    pub fn run<P: PrecompileStorageProvider>(
        &self,
        precompile: NativePrecompile,
        address: Address,
        calldata: &[u8],
        is_direct_call: bool,
        provider: &mut P,
    ) -> PrecompileResult {
        debug_assert_eq!(self.lookup(address), Some(precompile));
        if !is_direct_call {
            return BasePrecompileError::revert(DelegateCallNotAllowed {})
                .into_precompile_result(0, 0);
        }
        assert_eq!(provider.storage_features(), self.storage_features());
        let upgrade = self.upgrade;
        StorageCtx::enter(provider, |ctx| match precompile {
            NativePrecompile::ActivationRegistry => ActivationRegistryStorage::new(ctx).dispatch(
                ctx,
                calldata,
                self.admin_config,
                upgrade,
            ),
            NativePrecompile::B20Factory => {
                B20FactoryStorage::new(ctx).dispatch(ctx, calldata, upgrade)
            }
            NativePrecompile::PolicyRegistry => {
                PolicyRegistryStorage::new(ctx).dispatch(ctx, calldata, upgrade)
            }
            NativePrecompile::B20Asset => {
                let Some(version) = PolicyVersions::from_base_upgrade(upgrade) else {
                    return Ok(PrecompileOutput::revert(0, Bytes::new(), 0));
                };
                B20Token::with_storage_and_policy(
                    B20AssetStorage::from_address(address, ctx),
                    PolicyRegistryStorage::new(ctx),
                    version,
                )
                .dispatch_asset(ctx, calldata, upgrade)
            }
            NativePrecompile::B20Stablecoin => {
                let Some(version) = PolicyVersions::from_base_upgrade(upgrade) else {
                    return Ok(PrecompileOutput::revert(0, Bytes::new(), 0));
                };
                B20Token::with_storage_and_policy(
                    B20StablecoinStorage::from_address(address, ctx),
                    PolicyRegistryStorage::new(ctx),
                    version,
                )
                .dispatch_stablecoin(ctx, calldata, upgrade)
            }
            NativePrecompile::TxContext => TxContextStorage::new(ctx).dispatch(ctx, calldata),
            NativePrecompile::NonceManager => NonceManagerStorage::new(ctx).dispatch(ctx, calldata),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    fn token() -> Address {
        B20Variant::Asset.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22)).0
    }

    #[test]
    fn nothing_is_installed_before_beryl() {
        let set = NativePrecompiles::new(BaseUpgrade::Azul, None);
        assert!(set.fixed().is_empty());
        assert_eq!(set.lookup(token()), None);
        assert_eq!(set.lookup(B20FactoryStorage::ADDRESS), None);
    }

    #[test]
    fn beryl_installs_registries_factory_and_tokens() {
        let set = NativePrecompiles::new(BaseUpgrade::Beryl, None);
        assert_eq!(set.lookup(B20FactoryStorage::ADDRESS), Some(NativePrecompile::B20Factory));
        assert_eq!(
            set.lookup(PolicyRegistryStorage::ADDRESS),
            Some(NativePrecompile::PolicyRegistry)
        );
        assert_eq!(
            set.lookup(ActivationRegistryStorage::ADDRESS),
            Some(NativePrecompile::ActivationRegistry)
        );
        assert_eq!(set.lookup(token()), Some(NativePrecompile::B20Asset));
        assert_eq!(set.lookup(TxContextStorage::ADDRESS), None);
        assert_eq!(set.lookup(NonceManagerStorage::ADDRESS), None);
        assert_eq!(set.lookup(Address::repeat_byte(0x42)), None);
        assert_eq!(set.storage_features(), StorageFeatures::Legacy);
    }

    #[test]
    fn cobalt_adds_tx_context_and_nonce_manager() {
        let set = NativePrecompiles::new(BaseUpgrade::Cobalt, None);
        assert_eq!(set.lookup(TxContextStorage::ADDRESS), Some(NativePrecompile::TxContext));
        assert_eq!(set.lookup(NonceManagerStorage::ADDRESS), Some(NativePrecompile::NonceManager));
        assert_eq!(set.fixed().len(), NativePrecompiles::FIXED_COUNT_MAX);
        assert_eq!(set.storage_features(), StorageFeatures::Cobalt);
    }
}
