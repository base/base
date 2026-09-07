//! In-memory cache database.

use core::convert::Infallible;

use alloy_primitives::{
    Address, B256, KECCAK256_EMPTY,
    map::{AddressMap, B256Map, U256Map, hash_map::Entry},
};

use super::{DbResult, DynDatabase, EmptyDB};
use crate::{
    AnyError, ErrorCode,
    bytecode::Bytecode,
    evm::{
        bal::BalContext,
        state::{
            Account, AccountChangeRef, AccountInfo, PendingState, StateChangeSink,
            StateChangeSource, StorageChange, StorageOverlay,
        },
    },
    interpreter::Word,
};

/// A database implementation that stores initial state in memory.
pub type InMemoryDB = CacheDB<EmptyDB>;

/// Cached storage for one account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountStorageCache {
    /// Cached persistent slots for this account.
    pub slots: U256Map<Word>,
    /// Whether missing slots are known to be zero because storage was wiped.
    pub wiped: bool,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl AccountStorageCache {
    /// Marks all storage for this account as wiped.
    #[inline]
    pub fn wipe(&mut self) {
        self.slots.clear();
        self.wiped = true;
    }
}

/// Cache used by [`CacheDB`].
///
/// Accounts and code are stored separately: accounts carry the code hash, and bytecode is keyed by
/// that hash in [`Self::contracts`]. Account and storage entries are authoritative for this cache
/// layer: a cached `None` account or wiped per-account storage cache shadows the wrapped database
/// instead of falling through to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cache {
    /// Accounts keyed by address. `None` means the account is known to be absent/deleted.
    pub accounts: AddressMap<Option<AccountInfo>>,
    /// Contracts keyed by code hash.
    pub contracts: B256Map<Bytecode>,
    /// Persistent storage keyed by account, then slot.
    pub storage: AddressMap<AccountStorageCache>,
    /// Cached block hashes keyed by block number.
    pub block_hashes: U256Map<B256>,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl Default for Cache {
    #[inline]
    fn default() -> Self {
        let mut contracts = B256Map::default();
        contracts.insert(KECCAK256_EMPTY, Bytecode::default());
        contracts.insert(B256::ZERO, Bytecode::default());
        Self {
            accounts: AddressMap::default(),
            contracts,
            storage: AddressMap::default(),
            block_hashes: U256Map::default(),
            _non_exhaustive: (),
        }
    }
}

/// A cache database over another backing database.
///
/// The optional EIP-7928 Block Access List machinery is carried in [`Self::bal_context`]; when an
/// attached BAL is present there, [`DynDatabase`] reads are served from it (layered over the
/// cache/database). Keeping that state in [`BalContext`] leaves this wrapper otherwise
/// BAL-agnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheDB<ExtDB = EmptyDB> {
    /// The cache that stores all local state.
    pub cache: Cache,
    /// Wrapped backing database.
    pub db: ExtDB,
    /// Optional EIP-7928 Block Access List read/build state. Default is empty (no BAL attached and
    /// no builder), in which case reads go straight to the cache/database.
    pub bal_context: BalContext,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl Default for CacheDB<EmptyDB> {
    #[inline]
    fn default() -> Self {
        Self::new(EmptyDB::default())
    }
}

impl<ExtDB> CacheDB<ExtDB> {
    /// Creates a new cache over a backing database.
    #[inline]
    pub fn new(db: ExtDB) -> Self {
        Self { cache: Cache::default(), db, bal_context: BalContext::new(), _non_exhaustive: () }
    }

    /// Applies borrowed state changes to this cache.
    ///
    /// When BAL construction is enabled, the visited changes and reads are also folded into the
    /// BAL builder at the current block access index, matching [`Self::commit_pending`].
    #[inline]
    pub fn commit_source<S: StateChangeSource>(&mut self, source: &S) {
        let Ok(()) = source.visit(self);
    }

    /// Accepts a detached [`PendingState`] -- a committed transaction's post-state -- into this
    /// cache.
    ///
    /// When BAL construction is enabled, the pending state -- including loaded-but-unchanged
    /// reads -- is first folded into the BAL builder at the current block access index. Changed
    /// storage slots, code, and account info are then applied to the cache; the wrapped backing
    /// database is not written.
    pub fn commit_pending(&mut self, pending: &PendingState) {
        self.commit(&pending.accounts, &pending.storage);
    }

