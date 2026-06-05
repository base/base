//! Explicit storage context for Base native precompiles.
//!
//! [`StorageCtx`] is a zero-size token that provides access to the current
//! scoped [`PrecompileStorageProvider`]. All storage operations within a
//! precompile call receive a context from [`StorageCtx::enter`].

use alloc::string::ToString;
#[cfg(any(test, feature = "test-utils"))]
use alloc::vec::Vec;
use core::{cell::RefCell, fmt};

use alloy_primitives::{Address, B256, Bytes, LogData, U256};
use alloy_sol_types::SolInterface;
use revm::{
    context::journaled_state::JournalCheckpoint,
    precompile::{PrecompileOutput, PrecompileResult},
    state::{AccountInfo, Bytecode},
};

use crate::{
    error::{BasePrecompileError, Result},
    provider::PrecompileStorageProvider,
};

type ScopedProvider<'a> = dyn PrecompileStorageProvider + 'a;

/// Scoped handle providing access to the active [`PrecompileStorageProvider`].
///
/// Values of this type are created by [`StorageCtx::enter`] and cannot outlive
/// that closure.
#[derive(Clone, Copy)]
pub struct StorageCtx<'a> {
    storage: &'a RefCell<&'a mut ScopedProvider<'a>>,
}

impl fmt::Debug for StorageCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageCtx").finish_non_exhaustive()
    }
}

impl StorageCtx<'_> {
    /// Enter the storage context. All storage operations must happen within the closure.
    pub fn enter<S, R>(storage: &mut S, f: impl for<'ctx> FnOnce(StorageCtx<'ctx>) -> R) -> R
    where
        S: PrecompileStorageProvider,
    {
        let storage: &mut ScopedProvider<'_> = storage;
        let cell = RefCell::new(storage);
        f(StorageCtx { storage: &cell })
    }
}

impl<'a> StorageCtx<'a> {
    fn with_storage<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> R,
    {
        let mut guard = self.storage.borrow_mut();
        f(&mut **guard)
    }

    fn try_with_storage<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> Result<R>,
    {
        let mut guard = self.storage.try_borrow_mut().map_err(|_| {
            BasePrecompileError::Fatal("Storage context is already mutably borrowed".to_string())
        })?;
        f(&mut **guard)
    }

    // --- Provider method delegates ---

    /// Executes a closure with account info, returning the closure's result.
    pub fn with_account_info<T>(
        &self,
        address: Address,
        mut f: impl FnMut(&AccountInfo) -> Result<T>,
    ) -> Result<T> {
        let mut result: Option<Result<T>> = None;
        self.try_with_storage(|s| {
            s.with_account_info(address, &mut |info| {
                result = Some(f(info));
            })
        })?;
        result.unwrap_or_else(|| {
            Err(BasePrecompileError::Fatal(
                "with_account_info callback was not invoked".to_string(),
            ))
        })
    }

    /// Returns the current chain ID.
    pub fn chain_id(&self) -> u64 {
        self.with_storage(|s| s.chain_id())
    }
    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> U256 {
        self.with_storage(|s| s.timestamp())
    }
    /// Returns the block beneficiary (coinbase).
    pub fn beneficiary(&self) -> Address {
        self.with_storage(|s| s.beneficiary())
    }
    /// Returns the current block number.
    pub fn block_number(&self) -> u64 {
        self.with_storage(|s| s.block_number())
    }

    /// Sets the bytecode at the given address.
    pub fn set_code(&self, address: Address, code: Bytecode) -> Result<()> {
        self.try_with_storage(|s| s.set_code(address, code))
    }

    /// Performs an SLOAD (persistent storage read).
    pub fn sload(&self, address: Address, key: U256) -> Result<U256> {
        self.try_with_storage(|s| s.sload(address, key))
    }

    /// Performs a TLOAD (transient storage read).
    pub fn tload(&self, address: Address, key: U256) -> Result<U256> {
        self.try_with_storage(|s| s.tload(address, key))
    }

