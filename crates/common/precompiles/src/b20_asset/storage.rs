//! EVM storage adapter for the asset B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, B256, FixedBytes, U256, b256};
use base_precompile_macros::{AssetAccounting, Storable, TokenAccounting, contract};
use base_precompile_storage::{Handler, Mapping, Result, StorageCtx};

use crate::{B20CoreStorage, PolicyRegistryStorage};

/// Asset-specific B-20 storage rooted at the `base.b20.asset` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.asset")]
pub struct B20AssetExtensionStorage {
    /// Multiplier scaled to WAD.
    #[accessor]
    #[mutator]
    pub multiplier: U256, // offset 0
    /// Announcement IDs that have already been consumed.
    pub used_announcement_ids: Mapping<String, bool>, // offset 1
    /// Asset metadata values by identifier type.
    pub identifiers: Mapping<String, String>, // offset 2
}

/// Redemption-specific B-20 storage rooted at the `base.b20.redeem` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.redeem")]
pub struct B20RedeemStorage {
    /// Minimum scaled amount required for a redeem operation.
    #[accessor]
    #[mutator]
    pub minimum_redeemable: U256, // offset 0
    /// Redeem sender policy ID.
    #[accessor]
    #[mutator]
    pub redeem_sender_policy_id: u64, // slot 1, offset 0
    /// Reserved padding to fill the remainder of slot 1.
    pub redeem_reserved: FixedBytes<24>, // slot 1, offset 8 (fills remaining 24 bytes)
}

/// EVM-backed storage for an asset B-20 token.
#[contract]
#[derive(TokenAccounting, AssetAccounting)]
pub struct B20AssetStorage {
    pub b20: B20CoreStorage,
    pub asset: B20AssetExtensionStorage,
    pub redeem: B20RedeemStorage,
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
    /// Initial multiplier at WAD precision.
    pub multiplier: U256,
    /// ISIN identifier stored under the `"ISIN"` key.
    pub isin: String,
    /// Minimum redeemable amount; `0` allows any non-zero redemption.
    pub minimum_redeemable: U256,
}

impl<'a> B20AssetStorage<'a> {
    /// Policy scope identifier for the sender of a redeem operation:
    /// `keccak256("REDEEM_SENDER_POLICY")`.
    pub const REDEEM_SENDER_POLICY: B256 =
        b256!("0ff53b08b65363a609bb561211128f4044adc0e351f0b92b6aa23f8d85462f59");

    /// Creates a `B20AssetStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// `isin` may be empty; when non-empty it is stored under the `"ISIN"` key
    /// in the asset metadata mapping.
    ///
    /// `REDEEM_SENDER_POLICY` is initialised to `ALWAYS_BLOCK_ID` so redemption
    /// is closed by default; issuers must explicitly open it after creation.
    pub fn initialize(&mut self, init: B20AssetInit) -> Result<()> {
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        self.asset.multiplier.write(init.multiplier)?;
        self.redeem.minimum_redeemable.write(init.minimum_redeemable)?;
        if !init.isin.is_empty() {
            self.asset.identifiers.at_mut(&String::from("ISIN")).write(init.isin)?;
        }
        self.write_redeem_policy_ids_default()?;
        Ok(())
    }
}

