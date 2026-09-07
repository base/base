//! Contains the [`SlotChanges`] struct, which represents all changes made to a single storage slot
//! across multiple transactions.

use crate::StorageChange;
use alloc::vec::Vec;
use alloy_primitives::U256;

/// Represents all changes made to a single storage slot across multiple transactions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "rlp", derive(alloy_rlp::RlpEncodable, alloy_rlp::RlpDecodable))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "borsh", derive(borsh::BorshSerialize, borsh::BorshDeserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct SlotChanges {
    /// The storage slot key being modified.
    pub slot: U256,
    /// A list of write operations to this slot, ordered by transaction index.
    #[cfg_attr(feature = "serde", serde(alias = "slotChanges"))]
    pub changes: Vec<StorageChange>,
}

impl SlotChanges {
    /// Creates a new [`SlotChanges`] instance for the given slot key and changes.
    #[inline]
    pub const fn new(slot: U256, changes: Vec<StorageChange>) -> Self {
        Self { slot, changes }
    }

    /// Creates a new [`SlotChanges`] with preallocated capacity for the given number of changes.
    #[inline]
    pub fn with_capacity(slot: U256, capacity: usize) -> Self {
        Self { slot, changes: Vec::with_capacity(capacity) }
    }

    /// Appends a storage change to the list.
    #[inline]
    pub fn push(&mut self, change: StorageChange) {
        self.changes.push(change)
    }

    /// Returns `true` if no changes have been recorded.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Returns the number of changes recorded for this slot.
    #[inline]
    pub const fn len(&self) -> usize {
        self.changes.len()
    }

    /// Sorts this slot's storage changes by block access index in ascending order.
    ///
    /// This applies the per-slot ordering required by the "Ordering, Uniqueness and Determinism"
    /// section of EIP-7928. It only canonicalizes ordering and does not enforce uniqueness of block
    /// access indexes.
    pub fn sort(&mut self) {
        self.changes.sort_unstable_by_key(|change| change.block_access_index);
    }

    /// Creates a new `SlotChanges` for the given slot.
    #[inline]
    pub const fn with_slot(mut self, slot: U256) -> Self {
        self.slot = slot;
        self
    }

    /// Creates a new `SlotChanges` with the given change appended.
    #[inline]
    pub fn with_change(mut self, change: StorageChange) -> Self {
        self.changes.push(change);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockAccessIndex;

    #[test]
    fn sort_orders_changes_by_block_access_index() {
        let mut slot_changes = SlotChanges::new(
            U256::from(1),
            vec![
                StorageChange::new(BlockAccessIndex::new(8), U256::from(0x80)),
                StorageChange::new(BlockAccessIndex::new(2), U256::from(0x20)),
                StorageChange::new(BlockAccessIndex::new(5), U256::from(0x50)),
            ],
        );

        slot_changes.sort();

        assert_eq!(
            slot_changes.changes.iter().map(|change| change.block_access_index).collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(2), BlockAccessIndex::new(5), BlockAccessIndex::new(8)]
        );
    }
}