    /// Accepts a committed transaction's pending accounts and storage overlays into this cache.
    ///
    /// Same as [`Self::commit_pending`], operating on the transaction layers directly so the
    /// overlay need not be detached.
    pub(crate) fn commit(
        &mut self,
        accounts: &AddressMap<Account>,
        storage: &AddressMap<StorageOverlay>,
    ) {
        // When BAL construction is enabled, fold the transaction's pending post-state into the
        // builder before applying the changes to the cache.
        self.bal_context.commit(accounts, storage);

        for (&address, overlay) in storage {
            if overlay.wiped {
                self.cache.storage.entry(address).or_default().wipe();
            }
            for (&key, slot) in overlay.changed_slots() {
                self.cache.storage.entry(address).or_default().slots.insert(key, slot.current);
            }
        }

        for (&address, entry) in accounts {
            if let Some((code_hash, code)) = entry.changed_code() {
                self.cache.contracts.insert(code_hash, code.clone());
            }
            if !entry.is_changed() {
                continue;
            }
            match entry.present.as_ref() {
                Some(account) => self.insert_account_info(&address, account.clone_no_code()),
                None => {
                    self.cache.accounts.insert(address, None);
                    self.cache.storage.entry(address).or_default().wipe();
                }
            }
        }
    }

    /// Inserts account code into the contract cache.
    #[inline]
    pub fn insert_contract(&mut self, info: &mut AccountInfo) {
        Self::insert_contract_inner(&mut self.cache.contracts, info);
    }

    #[inline]
    fn insert_contract_inner(contracts: &mut B256Map<Bytecode>, info: &mut AccountInfo) {
        if let Some(code) = &info.code
            && !code.is_empty()
        {
            if info.code_hash == KECCAK256_EMPTY {
                info.code_hash = code.hash_slow();
            }
            contracts.entry(info.code_hash).or_insert_with(|| code.clone());
        }
        if info.code_hash.is_zero() {
            info.code_hash = KECCAK256_EMPTY;
        }
    }

    /// Inserts account info.
    #[inline]
    pub fn insert_account_info(&mut self, address: &Address, mut info: AccountInfo) {
        self.insert_contract(&mut info);
        info.code = None;
        self.cache.accounts.insert(*address, Some(info));
    }

    /// Returns cached account info if the account exists in the cache.
    #[inline]
    pub fn account_info(&self, address: &Address) -> Option<&AccountInfo> {
        self.cache.accounts.get(address).and_then(Option::as_ref)
    }

    /// Inserts persistent storage without modifying cached account info.
    #[inline]
    pub fn insert_account_storage(&mut self, address: &Address, key: &Word, value: &Word) {
        self.cache.storage.entry(*address).or_default().slots.insert(*key, *value);
    }

    /// Sets a historical block hash.
    #[inline]
    pub fn insert_block_hash(&mut self, number: &Word, hash: &B256) {
        self.cache.block_hashes.insert(*number, *hash);
    }
}

/// Applies streamed changes to the cache and forwards every callback to [`BalContext`]'s own sink
/// impl, so a visited source folds into the BAL builder exactly like [`CacheDB::commit_pending`]
/// when construction is enabled.
impl<ExtDB> StateChangeSink for CacheDB<ExtDB> {
    type Error = Infallible;

    #[inline]
    fn bytecode(&mut self, code_hash: B256, code: &Bytecode) -> Result<(), Self::Error> {
        self.bal_context.bytecode(code_hash, code)?;
        self.cache.contracts.insert(code_hash, code.clone());
        Ok(())
    }

    #[inline]
    fn storage_wipe(&mut self, address: Address) -> Result<(), Self::Error> {
        self.bal_context.storage_wipe(address)?;
        self.cache.storage.entry(address).or_default().wipe();
        Ok(())
    }

