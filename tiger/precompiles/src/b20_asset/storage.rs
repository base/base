//! EVM storage adapter for the asset B-20 variant.

use alloc::string::String;

use alloy_primitives::{U256, uint};
use base_tiger_precompile_storage::{Result, StorageLayout};

use crate::{AssetAccounting, B20CoreStorage, B20TokenKind, B20TokenStorage};

/// Asset-specific B-20 storage layout, rooted at the `base.b20.asset` ERC-7201 namespace.
#[derive(Debug)]
pub struct B20AssetExtensionStorage;

impl B20AssetExtensionStorage {
    /// ERC-7201 namespace id.
    pub const NAMESPACE: &'static str = "base.b20.asset";
    /// ERC-7201 namespace root.
    pub const ROOT: U256 =
        uint!(0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00_U256);
    /// `uint8 decimals`, alone in its slot; stored once at creation time.
    pub const DECIMALS: U256 = StorageLayout::field(Self::ROOT, 0);
    /// `uint256 multiplier`, scaled to WAD.
    pub const MULTIPLIER: U256 = StorageLayout::field(Self::ROOT, 1);
    /// `mapping(string id => bool) usedAnnouncementIds`.
    pub const USED_ANNOUNCEMENT_IDS: U256 = StorageLayout::field(Self::ROOT, 2);
    /// `mapping(string key => string value) extraMetadata`.
    pub const EXTRA_METADATA: U256 = StorageLayout::field(Self::ROOT, 3);
    /// ERC-8056 pending schedule, introduced with asset V2: `uint128` pending multiplier at byte
    /// offset 0 and `uint64` effective-at timestamp at byte offset 16; the top 8 bytes are unused.
    pub const PENDING: U256 = StorageLayout::field(Self::ROOT, 4);
}

impl B20TokenKind for B20AssetExtensionStorage {
    /// Returns the stored decimals, defaulting an unset slot to [`B20AssetStorage::MIN_DECIMALS`].
    fn decimals(token: &B20AssetStorage<'_>) -> Result<u8> {
        let decimals = token.storage.read(token.address, Self::DECIMALS)?;
        Ok(if decimals == 0 { B20AssetStorage::MIN_DECIMALS } else { decimals })
    }
}

/// EVM-backed storage for an asset B-20 token.
pub type B20AssetStorage<'a> = B20TokenStorage<'a, B20AssetExtensionStorage>;

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

impl B20AssetStorage<'_> {
    /// Minimum allowed decimals for a B-20 asset token.
    pub const MIN_DECIMALS: u8 = 6;
    /// Maximum allowed decimals for a B-20 asset token.
    pub const MAX_DECIMALS: u8 = 18;
    /// WAD precision for multiplier arithmetic: 1e18.
    pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    /// Writes all creation-time fields atomically.
    pub fn initialize(&mut self, init: B20AssetInit) -> Result<()> {
        self.storage.write_string(self.address, B20CoreStorage::NAME, &init.name)?;
        self.storage.write_string(self.address, B20CoreStorage::SYMBOL, &init.symbol)?;
        self.storage.write(self.address, B20CoreStorage::SUPPLY_CAP, init.supply_cap)?;
        self.storage.write(self.address, B20AssetExtensionStorage::DECIMALS, init.decimals)?;
        self.storage.write(self.address, B20AssetExtensionStorage::MULTIPLIER, init.multiplier)
    }

    /// Writes the ERC-8056 pending schedule (multiplier and effective-at) with one SLOAD and one
    /// SSTORE of the word they share, keeping its unused top 8 bytes.
    pub fn write_pending(&mut self, multiplier: u128, effective_at: u64) -> Result<()> {
        let slot = B20AssetExtensionStorage::PENDING;
        let word = self.storage.sload(self.address, slot)?;
        let word = StorageLayout::insert(word, 0, 16, U256::from(multiplier));
        let effective_at = U256::from(effective_at);
        let word = StorageLayout::insert(word, 16, StorageLayout::U64_BYTES, effective_at);
        self.storage.sstore(self.address, slot, word)
    }
}

