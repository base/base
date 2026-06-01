//! `B20TokenStorage` stores the EVM storage layout for B-20 tokens.

use alloc::string::String;

use alloy_primitives::{Address, B256, FixedBytes, U256};
use base_precompile_macros::{Storable, TokenAccounting, contract};
use base_precompile_storage::{Handler, Mapping, Result, StorageCtx};

/// Creation-time parameters for a B-20 token.
///
/// Passed to [`B20TokenStorage::initialize`] to write all fields atomically.
#[derive(Debug)]
pub struct B20TokenInit {
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Maximum total supply allowed.
    pub supply_cap: U256,
}

/// Core B-20 storage rooted at the `base.b20` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20")]
pub struct B20CoreStorage {
    /// Mutable token name.
    #[accessor]
    #[mutator]
    pub name: String, // offset 0
    /// Mutable token symbol.
    #[accessor]
    #[mutator]
    pub symbol: String, // offset 1
    /// ERC-7572 contract metadata URI.
    #[accessor]
    #[mutator]
    pub contract_uri: String, // offset 2
    /// Total token supply.
    #[accessor]
    #[mutator]
    pub total_supply: U256, // offset 3
    /// Token balances by account.
    #[accessor(name = balance_of, keys(account))]
    #[mutator(name = set_balance, keys(account), value = balance)]
    pub balances: Mapping<Address, U256>, // offset 4
    /// Spending allowances by owner and spender.
    #[accessor(name = allowance, keys(owner, spender))]
    #[mutator(name = set_allowance, keys(owner, spender), value = amount)]
    pub allowances: Mapping<Address, Mapping<Address, U256>>, // offset 5
    /// Role membership flags by role and account.
    #[accessor(name = has_role, keys(role, account))]
    #[mutator(name = set_role, keys(role, account), value = enabled)]
    pub roles: Mapping<B256, Mapping<Address, bool>>, // offset 6
    /// Admin role configured for each role.
    #[accessor(name = role_admin, keys(role))]
    #[mutator(name = set_role_admin, keys(role), value = admin_role)]
    pub role_admins: Mapping<B256, B256>, // offset 7
    /// Default-admin holder count.
    #[accessor]
    #[mutator]
    pub admin_count: U256, // offset 8
    /// Transfer sender policy ID.
    #[accessor]
    #[mutator]
    pub transfer_sender_policy_id: u64, // slot 9, offset 0
    /// Transfer receiver policy ID.
    #[accessor]
    #[mutator]
    pub transfer_receiver_policy_id: u64, // slot 9, offset 8
    /// Transfer executor policy ID.
    #[accessor]
    #[mutator]
    pub transfer_executor_policy_id: u64, // slot 9, offset 16
    /// Reserved padding to close slot 9.
    pub transfer_reserved_0: u64, // slot 9, offset 24 (filler to close the slot)
    /// Mint receiver policy ID.
    #[accessor]
    #[mutator]
    pub mint_receiver_policy_id: u64, // slot 10, offset 0
    /// Reserved padding to fill the remainder of slot 10.
    pub mint_reserved: FixedBytes<24>, // slot 10, offset 8 (fills remaining 24 bytes)
    /// Paused feature bitmask.
    #[accessor]
    #[mutator]
    pub paused: U256, // offset 11
    /// Maximum total supply.
    #[accessor]
    #[mutator]
    pub supply_cap: U256, // offset 12
    /// EIP-2612 permit nonces by owner.
    #[accessor(name = nonce, keys(owner))]
    #[mutator(name = set_nonce, keys(owner), value = nonce)]
    pub nonces: Mapping<Address, U256>, // offset 13
}

/// EVM-backed storage for the default B-20 variant.
#[contract]
#[derive(TokenAccounting)]
pub struct B20TokenStorage {
    pub b20: B20CoreStorage,
}

