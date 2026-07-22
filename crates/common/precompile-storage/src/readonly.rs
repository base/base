//! Read-only [`PrecompileStorageProvider`] over committed chain state.
//!
//! [`ReadOnlyStorage`] adapts an arbitrary [`StorageReader`] (a committed state
//! source that can `SLOAD` and read account info) into the
//! [`PrecompileStorageProvider`] boundary the `#[contract]` storage mirrors
//! require, without any EVM journal. Every mutating, gas, and checkpoint
//! operation is inert: writes report a static-call violation, gas accounting is
//! a no-op, and the context reports itself [static](PrecompileStorageProvider::is_static).
//!
//! This is the off-EVM read path used by RPC handlers and the builder to decode
//! system-contract storage (e.g. the payer config) against head/pending state
//! with a handful of `SLOAD`s and no execution.

use alloy_primitives::{Address, LogData, U256};
use revm::{
    context::journaled_state::JournalCheckpoint,
    state::{AccountInfo, Bytecode},
};

use crate::{
    error::{BasePrecompileError, Result},
    provider::PrecompileStorageProvider,
};

/// A source of committed chain state for read-only precompile-storage access.
///
/// Only [`sload`](Self::sload) is required; the account accessors default to an
/// empty account, which is sufficient for storage mirrors that never inspect
/// account info or code.
pub trait StorageReader {
    /// Reads the value at `key` in `address`'s persistent storage.
    fn sload(&self, address: Address, key: U256) -> Result<U256>;

    /// Reads `address`'s account info. Defaults to an empty account.
    fn account_info(&self, _address: Address) -> Result<AccountInfo> {
        Ok(AccountInfo::default())
    }

    /// Reads `address`'s deployed bytecode. Defaults to empty bytecode.
    fn account_code(&self, _address: Address) -> Result<Bytecode> {
        Ok(Bytecode::default())
    }
}

/// A read-only [`PrecompileStorageProvider`] backed by a [`StorageReader`].
///
/// Suitable for decoding contract storage against a fixed, committed state; any
/// attempt to mutate state returns [`BasePrecompileError::StaticCallViolation`].
#[derive(Debug)]
pub struct ReadOnlyStorage<R> {
    reader: R,
    chain_id: u64,
    timestamp: u64,
    caller: Address,
}

impl<R> ReadOnlyStorage<R> {
    /// Wraps `reader`, reporting `chain_id` and `timestamp` to the storage
    /// layer (the latter drives staleness checks in price snapshots).
    pub const fn new(reader: R, chain_id: u64, timestamp: u64) -> Self {
        Self { reader, chain_id, timestamp, caller: Address::ZERO }
    }
}

impl<R: StorageReader> PrecompileStorageProvider for ReadOnlyStorage<R> {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        U256::from(self.timestamp)
    }

    fn beneficiary(&self) -> Address {
        Address::ZERO
    }

    fn block_number(&self) -> u64 {
        0
    }

    fn origin(&self) -> Address {
        Address::ZERO
    }

    fn set_code(&mut self, _address: Address, _code: Bytecode) -> Result<()> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        let info = self.reader.account_info(address)?;
        f(&info);
        Ok(())
    }

    fn with_account_code(&mut self, address: Address, f: &mut dyn FnMut(&Bytecode)) -> Result<()> {
        let code = self.reader.account_code(address)?;
        f(&code);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.reader.sload(address, key)
    }

    fn tload(&mut self, _address: Address, _key: U256) -> Result<U256> {
        Ok(U256::ZERO)
    }

    fn sstore(&mut self, _address: Address, _key: U256, _value: U256) -> Result<()> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn tstore(&mut self, _address: Address, _key: U256, _value: U256) -> Result<()> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn emit_event(&mut self, _address: Address, _event: LogData) -> Result<()> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn deduct_gas(&mut self, _gas: u64) -> Result<()> {
        Ok(())
    }

    fn deduct_state_gas(&mut self, _gas: u64) -> Result<()> {
        Ok(())
    }

    fn refund_gas(&mut self, _gas: i64) {}

    fn gas_limit(&self) -> u64 {
        u64::MAX
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn state_gas_used(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        0
    }

    fn reservoir(&self) -> u64 {
        0
    }

    fn is_static(&self) -> bool {
        true
    }

    fn call_value(&self) -> U256 {
        U256::ZERO
    }

    fn caller(&self) -> Address {
        self.caller
    }

    fn replace_caller(&mut self, caller: Address) -> Address {
        core::mem::replace(&mut self.caller, caller)
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        JournalCheckpoint { log_i: 0, journal_i: 0, selfdestructed_i: 0 }
    }

    fn checkpoint_commit(&mut self) {}

    fn checkpoint_revert(&mut self, _checkpoint: JournalCheckpoint) {}
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use alloy_primitives::address;

    use super::*;
    use crate::storage_ctx::StorageCtx;

    struct MapReader(BTreeMap<(Address, U256), U256>);

    impl StorageReader for MapReader {
        fn sload(&self, address: Address, key: U256) -> Result<U256> {
            Ok(self.0.get(&(address, key)).copied().unwrap_or(U256::ZERO))
        }
    }

    const CONTRACT: Address = address!("0x0000000000000000000000000000000000000042");

    #[test]
    fn sload_reads_through_and_missing_is_zero() {
        let mut map = BTreeMap::new();
        map.insert((CONTRACT, U256::from(1u8)), U256::from(7u8));
        let mut storage = ReadOnlyStorage::new(MapReader(map), 8453, 1_000);

        assert_eq!(storage.sload(CONTRACT, U256::from(1u8)).unwrap(), U256::from(7u8));
        assert_eq!(storage.sload(CONTRACT, U256::from(2u8)).unwrap(), U256::ZERO);
        assert_eq!(storage.chain_id(), 8453);
        assert_eq!(storage.timestamp(), U256::from(1_000u64));
        assert!(storage.is_static());
    }

    #[test]
    fn writes_are_static_violations() {
        let mut storage = ReadOnlyStorage::new(MapReader(BTreeMap::new()), 8453, 0);
        assert!(matches!(
            storage.sstore(CONTRACT, U256::ZERO, U256::from(1u8)),
            Err(BasePrecompileError::StaticCallViolation)
        ));
    }

    #[test]
    fn usable_through_storage_ctx() {
        // A read-only context can be entered like any other provider.
        let mut storage = ReadOnlyStorage::new(MapReader(BTreeMap::new()), 8453, 0);
        let value = StorageCtx::enter(&mut storage, |ctx| ctx.sload(CONTRACT, U256::from(9u8)));
        assert_eq!(value.unwrap(), U256::ZERO);
    }
}
