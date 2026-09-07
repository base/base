//! The top-level Block Access List structure.

use alloc::vec::Vec;

use alloy_eip7928::{AccountChanges as AlloyAccountChanges, BlockAccessList as AlloyBal};
use alloy_primitives::{Address, U256, map::AddressMap};

use super::{AccountBal, BalError, BlockAccessIndex};
use crate::{
    bytecode::BytecodeDecodeError,
    evm::state::{AccountInfo, PendingState},
};

/// BAL structure.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bal {
    /// Accounts bal.
    pub accounts: AddressMap<AccountBal>,
}

impl FromIterator<(Address, AccountBal)> for Bal {
    fn from_iter<I: IntoIterator<Item = (Address, AccountBal)>>(iter: I) -> Self {
        Self { accounts: iter.into_iter().collect() }
    }
}

impl Bal {
    /// Create a new BAL builder.
    pub fn new() -> Self {
        Self { accounts: AddressMap::default() }
    }

    /// Extend BAL with a transaction's detached [`PendingState`] at `bal_index`.
    ///
    /// This is the fold [`Evm`](crate::Evm) applies on transaction commit when its BAL builder is
    /// enabled: loaded-but-unchanged accounts and storage slots are recorded as reads, changed
    /// ones as writes at `bal_index`. Apply pending states in transaction order, since writes
    /// must be appended with ascending indices.
    pub fn commit(&mut self, bal_index: BlockAccessIndex, pending: PendingState) {
        for (address, entry) in &pending.accounts {
            self.update_account(
                bal_index,
                *address,
                entry.original.as_ref(),
                entry.present.as_ref(),
            );
        }
        for (address, overlay) in &pending.storage {
            self.accounts
                .entry(*address)
                .or_default()
                .storage
                .update_pending(bal_index, &overlay.slots);
        }
    }

    /// Extend BAL with one pending account overlay entry: its transaction-boundary original info
    /// against its present info. An absent side is the non-existent (default) account, so an
    /// account removed by the transaction records zeroed writes.
    ///
    /// A selfdestructed account needs no special-casing: transaction finalization already resolved
    /// its present info to the EIP-8246 balance-only remnant or to a removed account, and its
    /// destroyed storage writes surface as reads (execution-specs `destroy_storage`).
    #[inline]
    pub(crate) fn update_account(
        &mut self,
        bal_index: BlockAccessIndex,
        address: Address,
        original: Option<&AccountInfo>,
        present: Option<&AccountInfo>,
    ) {
        let bal_account = self.accounts.entry(address).or_default();
        let absent = AccountInfo::default();
        bal_account.account_info.update(
            bal_index,
            original.unwrap_or(&absent),
            present.unwrap_or(&absent),
        );
    }

    /// Populate storage slot from BAL by account address.
    ///
    /// If the account is not found in the BAL, or the slot is not listed under it, an error is
    /// returned.
    #[inline]
    pub fn populate_storage_slot(
        &self,
        account_address: Address,
        bal_index: BlockAccessIndex,
        key: U256,
        value: &mut U256,
    ) -> Result<(), BalError> {
        let Some(bal_account) = self.accounts.get(&account_address) else {
            return Err(BalError::AccountNotFound { address: account_address });
        };

        if let Some(bal_value) = bal_account.storage.get(&account_address, key, bal_index)? {
            *value = bal_value;
        };
        Ok(())
    }
}

impl From<Bal> for AlloyBal {
    /// Consume `Bal` and create a canonical EIP-7928 [`AlloyBal`].
    ///
    /// The returned access list is ordered deterministically: accounts are
    /// sorted lexicographically by address, and each account's nested reads and
    /// changes are sorted by the [`AccountBal`] `From` conversion into
    /// [`AlloyAccountChanges`].
    ///
    /// This matches the EIP-7928 ordering requirements:
    /// <https://eips.ethereum.org/EIPS/eip-7928#ordering-uniqueness-and-determinism>.
    fn from(bal: Bal) -> Self {
        let mut alloy_bal = Self::from_iter(
            bal.accounts
                .into_iter()
                .map(|(address, account)| AlloyAccountChanges { address, ..account.into() }),
        );
        alloy_bal.sort_unstable_by_key(|a| a.address);
        alloy_bal
    }
}

impl TryFrom<&[AlloyAccountChanges]> for Bal {
    type Error = BytecodeDecodeError;

