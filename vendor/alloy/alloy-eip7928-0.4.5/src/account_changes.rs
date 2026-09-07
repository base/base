//! Contains the [`AccountChanges`] struct, which represents storage writes, balance, nonce, code
//! changes and read for the account. All changes for a single account, grouped by field type.
//! This eliminates address redundancy across different change types.

use crate::{
    SlotChanges, balance_change::BalanceChange, code_change::CodeChange, nonce_change::NonceChange,
};
use alloc::vec::Vec;
use alloy_primitives::{
    Address, U256,
    map::{HashMap, HashSet},
};

/// This struct is used to track the changes across accounts in a block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "rlp", derive(alloy_rlp::RlpEncodable, alloy_rlp::RlpDecodable))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "borsh", derive(borsh::BorshSerialize, borsh::BorshDeserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct AccountChanges {
    /// The address of the account whose changes are stored.
    pub address: Address,
    /// List of slot changes for this account.
    pub storage_changes: Vec<SlotChanges>,
    /// List of storage reads for this account.
    pub storage_reads: Vec<U256>,
    /// List of balance changes for this account.
    pub balance_changes: Vec<BalanceChange>,
    /// List of nonce changes for this account.
    pub nonce_changes: Vec<NonceChange>,
    /// List of code changes for this account.
    pub code_changes: Vec<CodeChange>,
}

impl AccountChanges {
    /// Creates a new [`AccountChanges`] instance for the given address with empty vectors.
    pub const fn new(address: Address) -> Self {
        Self {
            address,
            storage_changes: Vec::new(),
            storage_reads: Vec::new(),
            balance_changes: Vec::new(),
            nonce_changes: Vec::new(),
            code_changes: Vec::new(),
        }
    }

    /// Creates a new [`AccountChanges`] instance for the given address with specified capacity.
    pub fn with_capacity(address: Address, capacity: usize) -> Self {
        Self {
            address,
            storage_changes: Vec::with_capacity(capacity),
            storage_reads: Vec::with_capacity(capacity),
            balance_changes: Vec::with_capacity(capacity),
            nonce_changes: Vec::with_capacity(capacity),
            code_changes: Vec::with_capacity(capacity),
        }
    }

    /// Returns the address of the account.
    #[inline]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns the storage changes for this account.
    #[inline]
    pub fn storage_changes(&self) -> &[SlotChanges] {
        &self.storage_changes
    }

