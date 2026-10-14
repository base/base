//! Builder-side preflight for the EIP-7928 block access list bounds.
//!
//! reth rejects a payload whose block access list holds more than
//! `gas_limit / ITEM_COST` items before executing it, and the gossip layer drops a payload
//! envelope whose decoded size exceeds 10 MiB. Gas metering keeps ordinary blocks well inside
//! both bounds, but the builder must never seal a block that fails either one, so
//! [`BlockAccessListBudget`] tracks the list as transactions are committed and refuses a
//! transaction whose committed state would cross a bound.

use alloy_eips::eip7928::ITEM_COST;
use alloy_primitives::{
    U256,
    map::{AddressMap, HashSet},
};
use base_common_rpc_types_engine::MAX_DECOMPRESSED_ENVELOPE_BYTES;
use revm::{
    primitives::KECCAK_EMPTY,
    state::{EvmState, bal::Bal},
};

/// Bound of the block access list that admitting a transaction would cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlockAccessListLimit {
    /// The list would hold more items than the block gas limit allows (EIP-7928).
    #[error("block access list would hold {items} items, above the {max_items} item limit")]
    Items {
        /// Item count the list would reach.
        items: u64,
        /// Maximum item count for the block gas limit.
        max_items: u64,
    },
    /// The payload envelope carrying the list could exceed the gossip size limit.
    #[error("payload envelope could reach {bytes} bytes, above the {max_bytes} byte limit")]
    Size {
        /// Upper bound of the envelope size the payload would reach.
        bytes: u64,
        /// Maximum decoded envelope size.
        max_bytes: u64,
    },
}

/// Running EIP-7928 item count and payload-size bound for a block under construction.
///
/// The item count is exact: it mirrors the builder's block access list, which records one
/// item per accessed account and one per unique accessed storage slot. The size is an upper
/// bound on the decoded gossip envelope: exact transaction bytes plus a per-element ceiling on
/// the RLP-encoded block access list, so it can only refuse early, never admit an oversized
/// payload.
#[derive(Debug, Clone)]
pub struct BlockAccessListBudget {
    max_items: u64,
    items: u64,
    max_bytes: u64,
    bytes: u64,
    accessed: AddressMap<HashSet<U256>>,
}

impl BlockAccessListBudget {
    /// Ceiling on the envelope bytes outside the transactions and the list: the fixed payload
    /// fields, the extra data, the SSZ offsets, the signature and the parent beacon block root.
    pub const ENVELOPE_OVERHEAD_BYTES: u64 = 1_024;
    /// SSZ offset prepended to each transaction in the payload's transaction list.
    pub const TX_OFFSET_BYTES: u64 = 4;
    /// Ceiling on the outer RLP list header of the block access list.
    pub const LIST_HEADER_BYTES: u64 = 9;
    /// Ceiling on an account entry without its changes: the 21-byte address, five inner list
    /// headers and the account list header, each header at most 4 bytes under 16 MiB.
    pub const ACCOUNT_BYTES: u64 = 21 + 5 * 4 + 4;
    /// Ceiling on a storage slot entry without its writes: a 33-byte key plus the slot list
    /// header and the write list header.
    pub const SLOT_BYTES: u64 = 33 + 2 * 4;
    /// Ceiling on one balance, nonce or storage write: a 9-byte index, a 33-byte value and a
    /// 1-byte list header.
    pub const WRITE_BYTES: u64 = 1 + 9 + 33;
    /// Ceiling on one code write without the code bytes: a 9-byte index, the code string
    /// header and the list header.
    pub const CODE_WRITE_OVERHEAD_BYTES: u64 = 9 + 4 + 4;

    /// Returns the EIP-7928 item limit for a block gas limit: `gas_limit / ITEM_COST`.
    pub const fn max_items_for_gas_limit(gas_limit: u64) -> u64 {
        gas_limit / ITEM_COST as u64
    }

    /// Creates the budget for a block with `gas_limit`, seeded with the list the block has
    /// already built (pre-execution changes and sequencer transactions) and the encoded bytes,
    /// SSZ offsets included, of the transactions already committed.
    ///
    /// Fails when the seed alone already exceeds a bound: the block would be invalid whatever
    /// pool transactions follow.
    pub fn for_block(
        gas_limit: u64,
        built: &Bal,
        included_tx_bytes: u64,
    ) -> Result<Self, BlockAccessListLimit> {
        let budget = Self::new(
            Self::max_items_for_gas_limit(gas_limit),
            MAX_DECOMPRESSED_ENVELOPE_BYTES as u64,
            built,
            included_tx_bytes,
        );
        budget.check_limits()?;
        Ok(budget)
    }