    /// Convert borrowed EIP-7928 [`AlloyAccountChanges`] into a [`Bal`] without consuming
    /// the source.
    ///
    /// # Errors
    ///
    /// Returns [`BytecodeDecodeError`] if any account code change contains bytecode
    /// rejected by [`Bytecode::new_raw_checked`](crate::bytecode::Bytecode::new_raw_checked). This
    /// currently happens for malformed EIP-7702 bytecode, such as bytes with the EIP-7702 magic
    /// prefix but an invalid length or unsupported version.
    #[inline]
    fn try_from(alloy_bal: &[AlloyAccountChanges]) -> Result<Self, Self::Error> {
        let mut accounts =
            AddressMap::with_capacity_and_hasher(alloy_bal.len(), Default::default());
        for alloy_account in alloy_bal {
            accounts.insert(alloy_account.address, AccountBal::try_from(alloy_account)?);
        }

        Ok(Self { accounts })
    }
}

impl TryFrom<AlloyBal> for Bal {
    type Error = BytecodeDecodeError;

    /// Convert an EIP-7928 [`AlloyBal`] into a [`Bal`].
    ///
    /// # Errors
    ///
    /// Returns [`BytecodeDecodeError`] if any account code change contains bytecode
    /// rejected by [`Bytecode::new_raw_checked`](crate::bytecode::Bytecode::new_raw_checked). This
    /// currently happens for malformed EIP-7702 bytecode, such as bytes with the EIP-7702 magic
    /// prefix but an invalid length or unsupported version.
    #[inline]
    fn try_from(alloy_bal: AlloyBal) -> Result<Self, Self::Error> {
        let mut accounts =
            AddressMap::with_capacity_and_hasher(alloy_bal.len(), Default::default());
        for alloy_account in alloy_bal {
            let address = alloy_account.address;
            accounts.insert(address, AccountBal::try_from(alloy_account)?);
        }

        Ok(Self { accounts })
    }
}

impl core::fmt::Display for Bal {
    /// Pretty prints the entire BAL structure in a human-readable format.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "=== Block Access List (BAL) ===")?;
        writeln!(f, "Total accounts: {}", self.accounts.len())?;
        writeln!(f)?;

        if self.accounts.is_empty() {
            return writeln!(f, "(empty)");
        }

        // Sort accounts by address before printing
        let mut sorted_accounts: Vec<_> = self.accounts.iter().collect();
        sorted_accounts.sort_unstable_by_key(|(address, _)| *address);

        for (idx, (address, account)) in sorted_accounts.into_iter().enumerate() {
            writeln!(f, "Account #{idx} - Address: {address:?}")?;
            writeln!(f, "  Account Info:")?;

            // Print nonce writes
            if account.account_info.nonce.is_empty() {
                writeln!(f, "    Nonce: (read-only, no writes)")?;
            } else {
                writeln!(f, "    Nonce writes:")?;
                for change in &account.account_info.nonce.changes {
                    writeln!(f, "      [{}] -> {}", change.block_access_index, change.new_nonce)?;
                }
            }

            // Print balance writes
            if account.account_info.balance.is_empty() {
                writeln!(f, "    Balance: (read-only, no writes)")?;
            } else {
                writeln!(f, "    Balance writes:")?;
                for change in &account.account_info.balance.changes {
                    writeln!(
                        f,
                        "      [{}] -> {}",
                        change.block_access_index, change.post_balance
                    )?;
                }
            }

            // Print code writes
            if account.account_info.code.is_empty() {
                writeln!(f, "    Code: (read-only, no writes)")?;
            } else {
                writeln!(f, "    Code writes:")?;
                for change in &account.account_info.code.changes {
                    let (code_hash, bytecode) = &change.code;
                    writeln!(
                        f,
                        "      [{}] -> hash: {:?}, size: {} bytes",
                        change.block_access_index,
                        code_hash,
                        bytecode.len()
                    )?;
                }
            }

            // Print storage writes
            writeln!(f, "  Storage:")?;
            if account.storage.storage.is_empty() {
                writeln!(f, "    (no storage slots)")?;
            } else {
                writeln!(f, "    Total slots: {}", account.storage.storage.len())?;
                for (storage_key, slot_changes) in &account.storage.storage {
                    writeln!(f, "    Slot: {storage_key:#x}")?;
                    if slot_changes.is_empty() {
                        writeln!(f, "      (read-only, no writes)")?;
                    } else {
                        writeln!(f, "      Writes:")?;
                        for change in &slot_changes.changes {
                            writeln!(
                                f,
                                "        [{}] -> {:?}",
                                change.block_access_index, change.new_value
                            )?;
                        }
                    }
                }
            }

            writeln!(f)?;
        }
        write!(f, "=== End of BAL ===")
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_eip7928::{
        AccountChanges as AlloyAccountChanges, BalanceChange as AlloyBalanceChange,
        CodeChange as AlloyCodeChange, NonceChange as AlloyNonceChange,
        SlotChanges as AlloySlotChanges, StorageChange as AlloyStorageChange,
    };
    use alloy_primitives::{
        B256, Bytes, U256,
        map::{AddressSet, U256Map},
    };