    /// Returns an iterator over the post-state value for each changed storage slot.
    ///
    /// The post-state value is taken from the last recorded change for each slot.
    #[inline]
    pub fn storage_post_states(&self) -> impl Iterator<Item = (U256, U256)> + '_ {
        self.storage_changes.iter().filter_map(|changes| {
            changes.changes.last().map(|change| (changes.slot, change.new_value))
        })
    }

    /// Merges another account change set into this one.
    ///
    /// Storage changes for matching slots are grouped together. Storage reads are normalized after
    /// merging so that written slots are represented by `storage_changes`, while `storage_reads`
    /// only contains unique read-only slots in first-seen order. This preserves the EIP-7928
    /// invariant that a storage slot appears in either reads or changes, but not both, after
    /// independently valid account change sets are combined.
    ///
    /// This preserves relative ordering by appending incoming changes to existing changes. Call
    /// [`Self::sort`] after merging if canonical EIP-7928 ordering is required.
    ///
    /// # Panics
    ///
    /// Panics if the two account change sets have different addresses.
    pub fn merge(&mut self, incoming: Self) {
        assert_eq!(
            self.address, incoming.address,
            "cannot merge account changes for different addresses"
        );

        merge_slot_changes(&mut self.storage_changes, incoming.storage_changes);
        self.storage_reads.extend(incoming.storage_reads);
        self.balance_changes.extend(incoming.balance_changes);
        self.nonce_changes.extend(incoming.nonce_changes);
        self.code_changes.extend(incoming.code_changes);

        let written = self
            .storage_changes
            .iter()
            .map(|slot_changes| slot_changes.slot)
            .collect::<HashSet<_>>();
        self.storage_reads.retain(|slot| !written.contains(slot));

        let mut seen = HashSet::with_capacity(self.storage_reads.len());
        self.storage_reads.retain(|slot| seen.insert(*slot));
    }

    /// Returns the storage reads for this account.
    #[inline]
    pub fn storage_reads(&self) -> &[U256] {
        &self.storage_reads
    }

    /// Returns the balance changes for this account.
    #[inline]
    pub fn balance_changes(&self) -> &[BalanceChange] {
        &self.balance_changes
    }

    /// Returns the nonce changes for this account.
    #[inline]
    pub fn nonce_changes(&self) -> &[NonceChange] {
        &self.nonce_changes
    }

    /// Returns the code changes for this account.
    #[inline]
    pub fn code_changes(&self) -> &[CodeChange] {
        &self.code_changes
    }

    /// Sorts this account's changes in-place according to the account-local EIP-7928 ordering
    /// rules.
    ///
    /// This applies the account-local ordering required by the "Ordering, Uniqueness and
    /// Determinism" section of EIP-7928:
    ///
    /// - `storage_changes` are sorted lexicographically by storage key
    /// - each per-slot `StorageChange` list is sorted by block access index in ascending order
    /// - `storage_reads` are sorted lexicographically by storage key
    /// - `balance_changes`, `nonce_changes`, and `code_changes` are sorted by block access index in
    ///   ascending order
    ///
    /// Per-slot storage change ordering is delegated to [`SlotChanges::sort`].
    ///
    /// This method only canonicalizes ordering for a single account. It does not enforce the
    /// EIP-7928 uniqueness constraints for storage keys or block access indexes.
    pub fn sort(&mut self) {
        self.storage_changes.sort_unstable_by_key(|changes| changes.slot);
        for slot_changes in &mut self.storage_changes {
            slot_changes.sort();
        }

        self.storage_reads.sort_unstable();
        self.balance_changes.sort_unstable_by_key(|change| change.block_access_index);
        self.nonce_changes.sort_unstable_by_key(|change| change.block_access_index);
        self.code_changes.sort_unstable_by_key(|change| change.block_access_index);
    }

    /// Set the address.
    pub const fn with_address(mut self, address: Address) -> Self {
        self.address = address;
        self
    }

    /// Add a storage read slot.
    pub fn with_storage_read(mut self, key: U256) -> Self {
        self.storage_reads.push(key);
        self
    }

    /// Add a storage change (multiple writes to a slot grouped in `SlotChanges`).
    pub fn with_storage_change(mut self, change: SlotChanges) -> Self {
        self.storage_changes.push(change);
        self
    }

    /// Add a balance change.
    pub fn with_balance_change(mut self, change: BalanceChange) -> Self {
        self.balance_changes.push(change);
        self
    }

    /// Add a nonce change.
    pub fn with_nonce_change(mut self, change: NonceChange) -> Self {
        self.nonce_changes.push(change);
        self
    }

    /// Add a code change.
    pub fn with_code_change(mut self, change: CodeChange) -> Self {
        self.code_changes.push(change);
        self
    }

    /// Add multiple storage reads at once.
    pub fn extend_storage_reads<I>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = U256>,
    {
        self.storage_reads.extend(iter);
        self
    }

    /// Add multiple slot changes at once.
    pub fn extend_storage_changes<I>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = SlotChanges>,
    {
        self.storage_changes.extend(iter);
        self
    }
}

fn merge_slot_changes(existing: &mut Vec<SlotChanges>, incoming: Vec<SlotChanges>) {
    let mut slot_positions = existing
        .iter()
        .enumerate()
        .map(|(idx, slot_changes)| (slot_changes.slot, idx))
        .collect::<HashMap<_, _>>();

    for slot_changes in incoming {
        if let Some(&idx) = slot_positions.get(&slot_changes.slot) {
            existing[idx].changes.extend(slot_changes.changes);
        } else {
            slot_positions.insert(slot_changes.slot, existing.len());
            existing.push(slot_changes);
        }
    }
}

