//! Shared reads cached across repeated execution and speculative prewarming.
use core::cell::RefCell;

use alloy_primitives::{
    Address, B256, U256,
    map::{AddressMap, B256Map, Entry, HashMap, U256Map},
};
use base_execution_state_memory::AccountInfo;
use revm_bytecode::Bytecode;

use crate::{Database, DatabaseRef};

/// A container type that caches reads from an underlying [`DatabaseRef`].
///
/// This is intended to be used in conjunction with an execution state cache
/// during payload building which repeatedly accesses the same data.
///
/// [`CachedReads::as_db_mut`] transforms this type into a [`Database`] implementation that uses
/// [`CachedReads`] as a caching layer for operations, and records any cache misses.
///
/// # Example
///
/// ```
/// use base_execution_state_memory::{CachedReads, Database, DatabaseRef};
/// use alloy_primitives::Address;
///
/// fn build_payload<DB: DatabaseRef>(db: DB) {
///     let mut cached_reads = CachedReads::default();
///     let mut db = cached_reads.as_db_mut(db);
///     // this is `Database` and can be used to build a payload, it never commits to `CachedReads` or the underlying database, but all reads from the underlying database are cached in `CachedReads`.
///     // Subsequent payload build attempts can use cached reads and avoid hitting the underlying database.
///     // Note: `cached_reads` must outlive `db` to satisfy lifetime requirements.
///     let _account = db.basic(Address::ZERO);
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct CachedReads {
    /// Block state account with storage.
    pub accounts: AddressMap<CachedAccount>,
    /// Created contracts.
    pub contracts: B256Map<Bytecode>,
    /// Block hash mapped to the block number.
    pub block_hashes: HashMap<u64, B256>,
}

// === impl CachedReads ===

impl CachedReads {
    /// Creates a new [`CachedReads`] with capacity for account entries.
    pub fn with_account_capacity(capacity: usize) -> Self {
        Self {
            accounts: AddressMap::with_capacity_and_hasher(capacity, Default::default()),
            ..Default::default()
        }
    }

    /// Gets a [`DatabaseRef`] that will cache reads from the given database.
    pub const fn as_db<DB>(&mut self, db: DB) -> RefCell<CachedReadsDbMut<'_, DB>> {
        RefCell::new(self.as_db_mut(db))
    }

    /// Gets a mutable [`Database`] that will cache reads from the underlying database.
    pub const fn as_db_mut<DB>(&mut self, db: DB) -> CachedReadsDbMut<'_, DB> {
        CachedReadsDbMut { cached: self, db }
    }

    /// Inserts an account info into the cache.
    pub fn insert_account(&mut self, address: Address, info: AccountInfo, storage: U256Map<U256>) {
        self.accounts.insert(address, CachedAccount { info: Some(info), storage });
    }

    /// Extends current cache with entries from another [`CachedReads`] instance.
    ///
    /// Note: It is expected that both instances are based on the exact same state.
    pub fn extend(&mut self, other: Self) {
        self.accounts.extend(other.accounts);
        self.contracts.extend(other.contracts);
        self.block_hashes.extend(other.block_hashes);
    }
}

/// A [Database] that caches reads inside [`CachedReads`].
///
/// The lifetime parameter `'a` is tied to the lifetime of the underlying [`CachedReads`] instance.
/// This ensures that the cache remains valid for the entire duration this wrapper is used.
/// The original [`CachedReads`] must outlive this wrapper to prevent use-after-free.
#[derive(Debug)]
pub struct CachedReadsDbMut<'a, DB> {
    /// The cache of reads.
    pub cached: &'a mut CachedReads,
    /// The underlying database.
    pub db: DB,
}

impl<DB: DatabaseRef> Database for CachedReadsDbMut<'_, DB> {
    type Error = <DB as DatabaseRef>::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let basic = match self.cached.accounts.entry(address) {
            Entry::Occupied(entry) => entry.get().info.clone(),
            Entry::Vacant(entry) => {
                entry.insert(CachedAccount::new(self.db.basic_ref(address)?)).info.clone()
            }
        };
        Ok(basic)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let code = match self.cached.contracts.entry(code_hash) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry.insert(self.db.code_by_hash_ref(code_hash)?).clone(),
        };
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self.cached.accounts.entry(address) {
            Entry::Occupied(mut acc_entry) => match acc_entry.get_mut().storage.entry(index) {
                Entry::Occupied(entry) => Ok(*entry.get()),
                Entry::Vacant(entry) => Ok(*entry.insert(self.db.storage_ref(address, index)?)),
            },
            Entry::Vacant(acc_entry) => {
                // acc needs to be loaded for us to access slots.
                let info = self.db.basic_ref(address)?;
                let (account, value) = if info.is_some() {
                    let value = self.db.storage_ref(address, index)?;
                    let mut account = CachedAccount::new(info);
                    account.storage.insert(index, value);
                    (account, value)
                } else {
                    (CachedAccount::new(info), U256::ZERO)
                };
                acc_entry.insert(account);
                Ok(value)
            }
        }
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        let hash = match self.cached.block_hashes.entry(number) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => *entry.insert(self.db.block_hash_ref(number)?),
        };
        Ok(hash)
    }
}