    #[inline]
    fn storage(&mut self, change: StorageChange) -> Result<(), Self::Error> {
        self.bal_context.storage(change)?;
        let storage = self.cache.storage.entry(change.address).or_default();
        if storage.wiped && change.current.is_zero() {
            storage.slots.remove(&change.key);
        } else {
            storage.slots.insert(change.key, change.current);
        }
        Ok(())
    }

    #[inline]
    fn account(&mut self, change: AccountChangeRef<'_>) -> Result<(), Self::Error> {
        self.bal_context.account(change)?;
        match change.current {
            Some(info) => self.insert_account_info(&change.address, info.clone()),
            None => {
                self.cache.accounts.insert(change.address, None);
                self.cache.storage.entry(change.address).or_default().wipe();
            }
        }
        Ok(())
    }

    #[inline]
    fn account_read(
        &mut self,
        address: Address,
        info: Option<&AccountInfo>,
    ) -> Result<(), Self::Error> {
        self.bal_context.account_read(address, info)
    }

    #[inline]
    fn storage_read(
        &mut self,
        address: Address,
        key: Word,
        value: Word,
    ) -> Result<(), Self::Error> {
        self.bal_context.storage_read(address, key, value)
    }
}

impl<ExtDB: DynDatabase> DynDatabase for CacheDB<ExtDB> {
    #[inline]
    fn get_account(&mut self, address: &Address) -> DbResult<Option<AccountInfo>> {
        // Resolve the account in the attached read BAL first: with fallback disabled, an
        // uncovered account errors before the cache or backing database is consulted.
        let bal_account = match self.bal_context.get_bal_account(address) {
            Ok(bal_account) => bal_account,
            Err(err) => return Err(self.bal_context.store_error(err)),
        };

        // Resolve the raw account from the cache or backing database. The cache always stores the
        // raw value; any attached BAL is layered onto the returned value only.
        let mut account = {
            let Cache { accounts, contracts, .. } = &mut self.cache;
            match accounts.entry(*address) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => match self.db.get_account(address)? {
                    Some(mut info) => {
                        Self::insert_contract_inner(contracts, &mut info);
                        info.code = None;
                        entry.insert(Some(info)).clone()
                    }
                    None => entry.insert(None).clone(),
                },
            }
        };

        // Apply the resolved BAL account's info at the current block access index.
        if let Some(bal_account) = bal_account {
            self.bal_context.populate_bal_account(bal_account, &mut account);
        }
        Ok(account)
    }

    #[inline]
    fn get_code_by_hash(&mut self, code_hash: &B256) -> DbResult<Bytecode> {
        match self.cache.contracts.entry(*code_hash) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => Ok(entry.insert(self.db.get_code_by_hash(code_hash)?).clone()),
        }
    }

    #[inline]
    fn get_storage(&mut self, address: &Address, key: &Word) -> DbResult<Word> {
        // Serve the slot from the attached read BAL when it covers a write at or before the current
        // index; otherwise fall through to the cache/database.
        match self.bal_context.bal_storage(address, key) {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(err) => return Err(self.bal_context.store_error(err)),
        }

        // A cached slot can be a locally committed write even when the backing account is absent.
        // Only use account absence to suppress uncached backing-storage reads.
        match self.cache.storage.entry(*address) {
            Entry::Occupied(mut entry) => {
                let storage = entry.get_mut();
                match storage.slots.entry(*key) {
                    Entry::Occupied(slot) => Ok(*slot.get()),
                    Entry::Vacant(slot) => {
                        if storage.wiped {
                            return Ok(Word::ZERO);
                        }
                        if self.cache.accounts.get(address).is_some_and(Option::is_none) {
                            return Ok(Word::ZERO);
                        }
                        let value = self.db.get_storage(address, key)?;
                        Ok(*slot.insert(value))
                    }
                }
            }
            Entry::Vacant(entry) => {
                if self.cache.accounts.get(address).is_some_and(Option::is_none) {
                    return Ok(Word::ZERO);
                }
                let value = self.db.get_storage(address, key)?;
                let mut storage = AccountStorageCache::default();
                storage.slots.insert(*key, value);
                entry.insert(storage);
                Ok(value)
            }
        }
    }

    #[inline]
    fn get_block_hash(&mut self, number: &Word) -> DbResult<B256> {
        match self.cache.block_hashes.entry(*number) {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => Ok(*entry.insert(self.db.get_block_hash(number)?)),
        }
    }

    #[inline]
    fn error(&mut self, code: ErrorCode) -> AnyError {
        if let Some(err) = self.bal_context.take_error(code) {
            return err;
        }
        self.db.error(code)
    }
}