#[cfg(test)]
mod merge_tests {
    use crate::{BlockAccessIndex, StorageChange};

    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn merge_groups_slot_changes_and_appends_account_changes() {
        let address = Address::from([0x11; 20]);
        let mut existing = AccountChanges {
            address,
            storage_changes: vec![SlotChanges::new(
                U256::from(1),
                vec![StorageChange::new(BlockAccessIndex::new(0), U256::from(10))],
            )],
            storage_reads: vec![U256::from(3)],
            balance_changes: vec![BalanceChange::new(BlockAccessIndex::new(1), U256::from(100))],
            nonce_changes: vec![NonceChange::new(BlockAccessIndex::new(2), 7)],
            code_changes: vec![],
        };
        let incoming = AccountChanges {
            address,
            storage_changes: vec![
                SlotChanges::new(
                    U256::from(1),
                    vec![StorageChange::new(BlockAccessIndex::new(3), U256::from(20))],
                ),
                SlotChanges::new(
                    U256::from(2),
                    vec![StorageChange::new(BlockAccessIndex::new(4), U256::from(30))],
                ),
            ],
            storage_reads: vec![U256::from(4)],
            balance_changes: vec![BalanceChange::new(BlockAccessIndex::new(5), U256::from(150))],
            nonce_changes: vec![NonceChange::new(BlockAccessIndex::new(6), 8)],
            code_changes: vec![CodeChange::new(
                BlockAccessIndex::new(7),
                Bytes::from_static(&[0xaa]),
            )],
        };

        existing.merge(incoming);

        assert_eq!(existing.storage_reads, vec![U256::from(3), U256::from(4)]);
        assert_eq!(
            existing.storage_changes.iter().map(|changes| changes.slot).collect::<Vec<_>>(),
            vec![U256::from(1), U256::from(2)]
        );
        assert_eq!(
            existing.storage_changes[0]
                .changes
                .iter()
                .map(|change| change.new_value)
                .collect::<Vec<_>>(),
            vec![U256::from(10), U256::from(20)]
        );
        assert_eq!(existing.balance_changes.len(), 2);
        assert_eq!(existing.nonce_changes.len(), 2);
        assert_eq!(existing.code_changes.len(), 1);
    }

    #[test]
    fn merge_normalizes_storage_reads_after_cross_block_merge() {
        let address = Address::from([0x33; 20]);
        const A: U256 = U256::from_limbs([1, 0, 0, 0]);
        const B: U256 = U256::from_limbs([2, 0, 0, 0]);
        const C: U256 = U256::from_limbs([3, 0, 0, 0]);
        const D: U256 = U256::from_limbs([4, 0, 0, 0]);

        let mut existing = AccountChanges {
            address,
            storage_changes: vec![SlotChanges::new(
                A,
                vec![StorageChange::new(BlockAccessIndex::new(0), U256::from(10))],
            )],
            storage_reads: vec![B, C],
            balance_changes: vec![],
            nonce_changes: vec![],
            code_changes: vec![],
        };
        let incoming = AccountChanges {
            address,
            storage_changes: vec![SlotChanges::new(
                B,
                vec![StorageChange::new(BlockAccessIndex::new(1), U256::from(20))],
            )],
            storage_reads: vec![A, C, D],
            balance_changes: vec![],
            nonce_changes: vec![],
            code_changes: vec![],
        };

        existing.merge(incoming);

        assert_eq!(
            existing
                .storage_changes
                .iter()
                .map(|slot_changes| slot_changes.slot)
                .collect::<Vec<_>>(),
            vec![A, B]
        );
        assert_eq!(existing.storage_reads, vec![C, D]);
        assert!(existing.storage_reads.iter().all(|read_slot| {
            !existing.storage_changes.iter().any(|slot_changes| slot_changes.slot == *read_slot)
        }));
    }

    #[test]
    #[should_panic(expected = "cannot merge account changes for different addresses")]
    fn merge_rejects_different_addresses() {
        let mut existing = AccountChanges::new(Address::from([0x11; 20]));
        let incoming = AccountChanges::new(Address::from([0x22; 20]));

        existing.merge(incoming);
    }
}

#[cfg(test)]
mod sort_tests {
    use crate::{BlockAccessIndex, StorageChange};

    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn sort_orders_account_local_eip7928_lists() {
        let mut account = AccountChanges {
            address: Address::from([0x11; 20]),
            storage_changes: vec![
                SlotChanges::new(
                    U256::from(3),
                    vec![
                        StorageChange::new(BlockAccessIndex::new(8), U256::from(0x80)),
                        StorageChange::new(BlockAccessIndex::new(2), U256::from(0x20)),
                    ],
                ),
                SlotChanges::new(
                    U256::from(1),
                    vec![
                        StorageChange::new(BlockAccessIndex::new(5), U256::from(0x50)),
                        StorageChange::new(BlockAccessIndex::new(1), U256::from(0x10)),
                    ],
                ),
            ],
            storage_reads: vec![U256::from(4), U256::from(2)],
            balance_changes: vec![
                BalanceChange::new(BlockAccessIndex::new(6), U256::from(600)),
                BalanceChange::new(BlockAccessIndex::new(3), U256::from(300)),
            ],
            nonce_changes: vec![
                NonceChange::new(BlockAccessIndex::new(7), 70),
                NonceChange::new(BlockAccessIndex::new(4), 40),
            ],
            code_changes: vec![
                CodeChange::new(BlockAccessIndex::new(9), Bytes::from_static(&[0x60, 0x09])),
                CodeChange::new(BlockAccessIndex::new(5), Bytes::from_static(&[0x60, 0x05])),
            ],
        };

        account.sort();

        assert_eq!(
            account.storage_changes.iter().map(|changes| changes.slot).collect::<Vec<_>>(),
            vec![U256::from(1), U256::from(3)]
        );
        assert_eq!(
            account.storage_changes[0]
                .changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(1), BlockAccessIndex::new(5)]
        );
        assert_eq!(
            account.storage_changes[1]
                .changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(2), BlockAccessIndex::new(8)]
        );
        assert_eq!(account.storage_reads, vec![U256::from(2), U256::from(4)]);
        assert_eq!(
            account
                .balance_changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(3), BlockAccessIndex::new(6)]
        );
        assert_eq!(
            account
                .nonce_changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(4), BlockAccessIndex::new(7)]
        );
        assert_eq!(
            account.code_changes.iter().map(|change| change.block_access_index).collect::<Vec<_>>(),
            vec![BlockAccessIndex::new(5), BlockAccessIndex::new(9)]
        );
    }
}