    /// Performs an SSTORE (persistent storage write).
    pub fn sstore(&self, address: Address, key: U256, value: U256) -> Result<()> {
        self.try_with_storage(|s| s.sstore(address, key, value))
    }

    /// Performs a TSTORE (transient storage write).
    pub fn tstore(&self, address: Address, key: U256, value: U256) -> Result<()> {
        self.try_with_storage(|s| s.tstore(address, key, value))
    }

    /// Emits an event from the given contract address.
    pub fn emit_event(&self, address: Address, event: LogData) -> Result<()> {
        self.try_with_storage(|s| s.emit_event(address, event))
    }

    /// Adds gas to the refund counter.
    pub fn refund_gas(&self, gas: i64) {
        self.with_storage(|s| s.refund_gas(gas))
    }
    /// Returns the gas limit for this precompile call.
    pub fn gas_limit(&self) -> u64 {
        self.with_storage(|s| s.gas_limit())
    }
    /// Returns the gas used so far.
    pub fn gas_used(&self) -> u64 {
        self.with_storage(|s| s.gas_used())
    }
    /// Returns the state-creating gas spent so far (EIP-8037).
    pub fn state_gas_used(&self) -> u64 {
        self.with_storage(|s| s.state_gas_used())
    }
    /// Returns the gas refunded so far.
    pub fn gas_refunded(&self) -> i64 {
        self.with_storage(|s| s.gas_refunded())
    }
    /// Returns the remaining EIP-8037 state-gas reservoir.
    pub fn reservoir(&self) -> u64 {
        self.with_storage(|s| s.reservoir())
    }
    /// Returns whether the current call context is static.
    pub fn is_static(&self) -> bool {
        self.with_storage(|s| s.is_static())
    }
    /// Returns the address that called this precompile.
    pub fn caller(&self) -> Address {
        self.with_storage(|s| s.caller())
    }

    /// Executes `f` with a temporary caller override, restoring the previous caller on exit.
    pub fn with_caller<R>(&self, caller: Address, f: impl FnOnce() -> R) -> R {
        let previous = self.with_storage(|s| s.replace_caller(caller));
        let guard = CallerGuard { storage: *self, previous: Some(previous) };
        let result = f();
        drop(guard);
        result
    }

    /// Deducts gas from the remaining gas, returning `OutOfGas` if insufficient.
    pub fn deduct_gas(&self, gas: u64) -> Result<()> {
        self.try_with_storage(|s| s.deduct_gas(gas))
    }

    /// Deducts state-creating gas, consuming the EIP-8037 reservoir first.
    pub fn deduct_state_gas(&self, gas: u64) -> Result<()> {
        self.try_with_storage(|s| s.deduct_state_gas(gas))
    }

    /// Computes keccak256 and charges the appropriate gas.
    pub fn keccak256(&self, data: &[u8]) -> Result<B256> {
        self.try_with_storage(|s| s.keccak256(data))
    }

