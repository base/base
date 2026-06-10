use std::collections::HashMap;

use alloy_primitives::{Address, LogData, U256};
use revm::{
    context::journaled_state::JournalCheckpoint,
    context_interface::cfg::GasParams,
    interpreter::gas::{KECCAK256, KECCAK256WORD},
    state::{AccountInfo, Bytecode},
};

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
    caller: Address,
    call_value: U256,
    is_static: bool,
    counter_sload: u64,
    counter_sstore: u64,
    gas_deducted: u64,
    counter_keccak256: u64,
    snapshots: Vec<Snapshot>,
    gas_params: GasParams,
    state_gas_used: u64,
    gas_refunded: i64,
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
            caller: Address::ZERO,
            call_value: U256::ZERO,
            is_static: false,
            counter_sload: 0,
            counter_sstore: 0,
            gas_deducted: 0,
            counter_keccak256: 0,
            gas_params: GasParams::default(),
            state_gas_used: 0,
            gas_refunded: 0,
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
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }

        let code_len = code.len();
        self.deduct_gas(self.gas_params.code_deposit_cost(code_len))?;

        let is_new_code = self.accounts.get(&address).is_none_or(|info| info.is_empty_code_hash());
        if is_new_code {
            self.deduct_gas(self.gas_params.create_cost())?;
            let num_words = code_len.div_ceil(32) as u64;
            self.deduct_gas(KECCAK256.saturating_add(KECCAK256WORD.saturating_mul(num_words)))?;
        }

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
        if self.fail_on_sload == Some((address, key)) {
            return Err(BasePrecompileError::Fatal("injected sload failure".into()));
        }
        self.counter_sload += 1;
        Ok(self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        Ok(self.transient.get(&(address, key)).copied().unwrap_or(U256::ZERO))
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<(), BasePrecompileError> {
        self.gas_deducted = self.gas_deducted.saturating_add(gas);
        Ok(())
    }

    fn metered_keccak256(
        &mut self,
        data: &[u8],
    ) -> core::result::Result<alloy_primitives::B256, BasePrecompileError> {
        self.counter_keccak256 += 1;
        Ok(alloy_primitives::keccak256(data))
    }

    fn deduct_state_gas(&mut self, gas: u64) -> Result<(), BasePrecompileError> {
        // No gas limit in the test provider; just track the cumulative amount.
        self.state_gas_used = self.state_gas_used.saturating_add(gas);
        Ok(())
    }

    fn refund_gas(&mut self, gas: i64) {
        self.gas_refunded = self.gas_refunded.saturating_add(gas);
    }

    fn gas_limit(&self) -> u64 {
        0
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn state_gas_used(&self) -> u64 {
        self.state_gas_used
    }

    fn gas_refunded(&self) -> i64 {
        self.gas_refunded
    }

    fn reservoir(&self) -> u64 {
        0
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

    fn checkpoint(&mut self) -> JournalCheckpoint {
        let idx = self.snapshots.len();
        self.snapshots
            .push(Snapshot { internals: self.internals.clone(), events: self.events.clone() });
        JournalCheckpoint { log_i: 0, journal_i: idx, selfdestructed_i: 0 }
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
    /// Injects an SLOAD failure at the given address and slot (test-utils only).
    pub const fn fail_next_sload_at(&mut self, address: Address, slot: U256) {
        self.fail_on_sload = Some((address, slot));
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

    /// Overrides the block beneficiary (test-utils only).
    pub const fn set_beneficiary(&mut self, beneficiary: Address) {
        self.beneficiary = beneficiary;
    }

    /// Overrides the block number (test-utils only).
    pub const fn set_block_number(&mut self, block_number: u64) {
        self.block_number = block_number;
    }

    /// Sets the caller address (test-utils only).
    pub const fn set_caller(&mut self, caller: Address) {
        self.caller = caller;
    }

    /// Sets the native call value in wei (test-utils only).
    pub const fn set_call_value(&mut self, value: U256) {
        self.call_value = value;
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

    /// Reads a storage slot directly without journal overhead (test-utils only).
    pub fn sload_direct(&self, address: Address, key: U256) -> U256 {
        self.internals.get(&(address, key)).copied().unwrap_or(U256::ZERO)
    }

    /// Overrides the gas parameters used for state gas accounting (test-utils only).
    pub fn set_gas_params(&mut self, gas_params: GasParams) {
        self.gas_params = gas_params;
    }
}

/// Test helper: returns a fresh `(HashMapStorageProvider, precompile_address)` pair.
#[cfg(any(test, feature = "test-utils"))]
pub fn setup_storage() -> (HashMapStorageProvider, Address) {
    (HashMapStorageProvider::new(1), Address::from([0x42u8; 20]))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::*;
    use crate::{error::BasePrecompileError, provider::PrecompileStorageProvider};

    const ADDR: Address = Address::ZERO;
    const KEY: U256 = U256::ZERO;

    #[test]
    fn set_code_static_violation_before_state_gas_charge() {
        let mut p = HashMapStorageProvider::new(1);
        p.set_static(true);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());

        assert_eq!(p.set_code(Address::ZERO, code), Err(BasePrecompileError::StaticCallViolation),);
        // No state gas must have been charged.
        assert_eq!(p.state_gas_used(), 0);
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
}