    /// Creates a budget with explicit limits, seeded with the list built so far and the encoded
    /// bytes of the transactions already included.
    pub fn new(max_items: u64, max_bytes: u64, built: &Bal, included_tx_bytes: u64) -> Self {
        let mut accessed = AddressMap::default();
        let mut items = 0_u64;
        let mut bytes = Self::ENVELOPE_OVERHEAD_BYTES
            .saturating_add(Self::LIST_HEADER_BYTES)
            .saturating_add(included_tx_bytes);
        for (address, account) in &built.accounts {
            let slots = account.storage.storage.len() as u64;
            items = items.saturating_add(1).saturating_add(slots);
            let writes = account
                .storage
                .storage
                .values()
                .fold(0_u64, |writes, slot| writes.saturating_add(slot.writes.len() as u64))
                .saturating_add(account.account_info.balance.writes.len() as u64)
                .saturating_add(account.account_info.nonce.writes.len() as u64);
            let code_bytes = account.account_info.code.writes.iter().fold(0_u64, |bytes, write| {
                bytes
                    .saturating_add(Self::CODE_WRITE_OVERHEAD_BYTES)
                    .saturating_add(write.1.1.original_bytes().len() as u64)
            });
            bytes = bytes
                .saturating_add(Self::ACCOUNT_BYTES)
                .saturating_add(slots.saturating_mul(Self::SLOT_BYTES))
                .saturating_add(writes.saturating_mul(Self::WRITE_BYTES))
                .saturating_add(code_bytes);
            accessed.insert(*address, account.storage.storage.keys().copied().collect());
        }
        Self { max_items, items, max_bytes, bytes, accessed }
    }

    /// Fails when the list built so far already exceeds the item or the size limit.
    pub const fn check_limits(&self) -> Result<(), BlockAccessListLimit> {
        if self.items > self.max_items {
            return Err(BlockAccessListLimit::Items {
                items: self.items,
                max_items: self.max_items,
            });
        }
        if self.bytes > self.max_bytes {
            return Err(BlockAccessListLimit::Size {
                bytes: self.bytes,
                max_bytes: self.max_bytes,
            });
        }
        Ok(())
    }

    /// Returns the item count of the list built so far.
    pub const fn items(&self) -> u64 {
        self.items
    }

    /// Returns the EIP-7928 item limit.
    pub const fn max_items(&self) -> u64 {
        self.max_items
    }

