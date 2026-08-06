//! EVM storage adapter for the asset B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, U256};
use base_precompile_macros::{AssetAccounting, Storable, TokenAccounting, contract};
use base_precompile_storage::{Handler, Mapping, Result, StorageCtx, StorageOps, insert_into_word};

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
    /// Pending scheduled multiplier target (ERC-8056). Introduced with `AssetV2`
    #[accessor]
    #[mutator]
    pub pending_multiplier: u128, // slot 4, offset 0
    /// Timestamp at which [`Self::pending_multiplier`] becomes effective
    #[accessor]
    #[mutator]
    pub pending_effective_at: u64, // slot 4, offset 16
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

    /// Writes the ERC-8056 pending schedule (`pending_multiplier` + `pending_effective_at`) in a
    /// single read-modify-write of the slot they share.
    ///
    /// The two `#[mutator]`-generated setters would each pay a full SLOAD/SSTORE on the same packed
    /// word; coalescing them halves that to one SLOAD + one SSTORE. `insert_into_word` rewrites only
    /// each field's own bytes, so the slot's unused upper 8 bytes are preserved untouched.
    fn write_pending(&mut self, multiplier: u128, effective_at: u64) -> Result<()> {
        // `pending_multiplier` (u128) occupies the low 16 bytes; `pending_effective_at` (u64) the
        // next 8. Both share one slot, so writing either field's handle addresses the same word.
        let slot = self.asset.pending_multiplier.slot();
        let current = StorageOps::load(&self.asset.pending_multiplier, slot)?;
        let word = insert_into_word(current, &multiplier, 0, size_of::<u128>())?;
        let word = insert_into_word(word, &effective_at, size_of::<u128>(), size_of::<u64>())?;
        StorageOps::store(&mut self.asset.pending_multiplier, slot, word)
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
    use crate::{
        AssetAccounting, B20CoreStorage, B20PolicyType, B20TokenRole, TokenAccounting,
        TransferPolicyIds,
    };

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
        // Pending (ERC-8056) packs `uint128 multiplier | uint64 effectiveAt` into slot 4
        assert_eq!(__packing_b20_asset_extension_storage::PENDING_MULTIPLIER_LOC.offset_slots, 4);
        assert_eq!(__packing_b20_asset_extension_storage::PENDING_MULTIPLIER_LOC.offset_bytes, 0);
        assert_eq!(__packing_b20_asset_extension_storage::PENDING_EFFECTIVE_AT_LOC.offset_slots, 4);
        assert_eq!(
            __packing_b20_asset_extension_storage::PENDING_EFFECTIVE_AT_LOC.offset_bytes,
            16
        );
    }

    #[test]
    fn pending_multiplier_and_effective_at_pack_into_slot_four() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            // 3e18 pending multiplier, effective one year out — distinct, non-zero lanes.
            let multiplier: u128 = 3_000_000_000_000_000_000;
            let effective_at: u64 = 1_800_000_000;
            token.asset.pending_multiplier.write(multiplier).unwrap();
            token.asset.pending_effective_at.write(effective_at).unwrap();

            let pending_slot = ASSET_ROOT
                + U256::from(
                    __packing_b20_asset_extension_storage::PENDING_MULTIPLIER_LOC.offset_slots,
                );
            // Solidity packs `uint128 multiplier | uint64 effectiveAt`: multiplier in the low 128
            // bits, effectiveAt in the next 64. Assert the raw word matches that packing exactly.
            let expected = U256::from(multiplier) | (U256::from(effective_at) << 128);
            assert_eq!(ctx.sload(TOKEN, pending_slot).unwrap(), expected);

            assert_eq!(token.asset.pending_multiplier.read().unwrap(), multiplier);
            assert_eq!(token.asset.pending_effective_at.read().unwrap(), effective_at);
        });
    }

    #[test]
    fn set_pending_writes_shared_slot_once_and_preserves_reserved_bits() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            let pending_slot = ASSET_ROOT
                + U256::from(
                    __packing_b20_asset_extension_storage::PENDING_MULTIPLIER_LOC.offset_slots,
                );

            // Seed the reserved upper 8 bytes (bytes 24..32) to prove the combined write leaves
            // them intact — this is what protects a future append-only field sharing the slot.
            let reserved = U256::from(0xDEAD_BEEFu64) << 192;
            ctx.sstore(TOKEN, pending_slot, reserved).unwrap();

            let multiplier: u128 = 3_000_000_000_000_000_000;
            let effective_at: u64 = 1_800_000_000;

            let before = ctx.counter_sstore();
            AssetAccounting::set_pending_and_effective_at(&mut token, multiplier, effective_at)
                .unwrap();
            assert_eq!(
                ctx.counter_sstore() - before,
                1,
                "set_pending_and_effective_at must write the shared slot exactly once"
            );

            // Both lanes land in the same word and the reserved bytes survive.
            let expected = reserved | U256::from(multiplier) | (U256::from(effective_at) << 128);
            assert_eq!(ctx.sload(TOKEN, pending_slot).unwrap(), expected);
            assert_eq!(token.asset.pending_multiplier.read().unwrap(), multiplier);
            assert_eq!(token.asset.pending_effective_at.read().unwrap(), effective_at);

            // The clear path is likewise a single write that only zeroes the two lanes.
            let before = ctx.counter_sstore();
            AssetAccounting::clear_pending_multiplier_and_effective_at(&mut token).unwrap();
            assert_eq!(
                ctx.counter_sstore() - before,
                1,
                "clear_pending_multiplier_and_effective_at must write the shared slot exactly once"
            );
            assert_eq!(ctx.sload(TOKEN, pending_slot).unwrap(), reserved);
        });
    }

    #[test]
    fn transfer_policy_ids_reads_shared_slot_once() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            // Distinct id per lane so a mis-extraction can't accidentally pass.
            TokenAccounting::set_policy_id(&mut token, B20PolicyType::TransferSender.id(), 11)
                .unwrap();
            TokenAccounting::set_policy_id(&mut token, B20PolicyType::TransferReceiver.id(), 22)
                .unwrap();
            TokenAccounting::set_policy_id(&mut token, B20PolicyType::TransferExecutor.id(), 33)
                .unwrap();

            let before = ctx.counter_sload();
            let ids = TokenAccounting::transfer_policy_ids(&token).unwrap();
            assert_eq!(
                ctx.counter_sload() - before,
                1,
                "all three transfer policy ids must be fetched in a single SLOAD"
            );
            assert_eq!(ids, TransferPolicyIds { sender: 11, receiver: 22, executor: 33 });
        });
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
