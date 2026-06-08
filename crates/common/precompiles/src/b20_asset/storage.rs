//! EVM storage adapter for the asset B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, U256};
use base_precompile_macros::{AssetAccounting, Storable, TokenAccounting, contract};
use base_precompile_storage::{Handler, Mapping, Result, StorageCtx};

use crate::B20CoreStorage;

/// Asset-specific B-20 storage rooted at the `base.b20.asset` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.asset")]
pub struct B20AssetExtensionStorage {
    /// Custom decimal precision for this token; stored once at creation time.
    #[accessor]
    pub decimals: u8, // slot 0, offset 0
    /// Multiplier scaled to WAD.
    #[accessor]
    #[mutator]
    pub multiplier: U256, // slot 1
    /// Announcement IDs that have already been consumed.
    pub used_announcement_ids: Mapping<String, bool>, // slot 2
    /// Extra metadata values by metadata key.
    pub extra_metadata: Mapping<String, String>, // slot 3
}

/// EVM-backed storage for an asset B-20 token.
#[contract]
#[derive(TokenAccounting, AssetAccounting)]
pub struct B20AssetStorage {
    pub b20: B20CoreStorage,
    pub asset: B20AssetExtensionStorage,
}

/// Creation-time parameters for an asset B-20 token.
///
/// Passed to [`B20AssetStorage::initialize`] to write all fields atomically.
#[derive(Debug)]
pub struct B20AssetInit {
    /// ERC-20 token name.
    pub name: String,
    /// ERC-20 token symbol.
    pub symbol: String,
    /// Maximum total supply.
    pub supply_cap: U256,
    /// Multiplier at WAD precision.
    pub multiplier: U256,
    /// Custom decimal precision for this token; range is validated by the factory.
    pub decimals: u8,
}

impl<'a> B20AssetStorage<'a> {
    /// Creates a `B20AssetStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    pub fn initialize(&mut self, init: B20AssetInit) -> Result<()> {
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        self.asset.decimals.write(init.decimals)?;
        self.asset.multiplier.write(init.multiplier)?;
        Ok(())
    }
}