impl B20AssetStorage<'_> {
    /// WAD precision for multiplier arithmetic: 1e18.
    pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    /// Writes the default `redeem_sender_policy_id` to `ALWAYS_BLOCK_ID`.
    /// Called once from [`initialize`].
    fn write_redeem_policy_ids_default(&mut self) -> Result<()> {
        self.redeem.set_redeem_sender_policy_id(PolicyRegistryStorage::ALWAYS_BLOCK_ID)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, StorageKey, setup_storage};

    use super::{
        __packing_b20_asset_extension_storage, __packing_b20_redeem_storage,
        B20AssetExtensionStorage, B20AssetInit, B20AssetStorage, B20RedeemStorage, slots,
    };
    use crate::{AssetAccounting, B20CoreStorage, PolicyRegistryStorage, TokenAccounting};

    const TOKEN: Address = address!("000000000000000000000000000000000000b021");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    const ASSET_ROOT: U256 =
        uint!(0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00_U256);
    const REDEEM_ROOT: U256 =
        uint!(0xc95c24ab0255f9fb9fcdcd524f71c4fe0437265856b7e5e6d0801df0e6cf5100_U256);

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
        assert_eq!(<B20RedeemStorage as StorableType>::STORAGE_NAMESPACE_ID, "base.b20.redeem");
        assert_eq!(<B20RedeemStorage as StorableType>::STORAGE_NAMESPACE_ROOT, REDEEM_ROOT);

        assert_eq!(slots::B20, B20_ROOT);
        assert_eq!(slots::ASSET, ASSET_ROOT);
        assert_eq!(slots::REDEEM, REDEEM_ROOT);
    }

    #[test]
    fn asset_extension_offsets_match_mock_storage() {
        assert_eq!(__packing_b20_asset_extension_storage::MULTIPLIER_LOC.offset_slots, 0);
        assert_eq!(
            __packing_b20_asset_extension_storage::USED_ANNOUNCEMENT_IDS_LOC.offset_slots,
            1
        );
        assert_eq!(__packing_b20_asset_extension_storage::IDENTIFIERS_LOC.offset_slots, 2);
        assert_eq!(__packing_b20_redeem_storage::MINIMUM_REDEEMABLE_LOC.offset_slots, 0);
        assert_eq!(__packing_b20_redeem_storage::REDEEM_SENDER_POLICY_ID_LOC.offset_slots, 1);
        assert_eq!(__packing_b20_redeem_storage::REDEEM_SENDER_POLICY_ID_LOC.offset_bytes, 0);
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
    fn asset_string_mapping_slots_use_solidity_string_key_derivation() {
        let (mut storage, _) = setup_storage();
        let announcement_id = String::from("2026-Q1-split");
        let identifier_type = String::from("ISIN");
        let identifier_value = String::from("US0000000000");

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.asset.used_announcement_ids.at_mut(&announcement_id).write(true).unwrap();
            token
                .asset
                .identifiers
                .at_mut(&identifier_type)
                .write(identifier_value.clone())
                .unwrap();
            token.redeem.minimum_redeemable.write(U256::from(10u64)).unwrap();

            let announcement_slot = ASSET_ROOT
                + U256::from(
                    __packing_b20_asset_extension_storage::USED_ANNOUNCEMENT_IDS_LOC.offset_slots,
                );
            let identifiers_slot = ASSET_ROOT
                + U256::from(__packing_b20_asset_extension_storage::IDENTIFIERS_LOC.offset_slots);
            let minimum_slot = REDEEM_ROOT
                + U256::from(__packing_b20_redeem_storage::MINIMUM_REDEEMABLE_LOC.offset_slots);

            assert_eq!(
                ctx.sload(TOKEN, announcement_id.mapping_slot(announcement_slot)).unwrap(),
                U256::ONE
            );
            assert_eq!(
                ctx.sload(TOKEN, identifier_type.mapping_slot(identifiers_slot)).unwrap(),
                short_string_word(&identifier_value)
            );
            assert_eq!(ctx.sload(TOKEN, minimum_slot).unwrap(), U256::from(10u64));
        });
    }

    #[test]
    fn redeem_sender_policy_uses_redeem_storage_lane() {
        let (mut storage, _) = setup_storage();
        let policy_id = 42u64;

        StorageCtx::enter(&mut storage, |ctx| {
            {
                let mut token = B20AssetStorage::from_address(TOKEN, ctx);
                token.set_policy_id(B20AssetStorage::REDEEM_SENDER_POLICY, policy_id).unwrap();
                assert_eq!(
                    token.policy_id(B20AssetStorage::REDEEM_SENDER_POLICY).unwrap(),
                    policy_id
                );
            }

            let redeem_policy_slot = REDEEM_ROOT
                + U256::from(
                    __packing_b20_redeem_storage::REDEEM_SENDER_POLICY_ID_LOC.offset_slots,
                );
            assert_eq!(ctx.sload(TOKEN, redeem_policy_slot).unwrap(), U256::from(policy_id));
        });
    }

    #[test]
    fn initialize_sets_redeem_sender_policy_to_always_block() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token
                .initialize(B20AssetInit {
                    name: String::from("Test"),
                    symbol: String::from("TST"),
                    supply_cap: U256::from(1_000_000u64),
                    multiplier: B20AssetStorage::WAD,
                    isin: String::new(),
                    minimum_redeemable: U256::ZERO,
                })
                .unwrap();

            assert_eq!(
                token.policy_id(B20AssetStorage::REDEEM_SENDER_POLICY).unwrap(),
                PolicyRegistryStorage::ALWAYS_BLOCK_ID,
                "REDEEM_SENDER_POLICY must default to ALWAYS_BLOCK_ID at creation"
            );
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }
}