    use super::*;
    use crate::{
        bytecode::Bytecode,
        evm::{
            bal::{AccountInfoBal, BalChanges, BalCodeChange, StorageBal},
            state::{Account, AccountInfo, StorageOverlay, StorageSlot, Tracked},
        },
    };

    fn code(byte: u8) -> (B256, Bytecode) {
        let bytecode = Bytecode::new_raw(vec![byte].into());
        (bytecode.hash_slow(), bytecode)
    }

    const fn idx(index: u64) -> BlockAccessIndex {
        BlockAccessIndex::new(index)
    }

    #[test]
    fn into_alloy_bal_canonicalizes_eip_7928_ordering() {
        let low_address = Address::with_last_byte(1);
        let high_address = Address::with_last_byte(2);

        let unordered_account = AccountBal {
            account_info: AccountInfoBal {
                nonce: BalChanges {
                    changes: vec![
                        AlloyNonceChange::new(idx(9), 90),
                        AlloyNonceChange::new(idx(4), 40),
                    ],
                },
                balance: BalChanges {
                    changes: vec![
                        AlloyBalanceChange::new(idx(5), U256::from(50)),
                        AlloyBalanceChange::new(idx(2), U256::from(20)),
                    ],
                },
                code: BalChanges {
                    changes: vec![
                        BalCodeChange::new(idx(7), code(7)),
                        BalCodeChange::new(idx(3), code(3)),
                    ],
                },
            },
            storage: StorageBal {
                storage: U256Map::from_iter([
                    (
                        U256::from(4),
                        BalChanges {
                            changes: vec![
                                AlloyStorageChange::new(idx(8), U256::from(80)),
                                AlloyStorageChange::new(idx(6), U256::from(60)),
                            ],
                        },
                    ),
                    (U256::from(1), BalChanges::default()),
                    (
                        U256::from(2),
                        BalChanges {
                            changes: vec![
                                AlloyStorageChange::new(idx(3), U256::from(30)),
                                AlloyStorageChange::new(idx(1), U256::from(10)),
                            ],
                        },
                    ),
                    (U256::from(3), BalChanges::default()),
                ]),
            },
        };

        let alloy_bal = AlloyBal::from(Bal::from_iter([
            (high_address, AccountBal::default()),
            (low_address, unordered_account),
        ]));

        assert_eq!(
            alloy_bal.iter().map(|account| account.address).collect::<Vec<_>>(),
            vec![low_address, high_address]
        );

        let account = &alloy_bal[0];
        assert_eq!(account.storage_reads, vec![U256::from(1), U256::from(3)]);
        assert_eq!(
            account.storage_changes.iter().map(|slot| slot.slot).collect::<Vec<_>>(),
            vec![U256::from(2), U256::from(4)]
        );
        assert_eq!(
            account.storage_changes[0]
                .changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![idx(1), idx(3)]
        );
        assert_eq!(
            account.storage_changes[1]
                .changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![idx(6), idx(8)]
        );
        assert_eq!(
            account
                .balance_changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![idx(2), idx(5)]
        );
        assert_eq!(
            account
                .nonce_changes
                .iter()
                .map(|change| change.block_access_index)
                .collect::<Vec<_>>(),
            vec![idx(4), idx(9)]
        );
        assert_eq!(
            account.code_changes.iter().map(|change| change.block_access_index).collect::<Vec<_>>(),
            vec![idx(3), idx(7)]
        );
    }

    fn slot(original: U256, current: U256) -> StorageSlot {
        StorageSlot { value: Tracked::from_parts(original, current), ..Default::default() }
    }

    #[test]
    fn commit_builds_bal_from_pending_overlay() {
        let address = Address::with_last_byte(1);

        // A freshly created account: no original info, present nonce/balance set, and one changed
        // storage slot plus one loaded-but-unchanged (read) slot.
        let account = Account {
            original: None,
            present: Some(AccountInfo::default().with_nonce(1).with_balance(U256::from(100))),
            ..Default::default()
        };
        let mut overlay = StorageOverlay::default();
        overlay.slots.insert(U256::from(5), slot(U256::ZERO, U256::from(42)));
        overlay.slots.insert(U256::from(6), slot(U256::from(7), U256::from(7)));
        let pending = PendingState {
            accounts: AddressMap::from_iter([(address, account)]),
            storage: AddressMap::from_iter([(address, overlay)]),
            selfdestructs: Default::default(),
        };

        let mut bal = Bal::new();
        bal.commit(idx(1), pending);

        let account = bal.accounts.get(&address).unwrap();
        assert_eq!(account.account_info.nonce.changes, vec![AlloyNonceChange::new(idx(1), 1)]);
        assert_eq!(
            account.account_info.balance.changes,
            vec![AlloyBalanceChange::new(idx(1), U256::from(100))]
        );
        // No code change, so no code writes.
        assert!(account.account_info.code.is_empty());
        // Changed slot recorded as a write.
        assert_eq!(
            account.storage.storage.get(&U256::from(5)).unwrap().changes,
            vec![AlloyStorageChange::new(idx(1), U256::from(42))]
        );
        // Loaded-but-unchanged slot recorded as a read (empty writes).
        assert!(account.storage.storage.get(&U256::from(6)).unwrap().is_empty());
    }