#[cfg(test)]
mod post_state_tests {
    use crate::{BlockAccessIndex, StorageChange};

    use super::*;

    #[test]
    fn storage_post_states_yields_last_change_per_slot() {
        let account = AccountChanges::new(Address::from([0x11; 20]))
            .with_storage_change(SlotChanges::new(
                U256::from(1),
                vec![
                    StorageChange::new(BlockAccessIndex::new(0), U256::from(0xaa)),
                    StorageChange::new(BlockAccessIndex::new(2), U256::from(0xbb)),
                ],
            ))
            .with_storage_change(SlotChanges::new(
                U256::from(3),
                vec![
                    StorageChange::new(BlockAccessIndex::new(1), U256::from(0xcc)),
                    StorageChange::new(BlockAccessIndex::new(3), U256::from(0xdd)),
                ],
            ));

        let post_states = account.storage_post_states().collect::<Vec<_>>();

        assert_eq!(
            post_states,
            vec![(U256::from(1), U256::from(0xbb)), (U256::from(3), U256::from(0xdd))]
        );
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use crate::{BlockAccessIndex, StorageChange};

    use super::*;
    use alloy_primitives::Bytes;
    use serde_json;

    #[test]
    fn test_account_changes_serde() {
        let acc = AccountChanges {
            address: Address::from([0x11; 20]),
            storage_changes: vec![SlotChanges {
                slot: U256::from(1),
                changes: vec![StorageChange {
                    block_access_index: BlockAccessIndex::new(0),
                    new_value: U256::from(100),
                }],
            }],
            storage_reads: vec![U256::from(2)],
            balance_changes: vec![BalanceChange {
                block_access_index: BlockAccessIndex::new(1),
                post_balance: U256::from(1000),
            }],
            nonce_changes: vec![NonceChange {
                block_access_index: BlockAccessIndex::new(2),
                new_nonce: 42,
            }],
            code_changes: vec![CodeChange {
                block_access_index: BlockAccessIndex::new(3),
                new_code: Bytes::from(vec![0x60, 0x00]),
            }],
        };

        let json = serde_json::to_string(&acc).unwrap();
        let decoded: AccountChanges = serde_json::from_str(&json).unwrap();

        assert_eq!(acc, decoded);
    }

    #[test]
    fn test_vec_account_changes_serde() {
        let acc1 = AccountChanges::new(Address::from([0x11; 20]))
            .with_storage_read(U256::from(1))
            .with_balance_change(BalanceChange {
                block_access_index: BlockAccessIndex::new(0),
                post_balance: U256::from(100),
            });

        let acc2 = AccountChanges::new(Address::from([0x22; 20]))
            .with_storage_change(SlotChanges {
                slot: U256::from(2),
                changes: vec![StorageChange {
                    block_access_index: BlockAccessIndex::new(1),
                    new_value: U256::from(200),
                }],
            })
            .with_nonce_change(NonceChange {
                block_access_index: BlockAccessIndex::new(2),
                new_nonce: 42,
            });

        let acc3 = AccountChanges::new(Address::from([0x33; 20])).with_code_change(CodeChange {
            block_access_index: BlockAccessIndex::new(3),
            new_code: Bytes::from(vec![0x60, 0x00]),
        });

        let vec_acc = vec![acc1, acc2, acc3];

        let json = serde_json::to_string(&vec_acc).unwrap();
        let decoded: Vec<AccountChanges> = serde_json::from_str(&json).unwrap();

        assert_eq!(vec_acc, decoded);
    }
}