impl<'a> B20TokenStorage<'a> {
    /// Creates a `B20TokenStorage` instance targeting `addr`.
    ///
    /// Used by the factory to initialize token storage at a dynamically computed address.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    pub fn initialize(&mut self, init: B20TokenInit) -> Result<()> {
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, StorageKey, setup_storage};

    use crate::{
        B20CoreStorage, B20TokenRole, B20TokenStorage, TokenAccounting,
        b20::storage::{__packing_b20_core_storage, slots},
    };

    const TOKEN: Address = address!("000000000000000000000000000000000000b020");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);

    #[test]
    fn b20_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ID, "base.b20");
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);

        assert_eq!(slots::B20, B20_ROOT);
    }

    #[test]
    fn b20_core_offsets_match_mock_b20_storage() {
        assert_eq!(__packing_b20_core_storage::NAME_LOC.offset_slots, 0);
        assert_eq!(__packing_b20_core_storage::SYMBOL_LOC.offset_slots, 1);
        assert_eq!(__packing_b20_core_storage::CONTRACT_URI_LOC.offset_slots, 2);
        assert_eq!(__packing_b20_core_storage::TOTAL_SUPPLY_LOC.offset_slots, 3);
        assert_eq!(__packing_b20_core_storage::BALANCES_LOC.offset_slots, 4);
        assert_eq!(__packing_b20_core_storage::ALLOWANCES_LOC.offset_slots, 5);
        assert_eq!(__packing_b20_core_storage::ROLES_LOC.offset_slots, 6);
        assert_eq!(__packing_b20_core_storage::ROLE_ADMINS_LOC.offset_slots, 7);
        assert_eq!(__packing_b20_core_storage::ADMIN_COUNT_LOC.offset_slots, 8);
        assert_eq!(__packing_b20_core_storage::TRANSFER_SENDER_POLICY_ID_LOC.offset_slots, 9);
        assert_eq!(__packing_b20_core_storage::TRANSFER_SENDER_POLICY_ID_LOC.offset_bytes, 0);
        assert_eq!(__packing_b20_core_storage::TRANSFER_RECEIVER_POLICY_ID_LOC.offset_slots, 9);
        assert_eq!(__packing_b20_core_storage::TRANSFER_RECEIVER_POLICY_ID_LOC.offset_bytes, 8);
        assert_eq!(__packing_b20_core_storage::TRANSFER_EXECUTOR_POLICY_ID_LOC.offset_slots, 9);
        assert_eq!(__packing_b20_core_storage::TRANSFER_EXECUTOR_POLICY_ID_LOC.offset_bytes, 16);
        assert_eq!(__packing_b20_core_storage::TRANSFER_RESERVED_0_LOC.offset_slots, 9);
        assert_eq!(__packing_b20_core_storage::TRANSFER_RESERVED_0_LOC.offset_bytes, 24);
        assert_eq!(__packing_b20_core_storage::MINT_RECEIVER_POLICY_ID_LOC.offset_slots, 10);
        assert_eq!(__packing_b20_core_storage::MINT_RECEIVER_POLICY_ID_LOC.offset_bytes, 0);
        assert_eq!(__packing_b20_core_storage::PAUSED_LOC.offset_slots, 11);
        assert_eq!(__packing_b20_core_storage::SUPPLY_CAP_LOC.offset_slots, 12);
        assert_eq!(__packing_b20_core_storage::NONCES_LOC.offset_slots, 13);
    }

    #[test]
    fn b20_packed_slot_9_four_policy_ids_layout_matches_declared_offsets() {
        // Slot 9 packs four u64 fields at offset_bytes 0, 8, 16, 24.
        // Write distinct values to all four; read back via raw sload and verify
        // each field occupies its declared bit range without bleeding.
        let (mut storage, _) = setup_storage();

        let sender_id: u64 = 0x1111_1111_1111_1111;
        let receiver_id: u64 = 0x2222_2222_2222_2222;
        let executor_id: u64 = 0x3333_3333_3333_3333;
        let reserved: u64 = 0x4444_4444_4444_4444;

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20TokenStorage::from_address(TOKEN, ctx);
            token.b20.transfer_sender_policy_id.write(sender_id).unwrap();
            token.b20.transfer_receiver_policy_id.write(receiver_id).unwrap();
            token.b20.transfer_executor_policy_id.write(executor_id).unwrap();
            token.b20.transfer_reserved_0.write(reserved).unwrap();

            let slot_9 = B20_ROOT
                + U256::from(
                    __packing_b20_core_storage::TRANSFER_SENDER_POLICY_ID_LOC.offset_slots,
                );
            let raw = ctx.sload(TOKEN, slot_9).unwrap();
            let u64_mask = U256::from(u64::MAX);

            assert_eq!(raw & u64_mask, U256::from(sender_id), "sender_id at offset_bytes=0");
            assert_eq!((raw >> 64) & u64_mask, U256::from(receiver_id), "receiver_id at offset_bytes=8");
            assert_eq!((raw >> 128) & u64_mask, U256::from(executor_id), "executor_id at offset_bytes=16");
            assert_eq!((raw >> 192) & u64_mask, U256::from(reserved), "reserved at offset_bytes=24");
        });
    }

    #[test]
    fn b20_packed_slot_9_write_one_field_does_not_bleed_into_adjacent_fields() {
        // Writing only the sender policy ID must leave the three adjacent u64 ranges zero.
        let (mut storage, _) = setup_storage();
        let sender_id: u64 = 0xDEAD_BEEF_DEAD_BEEF;

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20TokenStorage::from_address(TOKEN, ctx);
            token.b20.transfer_sender_policy_id.write(sender_id).unwrap();

            let slot_9 = B20_ROOT + U256::from(9u64);
            let raw = ctx.sload(TOKEN, slot_9).unwrap();
            let u64_mask = U256::from(u64::MAX);

            assert_eq!(raw & u64_mask, U256::from(sender_id), "sender_id at bits [0..63]");
            assert_eq!((raw >> 64) & u64_mask, U256::ZERO, "receiver_id region must be zero");
            assert_eq!((raw >> 128) & u64_mask, U256::ZERO, "executor_id region must be zero");
            assert_eq!((raw >> 192) & u64_mask, U256::ZERO, "reserved region must be zero");
        });
    }

    #[test]
    fn b20_packed_slot_10_mint_policy_id_does_not_bleed_into_reserved() {
        // slot 10: mint_receiver_policy_id (u64 at offset_bytes=0) + mint_reserved (FixedBytes<24>).
        // Writing only the policy ID must leave the high 24 bytes zero.
        let (mut storage, _) = setup_storage();
        let mint_id: u64 = 0xCAFE_BABE_CAFE_BABE;

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20TokenStorage::from_address(TOKEN, ctx);
            token.b20.mint_receiver_policy_id.write(mint_id).unwrap();

            let slot_10 = B20_ROOT
                + U256::from(
                    __packing_b20_core_storage::MINT_RECEIVER_POLICY_ID_LOC.offset_slots,
                );
            let raw = ctx.sload(TOKEN, slot_10).unwrap();

            assert_eq!(raw & U256::from(u64::MAX), U256::from(mint_id), "mint_id at bits [0..63]");
            assert_eq!(raw >> 64, U256::ZERO, "mint_reserved region must be zero");
        });
    }

    #[test]
    fn b20_core_mapping_slots_are_rooted_at_namespace_offsets() {
        let (mut storage, _) = setup_storage();
        let holder = Address::repeat_byte(0xaa);
        let spender = Address::repeat_byte(0xbb);
        let role = B20TokenRole::Mint.id();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20TokenStorage::from_address(TOKEN, ctx);
            token.b20.balances.at_mut(&holder).write(U256::from(100)).unwrap();
            token.b20.allowances.at_mut(&holder).at_mut(&spender).write(U256::from(25)).unwrap();
            token.b20.roles.at_mut(&role).at_mut(&holder).write(true).unwrap();
            token.set_role_member_count(B20TokenRole::DefaultAdmin.id(), U256::ONE).unwrap();

            let balances_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::BALANCES_LOC.offset_slots);
            let allowances_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ALLOWANCES_LOC.offset_slots);
            let roles_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ROLES_LOC.offset_slots);
            let admin_count_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ADMIN_COUNT_LOC.offset_slots);

            assert_eq!(
                ctx.sload(TOKEN, holder.mapping_slot(balances_slot)).unwrap(),
                U256::from(100)
            );
            assert_eq!(
                ctx.sload(TOKEN, spender.mapping_slot(holder.mapping_slot(allowances_slot)))
                    .unwrap(),
                U256::from(25)
            );
            assert_eq!(
                ctx.sload(TOKEN, holder.mapping_slot(role.mapping_slot(roles_slot))).unwrap(),
                U256::ONE
            );
            assert_eq!(ctx.sload(TOKEN, admin_count_slot).unwrap(), U256::ONE);
        });
    }
}