mod typed {
    use super::*;
    use crate::evm::Database;

    impl<ExtDB: DynDatabase> Database for CacheDB<ExtDB> {
        type Error = AnyError;

        #[inline]
        fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            DynDatabase::get_account(self, address).map_err(|code| DynDatabase::error(self, code))
        }

        #[inline]
        fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
            DynDatabase::get_code_by_hash(self, code_hash)
                .map_err(|code| DynDatabase::error(self, code))
        }

        #[inline]
        fn get_storage(&mut self, address: &Address, key: &Word) -> Result<Word, Self::Error> {
            DynDatabase::get_storage(self, address, key)
                .map_err(|code| DynDatabase::error(self, code))
        }

        #[inline]
        fn get_block_hash(&mut self, number: &Word) -> Result<B256, Self::Error> {
            DynDatabase::get_block_hash(self, number).map_err(|code| DynDatabase::error(self, code))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, sync::Arc, vec};

    use alloy_eip7928::{BalanceChange, NonceChange, StorageChange};
    use alloy_primitives::Bytes;

    use super::*;
    use crate::{
        evm::bal::{AccountBal, Bal, BalChanges, BlockAccessIndex},
        interpreter::op,
    };

    #[derive(Debug, Default)]
    struct CountingDB {
        account: Option<AccountInfo>,
        storage: Word,
        block_hash: B256,
        account_loads: usize,
        code_loads: usize,
        storage_loads: usize,
        block_hash_loads: usize,
    }

    impl crate::evm::Database for CountingDB {
        type Error = core::convert::Infallible;

        fn get_account(&mut self, _address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.account_loads += 1;
            Ok(self.account.clone())
        }

        fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
            self.code_loads += 1;
            Ok(self
                .account
                .as_ref()
                .filter(|info| info.code_hash == *code_hash)
                .and_then(|info| info.code.clone())
                .unwrap_or_default())
        }

        fn get_storage(&mut self, _address: &Address, _key: &Word) -> Result<Word, Self::Error> {
            self.storage_loads += 1;
            Ok(self.storage)
        }

        fn get_block_hash(&mut self, _number: &Word) -> Result<B256, Self::Error> {
            self.block_hash_loads += 1;
            Ok(self.block_hash)
        }
    }

    #[test]
    fn cache_db_caches_wrapped_db_reads() {
        let address = Address::with_last_byte(1);
        let key = Word::from(2);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let block_hash = B256::with_last_byte(3);
        let db = CountingDB {
            account: Some(AccountInfo::default().with_code(code.clone())),
            storage: Word::from(4),
            block_hash,
            ..CountingDB::default()
        };
        let mut cache = CacheDB::new(crate::evm::Db::new(db));

        let code_hash = code.hash_slow();
        assert_eq!(
            cache.get_account(&address).unwrap().map(|info| info.code_hash),
            Some(code_hash)
        );
        assert_eq!(cache.get_code_by_hash(&code_hash).unwrap(), code);
        assert_eq!(cache.get_code_by_hash(&code_hash).unwrap(), code);
        assert_eq!(cache.db.inner().account_loads, 1);
        assert_eq!(cache.db.inner().code_loads, 0);

        assert_eq!(cache.get_storage(&address, &key).unwrap(), Word::from(4));
        assert_eq!(cache.get_storage(&address, &key).unwrap(), Word::from(4));
        assert_eq!(cache.db.inner().storage_loads, 1);

        assert_eq!(cache.get_block_hash(&Word::from(5)).unwrap(), block_hash);
        assert_eq!(cache.get_block_hash(&Word::from(5)).unwrap(), block_hash);
        assert_eq!(cache.db.inner().block_hash_loads, 1);
    }

    #[test]
    fn cached_storage_write_overrides_cached_absent_account() {
        let address = Address::with_last_byte(1);
        let key = Word::from(2);
        let value = Word::from(3);
        let mut cache = CacheDB::new(crate::evm::Db::new(CountingDB::default()));

        assert!(cache.get_account(&address).unwrap().is_none());

        let mut pending = PendingState::default();
        pending.insert_storage(address, key, Word::ZERO, value);
        cache.commit_pending(&pending);

        assert_eq!(cache.get_storage(&address, &key).unwrap(), value);
    }

    #[test]
    fn deleted_account_wipes_cached_storage() {
        let address = Address::with_last_byte(1);
        let key = Word::from(2);
        let original = AccountInfo::default();
        let mut cache = CacheDB::default();
        cache.insert_account_info(&address, original.clone());
        cache.insert_account_storage(&address, &key, &Word::from(3));

        let mut pending = PendingState::default();
        pending.insert_storage(address, key, Word::from(3), Word::from(4));
        pending.insert_account(address, Some(original), None);
        cache.commit_pending(&pending);

        assert!(cache.get_account(&address).unwrap().is_none());
        assert_eq!(cache.get_storage(&address, &key).unwrap(), Word::ZERO);
        assert!(cache.cache.storage[&address].wiped);
        assert!(cache.cache.storage[&address].slots.is_empty());
    }

    #[test]
    fn seeded_storage_does_not_shadow_wrapped_account() {
        let address = Address::with_last_byte(1);
        let key = Word::from(2);
        let value = Word::from(3);
        let code = Bytecode::new_legacy(Bytes::from_static(&[op::STOP]));
        let account = AccountInfo::default()
            .with_balance(Word::from(4))
            .with_nonce(5)
            .with_code(code.clone());
        let mut cache = CacheDB::new(crate::evm::Db::new(CountingDB {
            account: Some(account.clone()),
            ..CountingDB::default()
        }));

        cache.insert_account_storage(&address, &key, &value);

        assert_eq!(cache.get_storage(&address, &key).unwrap(), value);
        assert_eq!(cache.get_account(&address).unwrap(), Some(account));
        assert_eq!(cache.get_code_by_hash(&code.hash_slow()).unwrap(), code);
        assert_eq!(cache.db.inner().account_loads, 1);
    }

    /// A counting cache with an attached read BAL positioned at index 2.
    fn cache_with_read_bal(
        address: Address,
        allow_db_fallback: bool,
    ) -> CacheDB<crate::evm::Db<CountingDB>> {
        let mut cache = counting_cache();
        cache.bal_context = BalContext::new()
            .with_bal(Arc::new(read_bal(address)))
            .with_allow_db_fallback(allow_db_fallback);
        cache.bal_context.set_bal_index(BlockAccessIndex::new(2));
        cache
    }

    /// Read BAL covering address `1`: a balance write and a storage write to slot `7`, both at
    /// index 1 (so visible from index 2 onward).
    fn read_bal(address: Address) -> Bal {
        let mut account = AccountBal::default();
        account.account_info.balance =
            BalChanges::new(vec![BalanceChange::new(BlockAccessIndex::new(1), Word::from(500))]);
        account.storage.storage.insert(
            Word::from(7),
            BalChanges::new(vec![StorageChange::new(BlockAccessIndex::new(1), Word::from(42))]),
        );
        Bal::from_iter([(address, account)])
    }

    fn counting_cache() -> CacheDB<crate::evm::Db<CountingDB>> {
        let db = CountingDB {
            account: Some(AccountInfo::default().with_balance(Word::from(100)).with_nonce(3)),
            storage: Word::from(9),
            ..CountingDB::default()
        };
        CacheDB::new(crate::evm::Db::new(db))
    }

    #[test]
    fn attached_bal_serves_account_and_storage_reads() {
        let address = Address::with_last_byte(1);
        let mut cache = cache_with_read_bal(address, false);

        // Balance comes from the BAL write; nonce has no BAL write, so it stays the database value.
        let account = cache.get_account(&address).unwrap().unwrap();
        assert_eq!(account.balance, Word::from(500));
        assert_eq!(account.nonce, 3);

        // Storage slot 7 is served from the BAL, shadowing the database value (9).
        assert_eq!(cache.get_storage(&address, &Word::from(7)).unwrap(), Word::from(42));
    }

    #[test]
    fn uncovered_read_errors_without_fallback() {
        let address = Address::with_last_byte(1);
        let mut cache = cache_with_read_bal(address, false);

        // Slot 9 is not listed in the BAL for a covered account -> BAL is invalid for this access.
        let code = cache.get_storage(&address, &Word::from(9)).unwrap_err();
        assert_eq!(code, ErrorCode::BAL_NOT_COVERED);
        assert!(cache.error(code).to_string().contains("not found in BAL"));

        // An account entirely absent from the BAL also errors.
        let missing = Address::with_last_byte(2);
        let code = cache.get_account(&missing).unwrap_err();
        assert_eq!(code, ErrorCode::BAL_NOT_COVERED);
    }

    #[test]
    fn uncovered_read_falls_back_to_database_when_allowed() {
        let address = Address::with_last_byte(1);
        let mut cache = cache_with_read_bal(address, true);

        // Uncovered slot falls through to the database value instead of erroring.
        assert_eq!(cache.get_storage(&address, &Word::from(9)).unwrap(), Word::from(9));

        // Covered slot is still served from the BAL.
        assert_eq!(cache.get_storage(&address, &Word::from(7)).unwrap(), Word::from(42));

        // Account absent from the BAL falls back to the database account.
        let missing = Address::with_last_byte(2);
        let account = cache.get_account(&missing).unwrap().unwrap();
        assert_eq!(account.balance, Word::from(100));
    }

    #[test]
    fn sink_visit_folds_changes_into_bal_builder() {
        let address = Address::with_last_byte(1);
        let read_address = Address::with_last_byte(2);

        let mut cache = InMemoryDB::default();
        cache.bal_context.enable_bal_builder();
        cache.bal_context.set_bal_index(BlockAccessIndex::new(3));

        // One changed account with a changed slot and a loaded-but-unchanged slot, plus one
        // loaded-but-unchanged account.
        let mut pending = PendingState::default();
        pending.insert_account(
            address,
            None,
            Some(AccountInfo::default().with_nonce(1).with_balance(Word::from(100))),
        );
        pending.insert_storage(address, Word::from(5), Word::ZERO, Word::from(42));
        pending.insert_storage(address, Word::from(6), Word::from(7), Word::from(7));
        pending.insert_account(
            read_address,
            Some(AccountInfo::default()),
            Some(AccountInfo::default()),
        );

        cache.commit_source(&pending);

        // The visit applied the changes to the cache...
        assert_eq!(cache.account_info(&address).unwrap().nonce, 1);
        assert_eq!(cache.cache.storage[&address].slots[&Word::from(5)], Word::from(42));

        // ...and folded the same writes and reads into the BAL builder.
        let bal = cache.bal_context.take_bal_builder().unwrap();
        let account = bal.accounts.get(&address).unwrap();
        assert_eq!(
            account.account_info.nonce.changes,
            vec![NonceChange::new(BlockAccessIndex::new(3), 1)]
        );
        assert_eq!(
            account.account_info.balance.changes,
            vec![BalanceChange::new(BlockAccessIndex::new(3), Word::from(100))]
        );
        assert_eq!(
            account.storage.storage.get(&Word::from(5)).unwrap().changes,
            vec![StorageChange::new(BlockAccessIndex::new(3), Word::from(42))]
        );
        // Loaded-but-unchanged entries surface as reads: present, with no writes.
        assert!(account.storage.storage.get(&Word::from(6)).unwrap().is_empty());
        let read_account = bal.accounts.get(&read_address).unwrap();
        assert!(read_account.account_info.nonce.is_empty());
        assert!(read_account.account_info.balance.is_empty());
        assert!(read_account.storage.storage.is_empty());
    }

    #[test]
    fn no_attached_bal_reads_straight_from_database() {
        let address = Address::with_last_byte(1);
        let mut cache = counting_cache();

        let account = cache.get_account(&address).unwrap().unwrap();
        assert_eq!(account.balance, Word::from(100));
        assert_eq!(cache.get_storage(&address, &Word::from(7)).unwrap(), Word::from(9));
    }
}
