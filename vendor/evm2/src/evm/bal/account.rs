//! BAL builder module

use alloc::vec::Vec;

use alloy_eip7928::{
    AccountChanges as AlloyAccountChanges, BalanceChange, CodeChange as AlloyCodeChange,
    NonceChange, SlotChanges as AlloySlotChanges, StorageChange,
};
use alloy_primitives::{
    Address, B256, U256,
    map::{U256Map, hash_map::Entry},
};

use super::{
    BalError, BlockAccessIndex,
    changes::{BalChanges, BalCodeChange},
};
use crate::{
    bytecode::{Bytecode, BytecodeDecodeError},
    evm::state::{AccountInfo, StorageSlot},
};

/// Account BAL structure.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccountBal {
    /// Account info bal.
    pub account_info: AccountInfoBal,
    /// Storage bal.
    pub storage: StorageBal,
}

impl AccountBal {
    /// Populate account from BAL. Return true if account info got changed
    pub fn populate_account_info(
        &self,
        bal_index: BlockAccessIndex,
        account: &mut AccountInfo,
    ) -> bool {
        self.account_info.populate_account_info(bal_index, account)
    }
}

impl TryFrom<&AlloyAccountChanges> for AccountBal {
    type Error = BytecodeDecodeError;

    /// Create an account BAL from borrowed EIP-7928 [`AlloyAccountChanges`] without
    /// consuming the source.
    ///
    /// The account address is not part of the result; read it from
    /// [`AlloyAccountChanges::address`] before converting.
    ///
    /// # Errors
    ///
    /// Returns [`BytecodeDecodeError`] if any code change contains bytecode rejected by
    /// [`Bytecode::new_raw_checked`]. This currently happens for malformed EIP-7702
    /// bytecode, such as bytes with the EIP-7702 magic prefix but an invalid length or
    /// unsupported version.
    #[inline]
    fn try_from(alloy_account: &AlloyAccountChanges) -> Result<Self, Self::Error> {
        Ok(Self {
            account_info: AccountInfoBal {
                nonce: alloy_account.nonce_changes.clone().into(),
                balance: alloy_account.balance_changes.clone().into(),
                code: alloy_account
                    .code_changes
                    .iter()
                    .map(BalCodeChange::try_from)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            },
            storage: StorageBal::from_iter(
                alloy_account
                    .storage_changes
                    .iter()
                    .map(|slot| (slot.slot, slot.changes.clone().into()))
                    .chain(
                        alloy_account.storage_reads.iter().map(|key| (*key, BalChanges::default())),
                    ),
            ),
        })
    }
}

impl From<AccountBal> for AlloyAccountChanges {
    /// Consumes `AccountBal` and converts it into canonical EIP-7928
    /// [`AlloyAccountChanges`].
    ///
    /// The account address is not part of the source; the returned changes carry
    /// [`Address::ZERO`] and the caller is expected to set
    /// [`AlloyAccountChanges::address`].
    ///
    /// The returned account changes are ordered deterministically: storage reads
    /// and storage changes are sorted lexicographically by slot key, changes
    /// within each storage slot are sorted by block access index, and balance,
    /// nonce, and code changes are sorted by block access index.
    ///
    /// This matches the EIP-7928 ordering requirements:
    /// <https://eips.ethereum.org/EIPS/eip-7928#ordering-uniqueness-and-determinism>.
    #[inline]
    fn from(account: AccountBal) -> Self {
        let (storage_reads, writes) = account.storage.into_vecs();
        let storage_changes = writes
            .into_iter()
            .map(|(key, value)| {
                let mut changes = value.changes;
                changes.sort_unstable_by_key(|change| change.block_access_index);

                AlloySlotChanges::new(key, changes)
            })
            .collect::<Vec<_>>();

        let mut balance_changes = account.account_info.balance.changes;
        balance_changes.sort_unstable_by_key(|change| change.block_access_index);

        let mut nonce_changes = account.account_info.nonce.changes;
        nonce_changes.sort_unstable_by_key(|change| change.block_access_index);

        let mut code_changes = account
            .account_info
            .code
            .changes
            .into_iter()
            .map(AlloyCodeChange::from)
            .collect::<Vec<_>>();
        code_changes.sort_unstable_by_key(|change| change.block_access_index);

        Self {
            address: Address::ZERO,
            storage_changes,
            storage_reads,
            balance_changes,
            nonce_changes,
            code_changes,
        }
    }
}

impl TryFrom<AlloyAccountChanges> for AccountBal {
    type Error = BytecodeDecodeError;

    /// Create an account BAL from EIP-7928 [`AlloyAccountChanges`].
    ///
    /// The account address is not part of the result; read it from
    /// [`AlloyAccountChanges::address`] before converting.
    ///
    /// # Errors
    ///
    /// Returns [`BytecodeDecodeError`] if any code change contains bytecode rejected by
    /// [`Bytecode::new_raw_checked`]. This currently happens for malformed EIP-7702
    /// bytecode, such as bytes with the EIP-7702 magic prefix but an invalid length or
    /// unsupported version.
    #[inline]
    fn try_from(alloy_account: AlloyAccountChanges) -> Result<Self, Self::Error> {
        Ok(Self {
            account_info: AccountInfoBal {
                nonce: alloy_account.nonce_changes.into(),
                balance: alloy_account.balance_changes.into(),
                code: alloy_account
                    .code_changes
                    .iter()
                    .map(BalCodeChange::try_from)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            },
            storage: StorageBal::from_iter(
                alloy_account
                    .storage_changes
                    .into_iter()
                    .chain(
                        alloy_account
                            .storage_reads
                            .into_iter()
                            .map(|key| AlloySlotChanges::new(key, Default::default())),
                    )
                    .map(|slot| (slot.slot, slot.changes.into())),
            ),
        })
    }
}