    /// Creates a journal checkpoint and returns a RAII guard that auto-reverts on drop.
    pub fn checkpoint(&self) -> CheckpointGuard<'a> {
        let checkpoint = self.with_storage(|s| s.checkpoint());
        CheckpointGuard { storage: *self, checkpoint: Some(checkpoint) }
    }

    /// Returns a success [`PrecompileOutput`] with the current gas used and accumulated refund.
    ///
    /// The `gas_refunded` field is populated so revm's frame handler can propagate it to the
    /// transaction-level refund counter, where the EIP-3529 cap (`gas_used / 5`) is applied.
    pub fn success_output(&self, output: Bytes) -> PrecompileOutput {
        let mut out = PrecompileOutput::new(self.gas_used(), output, self.reservoir());
        out.gas_refunded = self.gas_refunded();
        out.state_gas_used = self.state_gas_used();
        out
    }

    /// Returns an ABI-encoded success output.
    pub fn abi_success(&self, output: impl SolInterface) -> PrecompileOutput {
        self.success_output(output.abi_encode().into())
    }

    /// Returns a revert [`PrecompileOutput`] with the current gas used.
    pub fn revert_output(&self, output: Bytes) -> PrecompileOutput {
        let mut out = PrecompileOutput::revert(self.gas_used(), output, self.reservoir());
        out.state_gas_used = self.state_gas_used();
        out
    }

    /// Reverts with an ABI-encoded error.
    pub fn abi_revert(&self, error: impl SolInterface) -> PrecompileOutput {
        self.revert_output(error.abi_encode().into())
    }

    /// Returns a [`PrecompileResult`] constructed from the given error.
    pub fn error_result(&self, error: impl Into<BasePrecompileError>) -> PrecompileResult {
        error.into().into_precompile_result(
            self.gas_used(),
            self.state_gas_used(),
            self.reservoir(),
        )
    }
}

/// RAII guard for temporary caller overrides.
#[derive(Debug)]
struct CallerGuard<'a> {
    storage: StorageCtx<'a>,
    previous: Option<Address>,
}

impl Drop for CallerGuard<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.storage.with_storage(|s| {
                s.replace_caller(previous);
            });
        }
    }
}

/// RAII guard for atomic state mutation batching.
///
/// On drop, automatically reverts all state changes made since the checkpoint
/// unless [`commit`](CheckpointGuard::commit) is called.
#[derive(Debug)]
pub struct CheckpointGuard<'a> {
    storage: StorageCtx<'a>,
    checkpoint: Option<JournalCheckpoint>,
}

impl CheckpointGuard<'_> {
    /// Commits all state changes since the checkpoint.
    pub fn commit(mut self) {
        if let Some(cp) = self.checkpoint.take() {
            self.storage.with_storage(|s| s.checkpoint_commit(cp));
        }
    }
}