    /// Returns the upper bound of the envelope size built so far.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns the envelope size limit.
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Admits a transaction whose execution produced `state` and whose encoded envelope is
    /// `tx_bytes` long, recording its accesses.
    ///
    /// A transaction that would push the list past the item limit or the envelope past the size
    /// limit is refused and leaves the budget unchanged, so the caller can discard it and keep
    /// building. A transaction that touches only already-listed accounts and slots adds no
    /// items and is admitted even when the list is at the limit.
    pub fn admit(&mut self, state: &EvmState, tx_bytes: u64) -> Result<(), BlockAccessListLimit> {
        let mut new_items = 0_u64;
        let mut new_bytes = tx_bytes.saturating_add(Self::TX_OFFSET_BYTES);
        for (address, account) in state {
            let known = self.accessed.get(address);
            if known.is_none() {
                new_items = new_items.saturating_add(1);
                new_bytes = new_bytes.saturating_add(Self::ACCOUNT_BYTES);
            }
            let selfdestructed = account.is_selfdestructed_locally();
            for (key, slot) in &account.storage {
                if !known.is_some_and(|slots| slots.contains(key)) {
                    new_items = new_items.saturating_add(1);
                    new_bytes = new_bytes.saturating_add(Self::SLOT_BYTES);
                }
                if selfdestructed || slot.is_changed() {
                    new_bytes = new_bytes.saturating_add(Self::WRITE_BYTES);
                }
            }
            if selfdestructed || account.is_changed() {
                let original = account.original_info();
                let (balance, nonce, code_hash, code_len) = if selfdestructed {
                    (U256::ZERO, 0, KECCAK_EMPTY, 0)
                } else {
                    let code_len =
                        account.info.code.as_ref().map_or(0, |code| code.original_bytes().len());
                    (account.info.balance, account.info.nonce, account.info.code_hash, code_len)
                };
                if original.balance != balance {
                    new_bytes = new_bytes.saturating_add(Self::WRITE_BYTES);
                }
                if original.nonce != nonce {
                    new_bytes = new_bytes.saturating_add(Self::WRITE_BYTES);
                }
                if original.code_hash != code_hash {
                    new_bytes = new_bytes
                        .saturating_add(Self::CODE_WRITE_OVERHEAD_BYTES)
                        .saturating_add(code_len as u64);
                }
            }
        }

        let items = self.items.saturating_add(new_items);
        if items > self.max_items {
            return Err(BlockAccessListLimit::Items { items, max_items: self.max_items });
        }
        let bytes = self.bytes.saturating_add(new_bytes);
        if bytes > self.max_bytes {
            return Err(BlockAccessListLimit::Size { bytes, max_bytes: self.max_bytes });
        }

        for (address, account) in state {
            self.accessed.entry(*address).or_default().extend(account.storage.keys().copied());
        }
        self.items = items;
        self.bytes = bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip7928::{AccountChanges, bal::Bal as AlloyBal};
    use alloy_primitives::{Address, Bytes, U256, keccak256};
    use revm::{
        bytecode::Bytecode,
        state::{
            Account, AccountInfo, AccountStatus, EvmState, EvmStorageSlot, TransactionId,
            bal::{AccountBal, Bal, BalWrites, BlockAccessIndex},
        },
    };

    use super::{BlockAccessListBudget, BlockAccessListLimit};

    /// Today's Base block gas limit (2 s blocks).
    const GAS_LIMIT_400M: u64 = 400_000_000;
    /// The 200 ms block gas limit (20M target, 40M limit).
    const GAS_LIMIT_40M: u64 = 40_000_000;

    /// A built list holding exactly `items` items: one account carrying `items - 1` storage reads.
    fn built_list_with_items(items: u64) -> Bal {
        let mut account = AccountBal::default();
        for slot in 0..items - 1 {
            account.storage.storage.insert(U256::from(slot), BalWrites::default());
        }
        let mut bal = Bal::new();
        bal.accounts.insert(Address::repeat_byte(0xaa), account);
        bal
    }

    /// Execution state that reads `slots` storage slots of `address` without changing it.
    fn read_state(address: Address, slots: impl IntoIterator<Item = u64>) -> EvmState {
        let mut account = Account::default();
        for slot in slots {
            account
                .storage
                .insert(U256::from(slot), EvmStorageSlot::new(U256::ZERO, TransactionId::ZERO));
        }
        EvmState::from_iter([(address, account)])
    }

    /// An alloy list holding exactly `items` items, as reth sees it.
    fn alloy_list_with_items(items: u64) -> AlloyBal {
        let account = (0..items - 1)
            .fold(AccountChanges::new(Address::repeat_byte(0xaa)), |account, slot| {
                account.with_storage_read(U256::from(slot))
            });
        AlloyBal::new(vec![account])
    }

    /// The budget's item limit must equal the bound reth's `Bal::validate_gas_limit` enforces:
    /// a list at the limit passes and one more item fails, for both block configurations.
    #[test]
    fn item_limit_matches_reth_gas_limit_check_for_both_configs() {
        for (gas_limit, expected_max) in [(GAS_LIMIT_400M, 200_000), (GAS_LIMIT_40M, 20_000)] {
            let max_items = BlockAccessListBudget::max_items_for_gas_limit(gas_limit);
            assert_eq!(
                max_items, expected_max,
                "gas limit {gas_limit} allows {expected_max} items"
            );

            let at_limit = alloy_list_with_items(max_items);
            assert_eq!(at_limit.total_bal_items(), max_items, "fixture must hold the limit");
            assert_eq!(
                at_limit.validate_gas_limit(gas_limit),
                Ok(()),
                "reth must accept a list at the item limit for gas limit {gas_limit}"
            );
            let over_limit = alloy_list_with_items(max_items + 1);
            assert!(
                over_limit.validate_gas_limit(gas_limit).is_err(),
                "reth must reject a list one item over the limit for gas limit {gas_limit}"
            );
        }
    }

    /// A transaction that brings the list exactly to the limit is admitted; the next one that
    /// adds an item is refused and leaves the budget untouched; a transaction that touches only
    /// listed accounts and slots is still admitted at the limit. Checked for both configs.
    #[test]
    fn admits_up_to_the_item_limit_and_refuses_one_past_it_for_both_configs() {
        for gas_limit in [GAS_LIMIT_400M, GAS_LIMIT_40M] {
            let max_items = BlockAccessListBudget::max_items_for_gas_limit(gas_limit);
            let mut budget = BlockAccessListBudget::for_block(
                gas_limit,
                &built_list_with_items(max_items - 2),
                0,
            )
            .expect("a seed under the limits must be accepted");
            assert_eq!(budget.items(), max_items - 2, "seed must count every built item");
            assert_eq!(budget.max_items(), max_items);

            // New account plus one new slot: exactly reaches the limit.
            let fresh = Address::repeat_byte(0x01);
            assert_eq!(budget.admit(&read_state(fresh, [7]), 100), Ok(()));
            assert_eq!(budget.items(), max_items, "the admitted transaction fills the list");

            // Any new account now crosses the limit by one.
            let bytes_before = budget.bytes();
            assert_eq!(
                budget.admit(&read_state(Address::repeat_byte(0x02), []), 100),
                Err(BlockAccessListLimit::Items { items: max_items + 1, max_items }),
                "one item past the limit must be refused for gas limit {gas_limit}"
            );
            assert_eq!(budget.items(), max_items, "a refusal must not change the item count");
            assert_eq!(budget.bytes(), bytes_before, "a refusal must not change the size");

            // A new slot on a listed account also crosses it.
            assert_eq!(
                budget.admit(&read_state(fresh, [8]), 100),
                Err(BlockAccessListLimit::Items { items: max_items + 1, max_items }),
                "a new slot past the limit must be refused for gas limit {gas_limit}"
            );

            // Re-touching listed accounts and slots adds nothing and still fits.
            let mut state = read_state(fresh, [7]);
            state.extend(read_state(Address::repeat_byte(0xaa), [0, 1]));
            assert_eq!(budget.admit(&state, 100), Ok(()));
            assert_eq!(budget.items(), max_items, "re-touching listed state adds no items");
        }
    }

    /// The size bound admits a transaction that lands exactly on the limit and refuses one byte
    /// more, without changing the budget.
    #[test]
    fn admits_up_to_the_size_limit_and_refuses_one_byte_past_it() {
        let overhead = BlockAccessListBudget::ENVELOPE_OVERHEAD_BYTES
            + BlockAccessListBudget::LIST_HEADER_BYTES;
        let tx_cost =
            100 + BlockAccessListBudget::TX_OFFSET_BYTES + BlockAccessListBudget::ACCOUNT_BYTES;
        let max_bytes = overhead + 50 + tx_cost;
        let mut budget = BlockAccessListBudget::new(u64::MAX, max_bytes, &Bal::new(), 50);
        assert_eq!(budget.bytes(), overhead + 50, "seed must count the included transactions");

        let sender = Address::repeat_byte(0x01);
        assert_eq!(budget.admit(&read_state(sender, []), 100), Ok(()));
        assert_eq!(budget.bytes(), max_bytes, "the admitted transaction lands on the limit");

        assert_eq!(
            budget.admit(&read_state(sender, []), 0),
            Err(BlockAccessListLimit::Size {
                bytes: max_bytes + BlockAccessListBudget::TX_OFFSET_BYTES,
                max_bytes,
            }),
            "any further transaction bytes must be refused"
        );
        assert_eq!(budget.bytes(), max_bytes, "a refusal must not change the size");
        assert_eq!(budget.items(), 1, "a refusal must not change the item count");
    }

    /// Changed slots, balances and nonces are charged a write each, and seeded writes are charged
    /// the same way, so the size bound covers every entry the encoded list carries.
    #[test]
    fn size_bound_charges_every_write() {
        let mut budget = BlockAccessListBudget::new(u64::MAX, u64::MAX, &Bal::new(), 0);
        let base = budget.bytes();

        let mut account =
            Account::from(AccountInfo { balance: U256::from(1), nonce: 1, ..Default::default() });
        account.info.balance = U256::from(2);
        account.info.nonce = 2;
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(5), TransactionId::ZERO),
        );
        account
            .storage
            .insert(U256::from(2), EvmStorageSlot::new(U256::from(3), TransactionId::ZERO));
        let address = Address::repeat_byte(0x01);
        assert_eq!(budget.admit(&EvmState::from_iter([(address, account)]), 10), Ok(()));
        assert_eq!(budget.items(), 3, "one account and two slots");
        assert_eq!(
            budget.bytes() - base,
            10 + BlockAccessListBudget::TX_OFFSET_BYTES
                + BlockAccessListBudget::ACCOUNT_BYTES
                + 2 * BlockAccessListBudget::SLOT_BYTES
                + 3 * BlockAccessListBudget::WRITE_BYTES,
            "one slot write, one balance write and one nonce write"
        );

