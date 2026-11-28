//! Test-only storage pieces: the in-memory [`HashMapStorageProvider`], [`StorageCtx`] accessors
//! for its state, and [`StorageLayout::erc7201_root`] for checking hard-coded namespace roots.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, LogData, U256, keccak256};

use super::{
    AccountInfo, BasePrecompileError, Checkpoint, GasSchedule, PrecompileStorageProvider,
    StorageCtx, StorageFeatures, StorageLayout,
};

/// In-memory [`PrecompileStorageProvider`] for unit tests.
///
/// Stores all state in `HashMap`s, avoiding the need for a real EVM context.
#[derive(Debug)]
pub struct HashMapStorageProvider {
    internals: HashMap<(Address, U256), U256>,
    transient: HashMap<(Address, U256), U256>,
    accounts: HashMap<Address, AccountInfo>,
    chain_id: u64,
    timestamp: U256,
    origin: Address,
    caller: Address,
    call_value: U256,
    is_static: bool,
    counter_sload: u64,
    counter_sstore: u64,
    gas_deducted: u64,
    counter_keccak256: u64,
    snapshots: Vec<Snapshot>,
    gas_refunded: i64,
    storage_features: StorageFeatures,
    /// Emitted events keyed by contract address.
    pub events: HashMap<Address, Vec<LogData>>,
}

#[derive(Debug)]
struct Snapshot {
    internals: HashMap<(Address, U256), U256>,
    transient: HashMap<(Address, U256), U256>,
    accounts: HashMap<Address, AccountInfo>,
    events: HashMap<Address, Vec<LogData>>,
    gas_refunded: i64,
}

impl HashMapStorageProvider {
    /// Creates a new provider with the given chain ID.
    pub fn new(chain_id: u64) -> Self {
        Self {
            internals: HashMap::new(),
            transient: HashMap::new(),
            accounts: HashMap::new(),
            events: HashMap::new(),
            snapshots: Vec::new(),
            chain_id,
            #[allow(clippy::disallowed_methods)]
            timestamp: U256::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
            origin: Address::ZERO,
            caller: Address::ZERO,
            call_value: U256::ZERO,
            is_static: false,
            counter_sload: 0,
            counter_sstore: 0,
            gas_deducted: 0,
            counter_keccak256: 0,
            gas_refunded: 0,
            storage_features: StorageFeatures::Legacy,
        }
    }

    /// Creates a new provider with the given chain ID and persistent-storage features, so a test
    /// can exercise a specific fork's `SLOAD`/tail-cleanup behavior instead of the `Legacy`
    /// default. Mirrors the EVM provider's `new_with_storage_features`.
    pub fn new_with_storage_features(chain_id: u64, storage_features: StorageFeatures) -> Self {
        let mut provider = Self::new(chain_id);
        provider.storage_features = storage_features;
        provider
    }
}

