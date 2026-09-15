//! Core B-20 EVM storage layout shared by all token variants.

use alloc::string::String;

use alloy_primitives::{Address, B256, FixedBytes, U256};
use base_precompile_macros::Storable;
use base_precompile_storage::{Mapping, Result, StorableType, StorageKey, StorageOps, Word};

use crate::TransferPolicyIds;

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
    // The base-std mock keeps an `initialized` bootstrap flag as its last field; this impl checks
    // factory-init via deployed marker bytecode instead, so it stores no such field.
    /// Seize-exempt policy ID, consulted against `from` by the seize operations. Accounts
    /// authorized by this policy are exempt from seizure; `from` is seizable only when it is NOT
    /// in the scope. The unset always-allow default keeps seizure closed until an issuer
    /// configures it.
    #[accessor]
    #[mutator]
    pub seize_exempt_policy_id: u64, // slot 14, offset 0
    /// Seize-receiver policy ID, consulted against `to` by the seize operations.
    #[accessor]
    #[mutator]
    pub seize_receiver_policy_id: u64, // slot 14, offset 8
    /// Reserved padding to close slot 14.
    pub seize_reserved: FixedBytes<16>, // slot 14, offset 16
}

impl B20CoreStorage {
    /// Maximum storage slots read by a `transferFrom` with distinct accounts.
    pub const TRANSFER_HINT_SLOTS: usize = 5;

    /// Storage slots a `transfer` (`spender == None`) or `transferFrom` reads, derivable from
    /// calldata alone: the paused bitmask, the packed transfer-policy-id word, both balances
    /// (deduplicated for self-transfers), and the `allowances[from][spender]` entry for the
    /// `transferFrom` path.
    ///
    /// This returns a stack-backed buffer plus its initialized length so issuing an optional
    /// prefetch hint never allocates. Slot arithmetic mirrors the generated handlers: namespace
    /// root plus the generated per-field offset, with mapping keys folded in via
    /// [`StorageKey::mapping_slot`].
    pub fn transfer_hint_slots(
        from: Address,
        to: Address,
        spender: Option<Address>,
    ) -> ([U256; Self::TRANSFER_HINT_SLOTS], usize) {
        let root = <Self as StorableType>::STORAGE_NAMESPACE_ROOT;
        let balances = root.saturating_add(__packing_b20_core_storage::BALANCES);
        let mut slots = [U256::ZERO; Self::TRANSFER_HINT_SLOTS];
        slots[0] = root.saturating_add(__packing_b20_core_storage::PAUSED);
        slots[1] = root.saturating_add(__packing_b20_core_storage::TRANSFER_SENDER_POLICY_ID);
        slots[2] = from.mapping_slot(balances);
        let mut slot_count = 3;
        if to != from {
            slots[slot_count] = to.mapping_slot(balances);
            slot_count += 1;
        }
        if let Some(spender) = spender {
            let allowances = root.saturating_add(__packing_b20_core_storage::ALLOWANCES);
            slots[slot_count] = spender.mapping_slot(from.mapping_slot(allowances));
            slot_count += 1;
        }
        (slots, slot_count)
    }
}

impl B20CoreStorageHandler<'_> {
    /// Reads the sender/receiver/executor transfer policy ids in a single SLOAD.
    ///
    /// The three ids are packed into one storage word (slot 9); loading it once and extracting each
    /// lane avoids the three separate SLOADs the per-field accessors would incur. Offsets come from
    /// the generated packing constants, so they track the field layout (also pinned by the
    /// offset test below).
    pub fn transfer_policy_ids(&self) -> Result<TransferPolicyIds> {
        let slot = self.transfer_sender_policy_id.slot();
        let word = StorageOps::load(&self.transfer_sender_policy_id, slot)?;
        Ok(TransferPolicyIds {
            sender: Word::extract_from_word::<u64>(
                word,
                __packing_b20_core_storage::TRANSFER_SENDER_POLICY_ID_LOC.offset_bytes,
                size_of::<u64>(),
            )?,
            receiver: Word::extract_from_word::<u64>(
                word,
                __packing_b20_core_storage::TRANSFER_RECEIVER_POLICY_ID_LOC.offset_bytes,
                size_of::<u64>(),
            )?,
            executor: Word::extract_from_word::<u64>(
                word,
                __packing_b20_core_storage::TRANSFER_EXECUTOR_POLICY_ID_LOC.offset_bytes,
                size_of::<u64>(),
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, uint};
    use base_precompile_storage::StorableType;

    use super::__packing_b20_core_storage;
    use crate::B20CoreStorage;

    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);

    #[test]
    fn transfer_hint_slots_dedupe_self_transfer_balance() {
        let account = Address::repeat_byte(0xaa);
        let spender = Address::repeat_byte(0xbb);
        assert_eq!(B20CoreStorage::transfer_hint_slots(account, account, None).1, 3);
        assert_eq!(B20CoreStorage::transfer_hint_slots(account, account, Some(spender)).1, 4);
    }

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
        assert_eq!(__packing_b20_core_storage::SEIZE_EXEMPT_POLICY_ID_LOC.offset_slots, 14);
        assert_eq!(__packing_b20_core_storage::SEIZE_EXEMPT_POLICY_ID_LOC.offset_bytes, 0);
        assert_eq!(__packing_b20_core_storage::SEIZE_RECEIVER_POLICY_ID_LOC.offset_slots, 14);
        assert_eq!(__packing_b20_core_storage::SEIZE_RECEIVER_POLICY_ID_LOC.offset_bytes, 8);
        assert_eq!(__packing_b20_core_storage::SEIZE_RESERVED_LOC.offset_slots, 14);
        assert_eq!(__packing_b20_core_storage::SEIZE_RESERVED_LOC.offset_bytes, 16);
    }
}