        let mut seeded = AccountBal::default();
        seeded
            .storage
            .storage
            .insert(U256::from(1), BalWrites::new(vec![(BlockAccessIndex::new(1), U256::from(5))]));
        seeded.account_info.balance = BalWrites::new(vec![
            (BlockAccessIndex::new(1), U256::from(2)),
            (BlockAccessIndex::new(2), U256::from(3)),
        ]);
        let mut built = Bal::new();
        built.accounts.insert(address, seeded);
        let budget = BlockAccessListBudget::new(u64::MAX, u64::MAX, &built, 0);
        assert_eq!(budget.items(), 2, "one account and one slot");
        assert_eq!(
            budget.bytes() - base,
            BlockAccessListBudget::ACCOUNT_BYTES
                + BlockAccessListBudget::SLOT_BYTES
                + 3 * BlockAccessListBudget::WRITE_BYTES,
            "one slot write and two balance writes"
        );
    }

    /// The block budget enforces the 10 MiB decoded gossip envelope limit at both configs.
    #[test]
    fn block_budget_uses_the_gossip_envelope_limit() {
        for gas_limit in [GAS_LIMIT_400M, GAS_LIMIT_40M] {
            let budget = BlockAccessListBudget::for_block(gas_limit, &Bal::new(), 0)
                .expect("an empty seed must be accepted");
            assert_eq!(budget.max_bytes(), 10 * (1 << 20), "the envelope limit is 10 MiB");
            assert_eq!(budget.items(), 0, "an empty list holds no items");
        }
    }

    /// Commits `transactions` in order, once through the budget and once through revm's own
    /// block access list builder, then checks that the RLP encoding of the list the block would
    /// carry never exceeds the size the budget reserved for it, both incrementally and when the
    /// budget is seeded from the built list.
    fn assert_bound_covers_real_encoding(first_index: u64, transactions: &[EvmState]) {
        let mut budget = BlockAccessListBudget::new(u64::MAX, u64::MAX, &Bal::new(), 0);
        let mut built = Bal::new();
        for (offset, state) in transactions.iter().enumerate() {
            budget.admit(state, 0).expect("unbounded budget admits everything");
            let index = BlockAccessIndex::new(first_index + offset as u64);
            for (address, account) in state {
                built.update_account(index, *address, account);
            }
        }
        let encoded = alloy_rlp::encode(built.clone().into_alloy_bal()).len() as u64;
        let tx_offsets = transactions.len() as u64 * BlockAccessListBudget::TX_OFFSET_BYTES;
        let reserved = budget.bytes() - BlockAccessListBudget::ENVELOPE_OVERHEAD_BYTES - tx_offsets;
        assert!(
            encoded <= reserved,
            "incremental bound {reserved} must cover the {encoded}-byte encoded list"
        );
        let seeded = BlockAccessListBudget::new(u64::MAX, u64::MAX, &built, 0);
        let seeded_reserved = seeded.bytes() - BlockAccessListBudget::ENVELOPE_OVERHEAD_BYTES;
        assert!(
            encoded <= seeded_reserved,
            "seeded bound {seeded_reserved} must cover the {encoded}-byte encoded list"
        );
        assert_eq!(seeded.items(), budget.items(), "seeded and incremental counts agree");
    }

    /// An account whose balance and nonce move from `original` to `present`.
    fn changed_account(original: AccountInfo, present: AccountInfo) -> Account {
        let mut account = Account::from(original);
        account.info = present;
        account
    }

    /// Full-width balances, nonces, storage keys and values at a block access index far beyond
    /// any real transaction count: every field takes its maximum RLP width.
    #[test]
    fn bound_covers_full_width_values() {
        let mut account = changed_account(
            AccountInfo { balance: U256::from(1), nonce: 1, ..Default::default() },
            AccountInfo { balance: U256::MAX, nonce: u64::MAX, ..Default::default() },
        );
        for offset in 0..64_u64 {
            account.storage.insert(
                U256::MAX - U256::from(offset),
                EvmStorageSlot::new_changed(U256::from(1), U256::MAX, TransactionId::ZERO),
            );
        }
        assert_bound_covers_real_encoding(
            u64::MAX - 1,
            &[EvmState::from_iter([(Address::repeat_byte(0xff), account)])],
        );
    }

    /// Lists straddling every RLP header width: an account with one read (short lists), one
    /// whose reads exceed 55 bytes, one whose reads exceed 64 KiB, and one with thousands of
    /// written slots.
    #[test]
    fn bound_covers_long_list_header_boundaries() {
        let reads = |count: u64| {
            let mut account = Account::default();
            for slot in 0..count {
                account.storage.insert(
                    U256::MAX - U256::from(slot),
                    EvmStorageSlot::new(U256::from(1), TransactionId::ZERO),
                );
            }
            account
        };
        let mut writes = Account::default();
        for slot in 0..3_000_u64 {
            writes.storage.insert(
                U256::MAX - U256::from(slot),
                EvmStorageSlot::new_changed(U256::ZERO, U256::MAX, TransactionId::ZERO),
            );
        }
        let state = EvmState::from_iter([
            (Address::repeat_byte(0x01), reads(1)),
            (Address::repeat_byte(0x02), reads(2)),
            (Address::repeat_byte(0x03), reads(2_100)),
            (Address::repeat_byte(0x04), writes),
        ]);
        assert_bound_covers_real_encoding(70_000, &[state]);
    }

    /// Code changes, including code longer than 55 bytes and a maximum-size contract.
    #[test]
    fn bound_covers_code_changes() {
        let deployed = |len: usize| {
            let code = Bytes::from(vec![0x5b; len]);
            changed_account(
                AccountInfo::default(),
                AccountInfo {
                    nonce: 1,
                    code_hash: keccak256(&code),
                    code: Some(Bytecode::new_raw(code)),
                    ..Default::default()
                },
            )
        };
        let state = EvmState::from_iter([
            (Address::repeat_byte(0x01), deployed(1)),
            (Address::repeat_byte(0x02), deployed(56)),
            (Address::repeat_byte(0x03), deployed(24_576)),
        ]);
        assert_bound_covers_real_encoding(1, &[state]);
    }

    /// A self-destructed account records zero writes for every touched slot and its info.
    #[test]
    fn bound_covers_selfdestruct() {
        let code = Bytes::from(vec![0x5b; 100]);
        let mut account = Account::from(AccountInfo {
            balance: U256::MAX,
            nonce: 7,
            code_hash: keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            ..Default::default()
        });
        for slot in 0..10_u64 {
            account
                .storage
                .insert(U256::from(slot), EvmStorageSlot::new(U256::MAX, TransactionId::ZERO));
        }
        account.status |= AccountStatus::SelfDestructedLocal;
        assert!(account.is_selfdestructed_locally(), "fixture must self-destruct");
        assert_bound_covers_real_encoding(
            1,
            &[EvmState::from_iter([(Address::repeat_byte(0x05), account)])],
        );
    }

    /// A slot read by one transaction and written by later ones, plus an account whose balance
    /// changes in every transaction (like the fee vaults), across many block access indices.
    #[test]
    fn bound_covers_read_then_written_slots_across_transactions() {
        let slot = U256::MAX;
        let mut transactions = Vec::new();
        let mut reader = Account::default();
        reader.storage.insert(slot, EvmStorageSlot::new(U256::from(1), TransactionId::ZERO));
        transactions.push(EvmState::from_iter([(Address::repeat_byte(0x06), reader)]));
        for step in 0..50_u64 {
            let mut writer = Account::default();
            writer.storage.insert(
                slot,
                EvmStorageSlot::new_changed(
                    U256::from(step + 1),
                    U256::MAX - U256::from(step),
                    TransactionId::ZERO,
                ),
            );
            let vault = changed_account(
                AccountInfo { balance: U256::from(step + 1), ..Default::default() },
                AccountInfo { balance: U256::MAX - U256::from(step), ..Default::default() },
            );
            transactions.push(EvmState::from_iter([
                (Address::repeat_byte(0x06), writer),
                (Address::repeat_byte(0x19), vault),
            ]));
        }
        assert_bound_covers_real_encoding(65_000, &transactions);
    }
}