impl AssetAccounting for B20AssetStorage<'_> {
    fn timestamp(&self) -> Result<U256> {
        Ok(self.storage.timestamp())
    }

    fn multiplier(&self) -> Result<U256> {
        let multiplier: U256 =
            self.storage.read(self.address, B20AssetExtensionStorage::MULTIPLIER)?;
        Ok(if multiplier.is_zero() { Self::WAD } else { multiplier })
    }

    fn set_multiplier(&mut self, multiplier: U256) -> Result<()> {
        self.storage.write(self.address, B20AssetExtensionStorage::MULTIPLIER, multiplier)
    }

    fn pending_multiplier(&self) -> Result<u128> {
        let slot = B20AssetExtensionStorage::PENDING;
        Ok(self.storage.read_packed(self.address, slot, 0, 16)?.to::<u128>())
    }

    fn pending_effective_at(&self) -> Result<u64> {
        let slot = B20AssetExtensionStorage::PENDING;
        let effective_at =
            self.storage.read_packed(self.address, slot, 16, StorageLayout::U64_BYTES)?;
        Ok(effective_at.to::<u64>())
    }

    fn set_pending_and_effective_at(&mut self, multiplier: u128, effective_at: u64) -> Result<()> {
        self.write_pending(multiplier, effective_at)
    }

    fn clear_pending_multiplier_and_effective_at(&mut self) -> Result<()> {
        self.write_pending(0, 0)
    }

    fn extra_metadata(&self, key: &str) -> Result<String> {
        let slot = StorageLayout::string_mapping(key, B20AssetExtensionStorage::EXTRA_METADATA);
        self.storage.read_string(self.address, slot)
    }

    fn set_extra_metadata_value(&mut self, key: &str, value: String) -> Result<()> {
        let slot = StorageLayout::string_mapping(key, B20AssetExtensionStorage::EXTRA_METADATA);
        if value.is_empty() {
            self.storage.delete_string(self.address, slot)
        } else {
            self.storage.write_string(self.address, slot, &value)
        }
    }

    fn is_announcement_id_used(&self, id: &str) -> Result<bool> {
        let slot =
            StorageLayout::string_mapping(id, B20AssetExtensionStorage::USED_ANNOUNCEMENT_IDS);
        self.storage.read(self.address, slot)
    }

    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()> {
        let slot =
            StorageLayout::string_mapping(id, B20AssetExtensionStorage::USED_ANNOUNCEMENT_IDS);
        self.storage.write(self.address, slot, true)
    }

    fn decimals(&self) -> Result<u8> {
        B20AssetExtensionStorage::decimals(self)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use alloy_primitives::{Address, B256, U256, address};
    use base_tiger_precompile_storage::{StorageCtx, StorageLayout, setup_storage};

    use super::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};
    use crate::{
        AssetAccounting, B20PolicyType, B20TokenKind, B20TokenRole, TokenAccounting,
        TransferPolicyIds,
    };

    const TOKEN: Address = address!("000000000000000000000000000000000000b021");

    #[test]
    fn wad_constant_is_ten_to_the_eighteenth() {
        assert_eq!(B20AssetStorage::WAD, U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn asset_namespace_matches_base_std_root() {
        assert_eq!(
            StorageLayout::erc7201_root(B20AssetExtensionStorage::NAMESPACE),
            B20AssetExtensionStorage::ROOT
        );
    }

    #[test]
    fn asset_extension_offsets_match_mock_storage() {
        let root = B20AssetExtensionStorage::ROOT;
        assert_eq!(B20AssetExtensionStorage::DECIMALS, root);
        assert_eq!(B20AssetExtensionStorage::MULTIPLIER, root + U256::from(1));
        assert_eq!(B20AssetExtensionStorage::USED_ANNOUNCEMENT_IDS, root + U256::from(2));
        assert_eq!(B20AssetExtensionStorage::EXTRA_METADATA, root + U256::from(3));
        assert_eq!(B20AssetExtensionStorage::PENDING, root + U256::from(4));
    }

    #[test]
    fn pending_multiplier_and_effective_at_pack_into_slot_four() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            // 3e18 pending multiplier, effective one year out — distinct, non-zero lanes.
            let multiplier: u128 = 3_000_000_000_000_000_000;
            let effective_at: u64 = 1_800_000_000;
            token.set_pending_and_effective_at(multiplier, effective_at).unwrap();

            // Solidity packs `uint128 multiplier | uint64 effectiveAt`: multiplier in the low 128
            // bits, effectiveAt in the next 64. Assert the raw word matches that packing exactly.
            let expected = U256::from(multiplier) | (U256::from(effective_at) << 128);
            assert_eq!(ctx.sload(TOKEN, B20AssetExtensionStorage::PENDING).unwrap(), expected);

            assert_eq!(token.pending_multiplier().unwrap(), multiplier);
            assert_eq!(token.pending_effective_at().unwrap(), effective_at);
        });
    }

    #[test]
    fn set_pending_writes_shared_slot_once_and_preserves_reserved_bits() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            let pending_slot = B20AssetExtensionStorage::PENDING;

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
            assert_eq!(token.pending_multiplier().unwrap(), multiplier);
            assert_eq!(token.pending_effective_at().unwrap(), effective_at);

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
            let multiplier_slot = B20AssetExtensionStorage::MULTIPLIER;

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

            let multiplier_slot = B20AssetExtensionStorage::MULTIPLIER;

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
            token.mark_announcement_id_used(&announcement_id).unwrap();
            token.set_extra_metadata_value(&metadata_key, metadata_value.clone()).unwrap();

            // Solidity: keccak256(bytes(key) ‖ slot) for string keys.
            let string_key_slot = |key: &str, slot: U256| {
                let preimage = [key.as_bytes(), &slot.to_be_bytes::<32>()].concat();
                U256::from_be_bytes(alloy_primitives::keccak256(preimage).0)
            };
            let announcement_slot = B20AssetExtensionStorage::USED_ANNOUNCEMENT_IDS;
            let metadata_slot = B20AssetExtensionStorage::EXTRA_METADATA;

            assert_eq!(
                ctx.sload(TOKEN, string_key_slot(&announcement_id, announcement_slot)).unwrap(),
                U256::ONE
            );
            assert_eq!(
                ctx.sload(TOKEN, string_key_slot(&metadata_key, metadata_slot)).unwrap(),
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

    fn read_raw_decimals(ctx: StorageCtx<'_>) -> u8 {
        ctx.read(TOKEN, B20AssetExtensionStorage::DECIMALS).unwrap()
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
            assert_eq!(read_raw_decimals(ctx), B20AssetStorage::MIN_DECIMALS);
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MIN_DECIMALS);
        });
    }

    #[test]
    fn decimals_stores_and_reads_back_upper_bound() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            token.initialize(make_init(B20AssetStorage::MAX_DECIMALS)).unwrap();
            assert_eq!(read_raw_decimals(ctx), B20AssetStorage::MAX_DECIMALS);
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MAX_DECIMALS);
        });
    }

    #[test]
    fn decimals_uninitialized_slot_falls_back_to_min_decimals() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let token = B20AssetStorage::from_address(TOKEN, ctx);
            assert_eq!(read_raw_decimals(ctx), 0);
            assert_eq!(
                B20AssetExtensionStorage::decimals(&token).unwrap(),
                B20AssetStorage::MIN_DECIMALS
            );
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MIN_DECIMALS);
        });
    }
}
