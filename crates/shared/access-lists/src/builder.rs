use std::u64;

use alloy_eip7928::{
    AccountChanges, BalanceChange, CodeChange, NonceChange, SlotChanges, StorageChange,
};
use alloy_primitives::{Address, U256};
use revm::{
    primitives::{HashMap, HashSet},
    state::Bytecode,
};

use crate::FlashblockAccessList;

/// A builder type for [`FlashblockAccessList`]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct FlashblockAccessListBuilder {
    /// Mapping from Address -> [`AccountChangesBuilder`]
    pub changes: HashMap<Address, AccountChangesBuilder>,
}

impl FlashblockAccessListBuilder {
    /// Creates a new [`FlashblockAccessListBuilder`]
    pub fn new() -> Self {
        Self { changes: Default::default() }
    }

    /// Merges another [`FlashblockAccessListBuilder`] with this one
    pub fn merge(&mut self, other: Self) {
        for (address, changes) in other.changes.into_iter() {
            self.changes
                .entry(address)
                .and_modify(|prev| prev.merge(changes.clone()))
                .or_insert(changes);
        }
    }

    /// Consumes the builder and produces a [`FlashblockAccessList`]
    pub fn build(self, min_tx_index: u64, max_tx_index: u64) -> FlashblockAccessList {
        let mut changes: Vec<_> = self.changes.into_iter().map(|(k, v)| v.build(k)).collect();
        changes.sort_unstable_by_key(|a| a.address);

        FlashblockAccessList::build(changes, min_tx_index, max_tx_index)
    }
}

/// A builder type for [`AccountChanges`]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct AccountChangesBuilder {
    /// Mapping from Storage Slot -> (Transaction Index -> New Value)
    pub storage_changes: HashMap<U256, HashMap<u64, U256>>,
    /// Set of storage slots
    pub storage_reads: HashSet<U256>,
    /// Mapping from Transaction Index -> New Balance
    pub balance_changes: HashMap<u64, U256>,
    /// Mapping from Transaction Index -> New Nonce
    pub nonce_changes: HashMap<u64, u64>,
    /// Mapping from Transaction Index -> New Code
    pub code_changes: HashMap<u64, Bytecode>,
}

impl AccountChangesBuilder {
    /// Merges another [`AccountChangesBuilder`] with this one
    pub fn merge(&mut self, other: Self) {
        for (slot, sc) in other.storage_changes {
            self.storage_changes
                .entry(slot)
                .and_modify(|prev| prev.extend(sc.clone()))
                .or_insert(sc);
        }
        self.storage_reads.extend(other.storage_reads);
        self.balance_changes.extend(other.balance_changes);
        self.nonce_changes.extend(other.nonce_changes);
        self.code_changes.extend(other.code_changes);
    }

    /// Consumes the builder and produces [`AccountChanges`]
    ///
    /// Sorting per FAL spec: storage keys lexicographic, block_access_index ascending
    pub fn build(mut self, address: Address) -> AccountChanges {
        let mut storage_changes: Vec<_> = self
            .storage_changes
            .drain()
            .map(|(slot, sc)| {
                let mut changes: Vec<_> = sc
                    .into_iter()
                    .map(|(tx_idx, val)| StorageChange {
                        block_access_index: tx_idx,
                        new_value: val.into(),
                    })
                    .collect();
                changes.sort_unstable_by_key(|c| c.block_access_index);
                SlotChanges { slot: slot.into(), changes }
            })
            .collect();
        storage_changes.sort_unstable_by_key(|sc| sc.slot);

        let mut storage_reads: Vec<_> = self.storage_reads.into_iter().collect();
        storage_reads.sort_unstable();

        let mut balance_changes: Vec<_> = self
            .balance_changes
            .into_iter()
            .map(|(tx_idx, val)| BalanceChange { block_access_index: tx_idx, post_balance: val })
            .collect();
        balance_changes.sort_unstable_by_key(|c| c.block_access_index);

        let mut nonce_changes: Vec<_> = self
            .nonce_changes
            .into_iter()
            .map(|(tx_idx, val)| NonceChange { block_access_index: tx_idx, new_nonce: val })
            .collect();
        nonce_changes.sort_unstable_by_key(|c| c.block_access_index);

        let mut code_changes: Vec<_> = self
            .code_changes
            .into_iter()
            .map(|(tx_idx, bc)| CodeChange {
                block_access_index: tx_idx,
                new_code: bc.original_bytes(),
            })
            .collect();
        code_changes.sort_unstable_by_key(|c| c.block_access_index);

        AccountChanges {
            address,
            storage_changes,
            storage_reads,
            balance_changes,
            nonce_changes,
            code_changes,
        }
    }
}