/// Account info bal structure.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccountInfoBal {
    /// Nonce builder.
    pub nonce: BalChanges<NonceChange>,
    /// Balance builder.
    pub balance: BalChanges<BalanceChange>,
    /// Code builder.
    pub code: BalChanges<BalCodeChange>,
}

impl AccountInfoBal {
    /// Populate account info from BAL. Return true if account info got changed
    pub fn populate_account_info(
        &self,
        bal_index: BlockAccessIndex,
        account: &mut AccountInfo,
    ) -> bool {
        let mut changed = false;
        if let Some(nonce) = self.nonce.get(bal_index) {
            account.nonce = *nonce;
            changed = true;
        }
        if let Some(balance) = self.balance.get(bal_index) {
            account.balance = *balance;
            changed = true;
        }
        if let Some((code_hash, code)) = self.code.get(bal_index) {
            account.code_hash = *code_hash;
            account.code = Some(code.clone());
            changed = true;
        }
        changed
    }

    /// Extend account info from another account info.
    #[inline]
    pub fn update(
        &mut self,
        index: BlockAccessIndex,
        original: &AccountInfo,
        present: &AccountInfo,
    ) {
        self.nonce.update(index, &original.nonce, present.nonce);
        self.balance.update(index, &original.balance, present.balance);
        if original.code_hash != present.code_hash {
            self.code.update_with_key(
                index,
                &original.code_hash,
                (present.code_hash, present.code.clone().unwrap_or_default()),
                |i| &i.0,
            );
        }
    }

    /// Extend account info from another account info.
    #[inline]
    pub fn extend(&mut self, bal_account: Self) {
        self.nonce.extend(bal_account.nonce);
        self.balance.extend(bal_account.balance);
        self.code.extend(bal_account.code);
    }

    /// Update account balance in BAL.
    #[inline]
    pub fn balance_update(
        &mut self,
        bal_index: BlockAccessIndex,
        original_balance: &U256,
        balance: U256,
    ) {
        self.balance.update(bal_index, original_balance, balance);
    }

    /// Update account nonce in BAL.
    #[inline]
    pub fn nonce_update(&mut self, bal_index: BlockAccessIndex, original_nonce: &u64, nonce: u64) {
        self.nonce.update(bal_index, original_nonce, nonce);
    }

    /// Update account code in BAL.
    #[inline]
    pub fn code_update(
        &mut self,
        bal_index: BlockAccessIndex,
        original_code_hash: &B256,
        code_hash: B256,
        code: Bytecode,
    ) {
        self.code.update_with_key(bal_index, original_code_hash, (code_hash, code), |i| &i.0);
    }
}

/// Storage BAL
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StorageBal {
    /// Storage with writes and reads.
    pub storage: U256Map<BalChanges<StorageChange>>,
}

impl StorageBal {
    /// Get storage from the builder.
    #[inline]
    pub fn get(
        &self,
        address: &Address,
        key: U256,
        bal_index: BlockAccessIndex,
    ) -> Result<Option<U256>, BalError> {
        Ok(self.get_bal_changes(address, key)?.get(bal_index).copied())
    }

    /// Get storage changes from the builder.
    ///
    /// `address` is only needed in case of an error to propagate the address.
    #[inline]
    pub fn get_bal_changes(
        &self,
        address: &Address,
        key: U256,
    ) -> Result<&BalChanges<StorageChange>, BalError> {
        self.storage.get(&key).ok_or(BalError::SlotNotFound { address: *address, slot: key })
    }

    /// Extend storage from another storage.
    #[inline]
    pub fn extend(&mut self, storage: Self) {
        self.storage.reserve(storage.storage.len());
        for (key, value) in storage.storage {
            match self.storage.entry(key) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().extend(value);
                }
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }
    }

    /// Update storage from an account's pending [`StorageSlot`] overlay: a changed slot records a
    /// write at `bal_index`, a loaded-but-unchanged slot records a read.
    #[inline]
    pub fn update_pending(&mut self, bal_index: BlockAccessIndex, slots: &U256Map<StorageSlot>) {
        self.storage.reserve(slots.len());
        for (key, slot) in slots {
            self.storage.entry(*key).or_default().update(
                bal_index,
                &slot.value.original,
                slot.value.current,
            );
        }
    }

    /// Update reads with new storage keys.
    ///
    /// It will expend inner map with new reads.
    #[inline]
    pub fn update_reads(&mut self, storage: impl Iterator<Item = U256>) {
        for key in storage {
            self.storage.entry(key).or_default();
        }
    }

    /// Insert storage into the builder.
    pub fn extend_iter(
        &mut self,
        storage: impl Iterator<Item = (U256, BalChanges<StorageChange>)>,
    ) {
        for (key, value) in storage {
            self.storage.insert(key, value);
        }
    }

    /// Convert the storage into a vector of reads and writes, each sorted by slot key.
    pub fn into_vecs(self) -> (Vec<U256>, Vec<(U256, BalChanges<StorageChange>)>) {
        let len = self.storage.len();
        let mut reads = Vec::with_capacity(len);
        let mut writes = Vec::with_capacity(len);

        for (key, value) in self.storage {
            if value.is_empty() {
                reads.push(key);
            } else {
                writes.push((key, value));
            }
        }

        reads.sort_unstable();
        writes.sort_unstable_by_key(|&(key, _)| key);

        (reads, writes)
    }
}

impl FromIterator<(U256, BalChanges<StorageChange>)> for StorageBal {
    fn from_iter<I: IntoIterator<Item = (U256, BalChanges<StorageChange>)>>(iter: I) -> Self {
        Self { storage: iter.into_iter().collect() }
    }
}