/// Cached account contains the account state with storage
/// but lacks the account status.
#[derive(Debug, Clone)]
pub struct CachedAccount {
    /// Account state.
    pub info: Option<AccountInfo>,
    /// Account's storage.
    pub storage: U256Map<U256>,
}

impl CachedAccount {
    /// Creates an account entry with no cached storage slots.
    pub fn new(info: Option<AccountInfo>) -> Self {
        Self { info, storage: U256Map::default() }
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use alloy_primitives::Bytes;
    use mockall::predicate::eq;

    use super::*;

    mockall::mock! {
        pub Source {}
        impl DatabaseRef for Source {
            type Error = Infallible;
            fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Infallible>;
            fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Infallible>;
            fn storage_ref(&self, address: Address, key: U256) -> Result<U256, Infallible>;
            fn block_hash_ref(&self, number: u64) -> Result<B256, Infallible>;
        }
    }

    #[test]
    fn mutable_and_immutable_reads_share_cached_values() {
        let address = Address::repeat_byte(1);
        let hash = B256::repeat_byte(2);
        let block_hash = B256::repeat_byte(3);
        let account = AccountInfo { nonce: 7, ..Default::default() };
        let code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let mut source = MockSource::new();
        let loaded_account = account.clone();
        source
            .expect_basic_ref()
            .with(eq(address))
            .once()
            .returning(move |_| Ok(Some(loaded_account.clone())));
        let loaded_code = code.clone();
        source
            .expect_code_by_hash_ref()
            .with(eq(hash))
            .once()
            .returning(move |_| Ok(loaded_code.clone()));
        source
            .expect_storage_ref()
            .with(eq(address), eq(U256::from(9)))
            .once()
            .returning(|_, _| Ok(U256::from(11)));
        source.expect_block_hash_ref().with(eq(12)).once().returning(move |_| Ok(block_hash));
        let mut cache = CachedReads::default();
        {
            let mut db = cache.as_db_mut(&source);
            assert_eq!(db.basic(address).unwrap(), Some(account.clone()));
            assert_eq!(db.code_by_hash(hash).unwrap(), code);
            assert_eq!(db.storage(address, U256::from(9)).unwrap(), U256::from(11));
            assert_eq!(db.block_hash(12).unwrap(), block_hash);
        }
        let db = cache.as_db(&source);
        assert_eq!(db.basic_ref(address).unwrap(), Some(account));
        assert_eq!(db.code_by_hash_ref(hash).unwrap(), code);
        assert_eq!(db.storage_ref(address, U256::from(9)).unwrap(), U256::from(11));
        assert_eq!(db.block_hash_ref(12).unwrap(), block_hash);
    }

    #[test]
    fn missing_accounts_are_cached() {
        let mut source = MockSource::new();
        source.expect_basic_ref().with(eq(Address::ZERO)).once().returning(|_| Ok(None));
        let mut cache = CachedReads::default();
        let db = cache.as_db(&source);
        assert!(db.basic_ref(Address::ZERO).unwrap().is_none());
        assert!(db.basic_ref(Address::ZERO).unwrap().is_none());
    }

    #[test]
    fn test_extend_with_two_cached_reads() {
        // Setup test data
        let hash1 = B256::from_slice(&[1u8; 32]);
        let hash2 = B256::from_slice(&[2u8; 32]);
        let address1 = Address::from_slice(&[1u8; 20]);
        let address2 = Address::from_slice(&[2u8; 20]);

        // Create primary cache
        let mut primary = {
            let mut cache = CachedReads::default();
            cache.accounts.insert(address1, CachedAccount::new(Some(AccountInfo::default())));
            cache.contracts.insert(hash1, Bytecode::default());
            cache.block_hashes.insert(1, hash1);
            cache
        };

        // Create additional cache
        let additional = {
            let mut cache = CachedReads::default();
            cache.accounts.insert(address2, CachedAccount::new(Some(AccountInfo::default())));
            cache.contracts.insert(hash2, Bytecode::default());
            cache.block_hashes.insert(2, hash2);
            cache
        };

        // Extending primary with additional cache
        primary.extend(additional);

        // Verify the combined state
        assert!(
            primary.accounts.len() == 2
                && primary.contracts.len() == 2
                && primary.block_hashes.len() == 2,
            "All maps should contain 2 entries"
        );

        // Verify specific entries
        assert!(
            primary.accounts.contains_key(&address1)
                && primary.accounts.contains_key(&address2)
                && primary.contracts.contains_key(&hash1)
                && primary.contracts.contains_key(&hash2)
                && primary.block_hashes.get(&1) == Some(&hash1)
                && primary.block_hashes.get(&2) == Some(&hash2),
            "All expected entries should be present"
        );
    }
}