impl Drop for CheckpointGuard<'_> {
    fn drop(&mut self) {
        if let Some(cp) = self.checkpoint.take() {
            self.storage.with_storage(|s| s.checkpoint_revert(cp));
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
use crate::hashmap::HashMapStorageProvider;

#[cfg(any(test, feature = "test-utils"))]
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
        self.with_hashmap(|storage| f(storage.get_account_info(address)))
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

    /// Overrides the block beneficiary (test-utils only).
    pub fn set_beneficiary(&self, beneficiary: Address) {
        self.with_hashmap(|storage| storage.set_beneficiary(beneficiary))
    }

    /// Overrides the block number (test-utils only).
    pub fn set_block_number(&self, block_number: u64) {
        self.with_hashmap(|storage| storage.set_block_number(block_number))
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

    /// Returns true if the contract at the given address has non-empty bytecode (test-utils only).
    pub fn has_bytecode(&self, address: Address) -> Result<bool> {
        self.with_account_info(address, |info| Ok(!info.is_empty_code_hash()))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn test_reentrant_with_storage_panics() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| ctx.with_storage(|_| ctx.with_storage(|_| ())));
    }

    #[test]
    fn test_with_caller_restores_previous_caller() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        let original = Address::repeat_byte(0x11);
        let outer = Address::repeat_byte(0x22);
        let inner = Address::repeat_byte(0x33);
        storage.set_caller(original);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_eq!(ctx.caller(), original);
            let value = ctx.with_caller(outer, || {
                assert_eq!(ctx.caller(), outer);
                ctx.with_caller(inner, || {
                    assert_eq!(ctx.caller(), inner);
                    7
                })
            });

            assert_eq!(value, 7);
            assert_eq!(ctx.caller(), original);
        });
    }

    #[test]
    fn success_output_carries_correct_reservoir_and_state_gas_used() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(1_000);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(400).unwrap(); // 400 from reservoir; reservoir = 600

            let out = ctx.success_output(Bytes::new());

            assert!(out.is_success());
            assert_eq!(
                out.state_gas_used, 400,
                "state_gas_used must equal total state gas charged"
            );
            assert_eq!(out.reservoir, 600, "reservoir must equal remaining reservoir");
        });
    }

    #[test]
    fn success_output_with_zero_reservoir_sets_fields_correctly() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        // reservoir = 0; all state gas spills into regular gas.

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(250).unwrap();

            let out = ctx.success_output(Bytes::new());

            assert!(out.is_success());
            assert_eq!(out.state_gas_used, 250);
            assert_eq!(out.reservoir, 0);
        });
    }

    #[test]
    fn success_output_no_state_gas_charged_leaves_reservoir_intact() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(800);

        StorageCtx::enter(&mut storage, |ctx| {
            let out = ctx.success_output(Bytes::new());

            assert!(out.is_success());
            assert_eq!(out.state_gas_used, 0);
            assert_eq!(out.reservoir, 800, "untouched reservoir must be returned in full");
        });
    }

    #[test]
    fn revert_output_carries_correct_reservoir_and_state_gas_used() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(500);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(150).unwrap(); // reservoir = 350

            let out = ctx.revert_output(Bytes::from("revert reason"));

            assert!(out.is_revert());
            assert_eq!(out.state_gas_used, 150);
            assert_eq!(out.reservoir, 350);
            assert_eq!(out.bytes, Bytes::from("revert reason"));
        });
    }

    #[test]
    fn revert_output_with_spillover_sets_fields_correctly() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(100);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(300).unwrap(); // 100 from reservoir, 200 spill; reservoir = 0

            let out = ctx.revert_output(Bytes::new());

            assert!(out.is_revert());
            assert_eq!(out.state_gas_used, 300);
            assert_eq!(out.reservoir, 0);
        });
    }

    #[test]
    fn error_result_revert_carries_state_gas_and_reservoir() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(600);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(200).unwrap(); // reservoir = 400

            let result = ctx.error_result(BasePrecompileError::Revert(Bytes::new()));
            let out = result.unwrap();

            assert!(out.is_revert());
            assert_eq!(out.state_gas_used, 200);
            assert_eq!(out.reservoir, 400);
        });
    }

    #[test]
    fn error_result_oog_carries_reservoir() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(999);

        StorageCtx::enter(&mut storage, |ctx| {
            let result = ctx.error_result(BasePrecompileError::OutOfGas);
            let out = result.unwrap();

            assert!(out.is_halt());
            // On OOG the remaining reservoir must be returned so the EVM can account for it.
            assert_eq!(out.reservoir, 999);
        });
    }

    #[test]
    fn success_output_carries_gas_refunded_alongside_new_fields() {
        // Verifies that gas_refunded is still set correctly alongside the new reservoir
        // and state_gas_used fields (regression guard for the output construction change).
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        storage.set_reservoir(500);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.deduct_state_gas(200).unwrap();

            let out = ctx.success_output(Bytes::new());

            assert!(out.is_success());
            assert_eq!(out.state_gas_used, 200);
            assert_eq!(out.reservoir, 300);
            assert_eq!(out.gas_refunded, 0); // HashMapStorageProvider always returns 0
        });
    }

    #[test]
    fn test_checkpoint_commit_and_revert() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        let addr = Address::ZERO;
        let key = U256::from(1);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.sstore(addr, key, U256::from(42)).unwrap();
            let guard = ctx.checkpoint();
            ctx.sstore(addr, key, U256::from(99)).unwrap();
            guard.commit();
            assert_eq!(ctx.sload(addr, key).unwrap(), U256::from(99));

            {
                let _guard = ctx.checkpoint();
                ctx.sstore(addr, key, U256::from(1)).unwrap();
            }
            assert_eq!(ctx.sload(addr, key).unwrap(), U256::from(99));
        });
    }
}
