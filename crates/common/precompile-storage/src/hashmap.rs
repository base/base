use alloy_primitives::{Address, LogData, U256};
use revm::{
    context::journaled_state::JournalCheckpoint,
    state::{AccountInfo, Bytecode},
};
use std::collections::HashMap;

use crate::{error::BasePrecompileError, provider::PrecompileStorageProvider};

/// In-memory [`PrecompileStorageProvider`] for unit tests.
///
/// Stores all state in `HashMap`s, avoiding the need for a real EVM context.
#[derive(Debug)]
pub struct HashMapStorageProvider {
    internals: HashMap<(Address, U256), U256>,
    transient: HashMap<(Address, U256), U256>,
    accounts: HashMap<Address, AccountInfo>,
    fail_on_sload: Option<(Address, U256)>,
    chain_id: u64,
    timestamp: U256,
    beneficiary: Address,
    block_number: u64,
    is_static: bool,
    counter_sload: u64,
    counter_sstore: u64,
    snapshots: Vec<Snapshot>,
    /// Emitted events keyed by contract address.
    pub events: HashMap<Address, Vec<LogData>>,
}

#[derive(Debug)]
struct Snapshot {
    internals: HashMap<(Address, U256), U256>,
    events: HashMap<Address, Vec<LogData>>,
}

impl HashMapStorageProvider {
    /// Creates a new provider with the given chain ID.
    pub fn new(chain_id: u64) -> Self {
        Self {
            internals: HashMap::new(),
            transient: HashMap::new(),
            accounts: HashMap::new(),
            fail_on_sload: None,
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
            beneficiary: Address::ZERO,
            block_number: 0,
            is_static: false,
            counter_sload: 0,
            counter_sstore: 0,
        }
    }
}

impl PrecompileStorageProvider for HashMapStorageProvider {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.timestamp
    }

    fn beneficiary(&self) -> Address {
        self.beneficiary
    }

    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), BasePrecompileError> {
        let account = self.accounts.entry(address).or_default();
        account.code_hash = code.hash_slow();
        account.code = Some(code);
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
        self.counter_sstore += 1;
        self.internals.insert((address, key), value);
        Ok(())
    }

    fn tstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), BasePrecompileError> {
        self.transient.insert((address, key), value);
        Ok(())
    }

    fn emit_event(
        &mut self,
        address: Address,
        event: LogData,
    ) -> Result<(), BasePrecompileError> {
        self.events.entry(address).or_default().push(event);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        if self.fail_on_sload == Some((address, key)) {
            return Err(BasePrecompileError::Fatal("injected sload failure".into()));
        }
        self.counter_sload += 1;
        Ok(self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        Ok(self.transient.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn deduct_gas(&mut self, _gas: u64) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn refund_gas(&mut self, _gas: i64) {}

    fn gas_limit(&self) -> u64 {
        0
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        0
    }

    fn is_static(&self) -> bool {
        self.is_static
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        let idx = self.snapshots.len();
        self.snapshots.push(Snapshot {
            internals: self.internals.clone(),
            events: self.events.clone(),
        });
        JournalCheckpoint { log_i: 0, journal_i: idx }
    }

    fn checkpoint_commit(&mut self, checkpoint: JournalCheckpoint) {
        assert_eq!(
            checkpoint.journal_i,
            self.snapshots.len() - 1,
            "out-of-order checkpoint commit (expected top of stack)"
        );
        self.snapshots.pop();
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        assert_eq!(
            checkpoint.journal_i,
            self.snapshots.len() - 1,
            "out-of-order checkpoint revert (expected top of stack)"
        );
        if let Some(snapshot) = self.snapshots.drain(checkpoint.journal_i..).next() {
            self.internals = snapshot.internals;
            self.events = snapshot.events;
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl HashMapStorageProvider {
    pub fn fail_next_sload_at(&mut self, address: Address, slot: U256) {
        self.fail_on_sload = Some((address, slot));
    }

    pub fn get_account_info(&self, address: Address) -> Option<&AccountInfo> {
        self.accounts.get(&address)
    }

    pub fn get_events(&self, address: Address) -> &Vec<LogData> {
        static EMPTY: Vec<LogData> = Vec::new();
        self.events.get(&address).unwrap_or(&EMPTY)
    }

    pub fn set_nonce(&mut self, address: Address, nonce: u64) {
        let account = self.accounts.entry(address).or_default();
        account.nonce = nonce;
    }

    pub fn set_timestamp(&mut self, timestamp: U256) {
        self.timestamp = timestamp;
    }

    pub fn set_beneficiary(&mut self, beneficiary: Address) {
        self.beneficiary = beneficiary;
    }

    pub fn set_block_number(&mut self, block_number: u64) {
        self.block_number = block_number;
    }

    pub fn clear_transient(&mut self) {
        self.transient.clear();
    }

    pub fn clear_events(&mut self, address: Address) {
        let _ = self.events.entry(address).and_modify(|v| v.clear()).or_default();
    }

    pub fn counter_sload(&self) -> u64 {
        self.counter_sload
    }

    pub fn counter_sstore(&self) -> u64 {
        self.counter_sstore
    }

    pub fn reset_counters(&mut self) {
        self.counter_sload = 0;
        self.counter_sstore = 0;
    }

    pub fn into_storage(self) -> impl Iterator<Item = (Address, U256, U256)> {
        self.internals
            .into_iter()
            .map(|((addr, slot), value)| (addr, slot, value))
    }

    pub fn sload_direct(&self, address: Address, key: U256) -> U256 {
        self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO)
    }
}

/// Test helper: returns a fresh `(HashMapStorageProvider, precompile_address)` pair.
#[cfg(any(test, feature = "test-utils"))]
pub fn setup_storage() -> (HashMapStorageProvider, Address) {
    (HashMapStorageProvider::new(1), Address::from([0x42u8; 20]))
}
