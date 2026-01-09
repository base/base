use alloy_primitives::{Address, B256};
use revm::{
    Database, DatabaseCommit,
    primitives::{HashMap, KECCAK_EMPTY, StorageKey, StorageValue},
    state::{Account, AccountInfo, Bytecode},
};
use tracing::error;

use crate::builder::FlashblockAccessListBuilder;

/// A [`Database`] implementation that builds an access list based on reads and writes
/// Use [`FBALBuilderDb::finish`] to build and retrieve the access list
#[derive(Debug)]
pub struct FBALBuilderDb<DB>
where
    DB: DatabaseCommit + Database,
{
    /// Underlying CacheDB
    db: DB,
    /// Transaction index of the txn being currently executed
    index: u64,
    /// Builder for the access list
    access_list: FlashblockAccessListBuilder,
    /// The most recent error generated during a commit attempt
    /// We need to store this as [`DatabaseCommit`] does not return an error
    /// and we need to return it on [`FBALBuilderDb::finish`] as that implies
    /// we weren't able to construct the access list properly
    error: Option<<Self as Database>::Error>,
}

impl<DB> FBALBuilderDb<DB>
where
    DB: DatabaseCommit + Database,
{
    /// Creates a new instance of [`FBALBuilderDb`] with the given underlying database
    pub fn new(db: DB) -> Self {
        Self { db, index: 0, access_list: Default::default(), error: None }
    }

    /// Returns a reference to the underlying database
    pub fn db(&self) -> &DB {
        &self.db
    }

    /// Returns a mutable reference to the underlying database
    pub fn db_mut(&mut self) -> &mut DB {
        &mut self.db
    }

    /// Sets the transaction index of the txn being currently executed
    pub fn set_index(&mut self, index: u64) {
        self.index = index;
    }

    /// Attempts to commit the changes to the underlying database
    /// as well as applies account/storage changes to the access list builder
    fn try_commit(
        &mut self,
        changes: HashMap<Address, Account>,
    ) -> Result<(), <DB as Database>::Error> {
        for (address, account) in changes.iter() {
            let changes = self.access_list.changes.entry(*address).or_default();

            // Update balance, nonce, and code
            match self.db.basic(*address)? {
                Some(prev) => {
                    if prev.balance != account.info.balance {
                        changes.balance_changes.insert(self.index, account.info.balance);
                    }

                    if prev.nonce != account.info.nonce {
                        changes.nonce_changes.insert(self.index, account.info.nonce);
                    }

                    if prev.code_hash != account.info.code_hash {
                        let bytecode = match account.info.code.clone() {
                            Some(code) => code,
                            None => self.db.code_by_hash(account.info.code_hash)?,
                        };
                        changes.code_changes.insert(self.index, bytecode);
                    }
                }
                None => {
                    // For new accounts, only record changes if they differ from defaults
                    if !account.info.balance.is_zero() {
                        changes.balance_changes.insert(self.index, account.info.balance);
                    }
                    if account.info.nonce != 0 {
                        changes.nonce_changes.insert(self.index, account.info.nonce);
                    }
                    // Only record code changes if the account actually has code
                    if account.info.code_hash != KECCAK_EMPTY {
                        let bytecode = match account.info.code.clone() {
                            Some(code) => code,
                            None => self.db.code_by_hash(account.info.code_hash)?,
                        };
                        changes.code_changes.insert(self.index, bytecode);
                    }
                }
            }

            // Update storage
            for (slot, value) in account.storage.iter() {
                let prev = value.original_value;
                let new = value.present_value;

                if prev != new {
                    changes.storage_changes.entry(*slot).or_default().insert(self.index, new);
                }
            }
        }

        self.db.commit(changes);
        Ok(())
    }

    /// Consumes the database and returns the access list back as well as the most recent
    /// error during commiting if any
    pub fn finish(self) -> Result<FlashblockAccessListBuilder, <Self as Database>::Error> {
        if let Some(e) = self.error {
            return Err(e);
        }

        Ok(self.access_list)
    }
}

impl<DB> Database for FBALBuilderDb<DB>
where
    DB: DatabaseCommit + Database,
{
    type Error = <DB as Database>::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.access_list.changes.entry(address).or_default();
        self.db.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let account = self.access_list.changes.entry(address).or_default();
        account.storage_reads.insert(index);
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

impl<DB> DatabaseCommit for FBALBuilderDb<DB>
where
    DB: DatabaseCommit + Database,
{
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        if let Err(e) = self.try_commit(changes) {
            error!("Failed to commit changes via FBALBuilderDb: {:?}", e);
            self.error = Some(e);
        }
    }
}