impl PrecompileStorageProvider for HashMapStorageProvider {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.timestamp
    }

    fn origin(&self) -> Address {
        self.origin
    }

    fn set_code(&mut self, address: Address, code: Bytes) -> Result<(), BasePrecompileError> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }

        let code_len = code.len();
        self.deduct_gas(GasSchedule::CODE_DEPOSIT.saturating_mul(code_len as u64))?;

        let is_new_code = self.accounts.get(&address).is_none_or(|info| info.is_empty_code_hash());
        if is_new_code {
            self.deduct_gas(GasSchedule::CREATE)?;
            self.deduct_gas(GasSchedule::keccak256(code_len))?;
        }

        let account = self.accounts.entry(address).or_default();
        account.code_hash = if code.is_empty() { KECCAK256_EMPTY } else { keccak256(&code) };
        Ok(())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<(), BasePrecompileError> {
        let account = self.accounts.entry(address).or_default();
        f(&*account);
        Ok(())
    }

    fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), BasePrecompileError> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        let old = self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO);
        self.counter_sstore += 1;
        self.internals.insert((address, key), value);
        // Simplified EIP-3529 refund: clearing a previously set slot earns a refund.
        // Exact EIP-2200/3529 accounting requires original-value tracking; the test provider
        // approximates with the post-London clear-slot refund to keep the mock simple.
        if !old.is_zero() && value.is_zero() {
            self.refund_gas(4_800);
        }
        Ok(())
    }

    fn tstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), BasePrecompileError> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        self.transient.insert((address, key), value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<(), BasePrecompileError> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        self.events.entry(address).or_default().push(event);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        self.counter_sload += 1;
        Ok(self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn tload_unmetered(
        &mut self,
        address: Address,
        key: U256,
    ) -> Result<U256, BasePrecompileError> {
        Ok(self.transient.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<(), BasePrecompileError> {
        self.gas_deducted = self.gas_deducted.saturating_add(gas);
        Ok(())
    }

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256, BasePrecompileError> {
        self.counter_keccak256 += 1;
        Ok(keccak256(data))
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        self.gas_refunded
    }

    fn storage_features(&self) -> StorageFeatures {
        self.storage_features
    }

    fn is_static(&self) -> bool {
        self.is_static
    }

    fn call_value(&self) -> U256 {
        self.call_value
    }

    fn caller(&self) -> alloy_primitives::Address {
        self.caller
    }

    fn replace_caller(&mut self, caller: Address) -> Address {
        core::mem::replace(&mut self.caller, caller)
    }

    fn checkpoint(&mut self) -> Checkpoint {
        let idx = self.snapshots.len();
        self.snapshots.push(Snapshot {
            internals: self.internals.clone(),
            transient: self.transient.clone(),
            accounts: self.accounts.clone(),
            events: self.events.clone(),
            gas_refunded: self.gas_refunded,
        });
        Checkpoint { journal_len: idx, logs_len: 0 }
    }

    fn commit_latest_checkpoint(&mut self) {
        assert!(!self.snapshots.is_empty(), "checkpoint_commit called with no active checkpoint");
        self.snapshots.pop();
    }

    fn assert_latest_checkpoint(&self, checkpoint: Checkpoint) {
        assert!(!self.snapshots.is_empty(), "checkpoint_commit called with no active checkpoint");
        assert_eq!(
            checkpoint.journal_len,
            self.snapshots.len() - 1,
            "out-of-order checkpoint commit (expected top of stack)"
        );
    }

    fn checkpoint_revert(&mut self, checkpoint: Checkpoint) {
        assert_eq!(
            checkpoint.journal_len,
            self.snapshots.len() - 1,
            "out-of-order checkpoint revert (expected top of stack)"
        );
        if let Some(snapshot) = self.snapshots.drain(checkpoint.journal_len..).next() {
            self.internals = snapshot.internals;
            self.transient = snapshot.transient;
            self.accounts = snapshot.accounts;
            self.events = snapshot.events;
            self.gas_refunded = snapshot.gas_refunded;
        }
    }
}

impl HashMapStorageProvider {
    /// Adds `gas` to the refund counter.
    pub const fn refund_gas(&mut self, gas: i64) {
        self.gas_refunded = self.gas_refunded.saturating_add(gas);
    }

    /// Returns account info for the given address (test-utils only).
    pub fn get_account_info(&self, address: Address) -> Option<&AccountInfo> {
        self.accounts.get(&address)
    }

    /// Returns emitted events for the given address (test-utils only).
    pub fn get_events(&self, address: Address) -> &Vec<LogData> {
        static EMPTY: Vec<LogData> = Vec::new();
        self.events.get(&address).unwrap_or(&EMPTY)
    }

    /// Sets the balance for the given address (test-utils only).
    pub fn set_balance(&mut self, address: Address, balance: U256) {
        let account = self.accounts.entry(address).or_default();
        account.balance = balance;
    }

    /// Sets the nonce for the given address (test-utils only).
    pub fn set_nonce(&mut self, address: Address, nonce: u64) {
        let account = self.accounts.entry(address).or_default();
        account.nonce = nonce;
    }

    /// Overrides the block timestamp (test-utils only).
    pub const fn set_timestamp(&mut self, timestamp: U256) {
        self.timestamp = timestamp;
    }

    /// Sets the caller address (test-utils only).
    pub const fn set_caller(&mut self, caller: Address) {
        self.caller = caller;
    }

    /// Sets the native call value in wei (test-utils only).
    pub const fn set_call_value(&mut self, value: U256) {
        self.call_value = value;
    }

    /// Overrides the transaction origin (`tx.origin`) (test-utils only).
    pub const fn set_origin(&mut self, origin: Address) {
        self.origin = origin;
    }

    /// Sets whether the current call is static (test-utils only).
    pub const fn set_static(&mut self, is_static: bool) {
        self.is_static = is_static;
    }

    /// Clears all transient storage (test-utils only).
    pub fn clear_transient(&mut self) {
        self.transient.clear();
    }

    /// Clears emitted events for the given address (test-utils only).
    pub fn clear_events(&mut self, address: Address) {
        let _ = self.events.entry(address).and_modify(|v| v.clear()).or_default();
    }

    /// Returns the SLOAD counter (test-utils only).
    pub const fn counter_sload(&self) -> u64 {
        self.counter_sload
    }

    /// Returns the SSTORE counter (test-utils only).
    pub const fn counter_sstore(&self) -> u64 {
        self.counter_sstore
    }

    /// Returns the total gas deducted via [`PrecompileStorageProvider::deduct_gas`] (test-utils only).
    pub const fn gas_deducted(&self) -> u64 {
        self.gas_deducted
    }

    /// Returns the number of times [`PrecompileStorageProvider::metered_keccak256`] was called.
    pub const fn counter_keccak256(&self) -> u64 {
        self.counter_keccak256
    }

    /// Resets the SLOAD/SSTORE/keccak256 counters (test-utils only).
    pub const fn reset_counters(&mut self) {
        self.counter_sload = 0;
        self.counter_sstore = 0;
        self.counter_keccak256 = 0;
    }

    /// Returns an iterator over all stored (address, slot, value) triples (test-utils only).
    pub fn into_storage(self) -> impl Iterator<Item = (Address, U256, U256)> {
        self.internals.into_iter().map(|((addr, slot), value)| (addr, slot, value))
    }

    /// Overrides persistent-storage features (test-utils only).
    pub const fn set_storage_features(&mut self, features: StorageFeatures) {
        self.storage_features = features;
    }
}

/// Test helper: returns a fresh `(HashMapStorageProvider, precompile_address)` pair.
pub fn setup_storage() -> (HashMapStorageProvider, Address) {
    (HashMapStorageProvider::new(1), Address::from([0x42u8; 20]))
}

impl StorageCtx<'_> {
    fn with_hashmap<R>(&self, f: impl FnOnce(&mut HashMapStorageProvider) -> R) -> R {
        let mut guard = self.storage.borrow_mut();
        // SAFETY: Test-utils code always uses `HashMapStorageProvider`. The borrow
        // guard stays alive for the full callback, preserving `RefCell` borrow checks.
        let provider = unsafe {
            &mut *(&mut **guard as *mut dyn PrecompileStorageProvider
                as *mut HashMapStorageProvider)
        };
        f(provider)
    }

    /// Executes a closure with account info from the test storage provider.
    pub fn with_test_account_info<T>(
        &self,
        address: Address,
        f: impl FnOnce(Option<&AccountInfo>) -> T,
    ) -> T {
        self.with_hashmap(|storage| {
            let info = storage.get_account_info(address).cloned();
            f(info.as_ref())
        })
    }

    /// Executes a closure with emitted events from the test storage provider.
    pub fn with_events<T>(&self, address: Address, f: impl FnOnce(&[LogData]) -> T) -> T {
        self.with_hashmap(|storage| {
            let events = storage.get_events(address);
            f(events)
        })
    }

    /// Returns account info for the given address (test-utils only).
    pub fn get_account_info(&self, address: Address) -> Option<AccountInfo> {
        self.with_test_account_info(address, |account| account.cloned())
    }

    /// Returns emitted events for the given address (test-utils only).
    pub fn get_events(&self, address: Address) -> Vec<LogData> {
        self.with_events(address, <[LogData]>::to_vec)
    }

    /// Sets the nonce for the given address (test-utils only).
    pub fn set_nonce(&self, address: Address, nonce: u64) {
        self.with_hashmap(|storage| storage.set_nonce(address, nonce))
    }

    /// Overrides the block timestamp (test-utils only).
    pub fn set_timestamp(&self, timestamp: U256) {
        self.with_hashmap(|storage| storage.set_timestamp(timestamp))
    }

    /// Clears all transient storage (test-utils only).
    pub fn clear_transient(&self) {
        self.with_hashmap(HashMapStorageProvider::clear_transient)
    }
    /// Clears emitted events for the given address (test-utils only).
    pub fn clear_events(&self, address: Address) {
        self.with_hashmap(|storage| storage.clear_events(address));
    }
    /// Returns the SLOAD counter (test-utils only).
    pub fn counter_sload(&self) -> u64 {
        self.with_hashmap(|storage| storage.counter_sload())
    }
    /// Returns the SSTORE counter (test-utils only).
    pub fn counter_sstore(&self) -> u64 {
        self.with_hashmap(|storage| storage.counter_sstore())
    }
    /// Resets the SLOAD/SSTORE counters (test-utils only).
    pub fn reset_counters(&self) {
        self.with_hashmap(HashMapStorageProvider::reset_counters)
    }
}

impl StorageLayout {
    /// Returns the ERC-7201 root of namespace `id`: `keccak256(keccak256(id) - 1) & ~0xff`.
    pub fn erc7201_root(id: &str) -> U256 {
        let inner = U256::from_be_bytes(keccak256(id.as_bytes()).0) - U256::ONE;
        U256::from_be_bytes(keccak256(inner.to_be_bytes::<32>()).0) & !U256::from(0xff)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::*;

    const ADDR: Address = Address::ZERO;
    const KEY: U256 = U256::ZERO;

    #[test]
    fn set_code_static_violation_before_gas_charge() {
        let mut p = HashMapStorageProvider::new(1);
        p.set_static(true);
        let code = Bytes::from_static(&[0x60, 0x00]);

        assert_eq!(p.set_code(Address::ZERO, code), Err(BasePrecompileError::StaticCallViolation),);
        // No gas must have been charged.
        assert_eq!(p.gas_deducted(), 0);
    }

    #[test]
    fn refund_gas_accumulates_positive() {
        let mut p = HashMapStorageProvider::new(1);
        p.refund_gas(1_000);
        p.refund_gas(500);
        assert_eq!(p.gas_refunded(), 1_500);
    }

    #[test]
    fn refund_gas_accumulates_negative() {
        let mut p = HashMapStorageProvider::new(1);
        p.refund_gas(4_800);
        p.refund_gas(-4_800);
        assert_eq!(p.gas_refunded(), 0);
    }

    #[test]
    fn refund_gas_starts_at_zero() {
        let p = HashMapStorageProvider::new(1);
        assert_eq!(p.gas_refunded(), 0);
    }

    #[test]
    fn sstore_clearing_slot_generates_refund() {
        let mut p = HashMapStorageProvider::new(1);
        // Write a non-zero value first.
        p.sstore(ADDR, KEY, U256::from(42u64)).unwrap();
        assert_eq!(p.gas_refunded(), 0, "writing non-zero to zero slot earns no refund");
        // Clear it — should earn EIP-3529 refund.
        p.sstore(ADDR, KEY, U256::ZERO).unwrap();
        assert!(p.gas_refunded() > 0, "clearing a non-zero slot must earn a refund");
    }

    #[test]
    fn sstore_nonzero_to_nonzero_earns_no_refund() {
        let mut p = HashMapStorageProvider::new(1);
        p.sstore(ADDR, KEY, U256::from(1u64)).unwrap();
        p.sstore(ADDR, KEY, U256::from(2u64)).unwrap();
        assert_eq!(p.gas_refunded(), 0);
    }

    #[test]
    fn sstore_zero_to_nonzero_earns_no_refund() {
        let mut p = HashMapStorageProvider::new(1);
        p.sstore(ADDR, KEY, U256::from(99u64)).unwrap();
        assert_eq!(p.gas_refunded(), 0);
    }

    #[test]
    fn checkpoint_revert_restores_internals() {
        let mut p = HashMapStorageProvider::new(1);
        p.sstore(ADDR, KEY, U256::from(1u64)).unwrap();
        let cp = p.checkpoint();
        p.sstore(ADDR, KEY, U256::from(2u64)).unwrap();
        p.checkpoint_revert(cp);
        assert_eq!(p.sload(ADDR, KEY).unwrap(), U256::from(1u64));
    }

    #[test]
    fn checkpoint_revert_restores_transient() {
        let mut p = HashMapStorageProvider::new(1);
        p.tstore(ADDR, KEY, U256::from(10u64)).unwrap();
        let cp = p.checkpoint();
        p.tstore(ADDR, KEY, U256::from(99u64)).unwrap();
        p.checkpoint_revert(cp);
        assert_eq!(p.tload_unmetered(ADDR, KEY).unwrap(), U256::from(10u64));
    }

    #[test]
    fn checkpoint_revert_restores_accounts() {
        let mut p = HashMapStorageProvider::new(1);
        let code_before = Bytes::from_static(&[0x60, 0x00]);
        p.set_code(ADDR, code_before.clone()).unwrap();
        let cp = p.checkpoint();
        let code_after = Bytes::from_static(&[0x60, 0x01]);
        p.set_code(ADDR, code_after).unwrap();
        p.checkpoint_revert(cp);
        let mut seen_hash = None;
        p.with_account_info(ADDR, &mut |info| {
            seen_hash = Some(info.code_hash);
        })
        .unwrap();
        assert_eq!(seen_hash.unwrap(), keccak256(&code_before));
    }

    #[test]
    fn checkpoint_revert_restores_events() {
        let mut p = HashMapStorageProvider::new(1);
        let event_before = LogData::new_unchecked(vec![], Bytes::from_static(b"before"));
        p.emit_event(ADDR, event_before).unwrap();
        let cp = p.checkpoint();
        let event_after = LogData::new_unchecked(vec![], Bytes::from_static(b"after"));
        p.emit_event(ADDR, event_after).unwrap();
        p.checkpoint_revert(cp);
        let events = p.get_events(ADDR);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, Bytes::from_static(b"before"));
    }

    #[test]
    fn checkpoint_revert_restores_gas_refunded() {
        let mut p = HashMapStorageProvider::new(1);
        p.refund_gas(1_000);
        let cp = p.checkpoint();
        p.refund_gas(500);
        assert_eq!(p.gas_refunded(), 1_500);
        p.checkpoint_revert(cp);
        assert_eq!(p.gas_refunded(), 1_000);
    }

    #[test]
    fn commit_latest_checkpoint_does_not_revert_mutations() {
        let mut p = HashMapStorageProvider::new(1);
        p.checkpoint();
        p.sstore(ADDR, KEY, U256::from(42u64)).unwrap();
        p.commit_latest_checkpoint();
        assert_eq!(p.sload(ADDR, KEY).unwrap(), U256::from(42u64));
    }

    #[test]
    #[should_panic(expected = "out-of-order checkpoint commit (expected top of stack)")]
    fn assert_latest_checkpoint_rejects_out_of_order_checkpoint() {
        let mut p = HashMapStorageProvider::new(1);
        let outer = p.checkpoint();
        p.checkpoint();

        p.assert_latest_checkpoint(outer);
    }

    #[test]
    fn new_with_storage_features_sets_the_requested_features() {
        assert_eq!(
            HashMapStorageProvider::new(1).storage_features(),
            StorageFeatures::Legacy,
            "plain constructor keeps the Legacy default",
        );
        assert_eq!(
            HashMapStorageProvider::new_with_storage_features(1, StorageFeatures::Cobalt)
                .storage_features(),
            StorageFeatures::Cobalt,
        );
    }
}
