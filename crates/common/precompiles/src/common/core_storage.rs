//! Core B-20 EVM storage layout shared by all token variants.

use alloc::string::String;

use alloy_primitives::{Address, B256, FixedBytes, U256};
use base_precompile_macros::Storable;
use base_precompile_storage::Mapping;

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

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, uint};
    use base_precompile_storage::StorableType;

    use super::__packing_b20_core_storage;
    use crate::B20CoreStorage;

    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);

    #[test]
    fn b20_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ID, "base.b20");
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);
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
}