impl B20AssetStorage<'_> {
    /// Minimum allowed decimals for a B-20 asset token.
    pub const MIN_DECIMALS: u8 = 6;
    /// Maximum allowed decimals for a B-20 asset token.
    pub const MAX_DECIMALS: u8 = 18;
    /// WAD precision for multiplier arithmetic: 1e18.
    pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    /// Returns the configured asset decimals, defaulting an unset storage slot to
    /// [`Self::MIN_DECIMALS`].
    pub fn decimals(&self) -> Result<u8> {
        let decimals = self.asset.decimals()?;
        Ok(if decimals == 0 { Self::MIN_DECIMALS } else { decimals })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, StorageKey, setup_storage};

    use super::{
        __packing_b20_asset_extension_storage, B20AssetExtensionStorage, B20AssetInit,
        B20AssetStorage, slots,
    };
    use crate::{AssetAccounting, B20CoreStorage, B20TokenRole, TokenAccounting};

    const TOKEN: Address = address!("000000000000000000000000000000000000b021");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    const ASSET_ROOT: U256 =
        uint!(0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00_U256);

    #[test]
    fn wad_constant_is_ten_to_the_eighteenth() {
        assert_eq!(B20AssetStorage::WAD, U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn asset_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);
        assert_eq!(
            <B20AssetExtensionStorage as StorableType>::STORAGE_NAMESPACE_ID,
            "base.b20.asset"
        );
        assert_eq!(<B20AssetExtensionStorage as StorableType>::STORAGE_NAMESPACE_ROOT, ASSET_ROOT);

        assert_eq!(slots::B20, B20_ROOT);
        assert_eq!(slots::ASSET, ASSET_ROOT);
    }

    #[test]
    fn asset_extension_offsets_match_mock_storage() {
        assert_eq!(__packing_b20_asset_extension_storage::DECIMALS_LOC.offset_slots, 0);
        assert_eq!(__packing_b20_asset_extension_storage::DECIMALS_LOC.offset_bytes, 0);
        assert_eq!(__packing_b20_asset_extension_storage::MULTIPLIER_LOC.offset_slots, 1);
        assert_eq!(
            __packing_b20_asset_extension_storage::USED_ANNOUNCEMENT_IDS_LOC.offset_slots,
            2
        );
        assert_eq!(__packing_b20_asset_extension_storage::EXTRA_METADATA_LOC.offset_slots, 3);
    }

    #[test]
    fn multiplier_defaults_unset_slot_to_wad() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let token = B20AssetStorage::from_address(TOKEN, ctx);
            let multiplier_slot = ASSET_ROOT
                + U256::from(__packing_b20_asset_extension_storage::MULTIPLIER_LOC.offset_slots);

            assert_eq!(ctx.sload(TOKEN, multiplier_slot).unwrap(), U256::ZERO);
            assert_eq!(token.multiplier().unwrap(), B20AssetStorage::WAD);
        });
    }

    #[test]
    fn multiplier_preserves_configured_value() {
        let (mut storage, _) = setup_storage();
        let configured_multiplier = B20AssetStorage::WAD * U256::from(3u64);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.set_multiplier(configured_multiplier).unwrap();

            let multiplier_slot = ASSET_ROOT
                + U256::from(__packing_b20_asset_extension_storage::MULTIPLIER_LOC.offset_slots);

            assert_eq!(ctx.sload(TOKEN, multiplier_slot).unwrap(), configured_multiplier);
            assert_eq!(token.multiplier().unwrap(), configured_multiplier);
        });
    }

    #[test]
    fn role_admin_reads_raw_storage_default() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let token = B20AssetStorage::from_address(TOKEN, ctx);

            assert_eq!(
                TokenAccounting::role_admin(&token, B20TokenRole::Mint.id()).unwrap(),
                B20TokenRole::DefaultAdmin.id()
            );
            assert_eq!(
                TokenAccounting::role_admin(&token, B20TokenRole::DefaultAdmin.id()).unwrap(),
                B256::ZERO
            );
        });
    }

    #[test]
    fn string_mapping_slots_use_solidity_string_key_derivation() {
        let (mut storage, _) = setup_storage();
        let announcement_id = String::from("2026-Q1-split");
        let metadata_key = String::from("category");
        let metadata_value = String::from("fund");

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.asset.used_announcement_ids.at_mut(&announcement_id).write(true).unwrap();
            token.asset.extra_metadata.at_mut(&metadata_key).write(metadata_value.clone()).unwrap();

            let announcement_slot = ASSET_ROOT
                + U256::from(
                    __packing_b20_asset_extension_storage::USED_ANNOUNCEMENT_IDS_LOC.offset_slots,
                );
            let metadata_slot = ASSET_ROOT
                + U256::from(
                    __packing_b20_asset_extension_storage::EXTRA_METADATA_LOC.offset_slots,
                );

            assert_eq!(
                ctx.sload(TOKEN, announcement_id.mapping_slot(announcement_slot)).unwrap(),
                U256::ONE
            );
            assert_eq!(
                ctx.sload(TOKEN, metadata_key.mapping_slot(metadata_slot)).unwrap(),
                short_string_word(&metadata_value)
            );
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }

    fn make_init(decimals: u8) -> B20AssetInit {
        B20AssetInit {
            name: String::from("Test"),
            symbol: String::from("TST"),
            supply_cap: U256::from(1_000_000u64),
            multiplier: B20AssetStorage::WAD,
            decimals,
        }
    }

    #[test]
    fn decimals_stores_and_reads_back_lower_bound() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.initialize(make_init(B20AssetStorage::MIN_DECIMALS)).unwrap();
            assert_eq!(token.asset.decimals.read().unwrap(), B20AssetStorage::MIN_DECIMALS);
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MIN_DECIMALS);
        });
    }

    #[test]
    fn decimals_stores_and_reads_back_upper_bound() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.initialize(make_init(B20AssetStorage::MAX_DECIMALS)).unwrap();
            assert_eq!(token.asset.decimals.read().unwrap(), B20AssetStorage::MAX_DECIMALS);
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MAX_DECIMALS);
        });
    }

    #[test]
    fn decimals_uninitialized_slot_falls_back_to_min_decimals() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let token = B20AssetStorage::from_address(TOKEN, ctx);
            assert_eq!(token.asset.decimals.read().unwrap(), 0);
            assert_eq!(token.decimals().unwrap(), B20AssetStorage::MIN_DECIMALS);
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MIN_DECIMALS);
        });
    }
}