    #[test]
    fn commit_selfdestruct_uses_finalized_post_state() {
        let address = Address::with_last_byte(1);

        // Transaction finalization already resolved the selfdestructed account: removed
        // (`present: None`), storage overlay wiped with the prior writes turned into
        // loaded-but-unchanged slots surfacing as reads. The BAL derives from that overlay
        // without special-casing.
        let account = Account {
            original: Some(AccountInfo::default().with_balance(U256::from(100))),
            present: None,
            ..Default::default()
        };
        let mut overlay = StorageOverlay { wiped: true, ..Default::default() };
        overlay.slots.insert(U256::from(5), slot(U256::from(42), U256::from(42)));
        let pending = PendingState {
            accounts: AddressMap::from_iter([(address, account)]),
            storage: AddressMap::from_iter([(address, overlay)]),
            selfdestructs: AddressSet::from_iter([address]),
        };

        let mut bal = Bal::new();
        bal.commit(idx(2), pending);

        let account = bal.accounts.get(&address).unwrap();
        // Balance goes to zero on removal.
        assert_eq!(
            account.account_info.balance.changes,
            vec![AlloyBalanceChange::new(idx(2), U256::ZERO)]
        );
        // The loaded-but-unchanged slot stays a read (empty writes).
        assert!(account.storage.storage.get(&U256::from(5)).unwrap().is_empty());
    }

    #[test]
    fn try_from_alloy_decodes_block_access_list() {
        let address = Address::with_last_byte(1);
        let code_bytes = Bytes::from_static(&[0x60, 0x00]);
        let alloy_bal = vec![AlloyAccountChanges {
            address,
            code_changes: vec![AlloyCodeChange::new(idx(1), code_bytes.clone())],
            ..Default::default()
        }];

        let bal = Bal::try_from(alloy_bal).unwrap();
        let account = bal.accounts.get(&address).unwrap();
        let (_, bytecode) = &account.account_info.code.changes[0].code;

        assert_eq!(bytecode.original_bytes(), code_bytes);
    }

    #[test]
    fn try_from_alloy_ref_matches_owned_conversion() {
        let address = Address::with_last_byte(1);
        let code_bytes = Bytes::from_static(&[0x60, 0x00]);
        let alloy_bal = vec![AlloyAccountChanges {
            address,
            storage_changes: vec![AlloySlotChanges::new(
                U256::from(1),
                vec![AlloyStorageChange::new(idx(1), U256::from(10))],
            )],
            storage_reads: vec![U256::from(2)],
            balance_changes: vec![AlloyBalanceChange::new(idx(2), U256::from(20))],
            nonce_changes: vec![AlloyNonceChange::new(idx(3), 30)],
            code_changes: vec![AlloyCodeChange::new(idx(4), code_bytes.clone())],
        }];

        let borrowed = Bal::try_from(alloy_bal.as_slice()).unwrap();
        let owned = Bal::try_from(alloy_bal.clone()).unwrap();

        assert_eq!(borrowed, owned);
        assert_eq!(alloy_bal[0].code_changes[0].new_code(), &code_bytes);
    }

    #[test]
    fn try_from_alloy_errors_on_invalid_code_change() {
        let alloy_bal = vec![AlloyAccountChanges {
            address: Address::with_last_byte(1),
            code_changes: vec![AlloyCodeChange::new(idx(1), vec![0xef, 0x01, 0xde].into())],
            ..Default::default()
        }];

        assert!(Bal::try_from(alloy_bal).is_err());
    }

    #[test]
    fn try_from_alloy_ref_errors_on_invalid_code_change() {
        let alloy_bal = vec![AlloyAccountChanges {
            address: Address::with_last_byte(1),
            code_changes: vec![AlloyCodeChange::new(idx(1), vec![0xef, 0x01, 0xde].into())],
            ..Default::default()
        }];

        assert!(Bal::try_from(alloy_bal.as_slice()).is_err());
    }
}
