//! Scoped storage context for Base native precompiles.
//!
//! [`StorageCtx`] is a zero-size token that provides access to the current
//! scoped [`PrecompileStorageProvider`]. All storage operations within
//! a precompile call must happen inside a [`StorageCtx::enter`] closure.
//!
//! In `std` builds, the active provider is thread-local. In `no_std` builds,
//! `core` does not provide thread-local storage, so the active provider is stored
//! as a scoped process-local pointer for the duration of [`StorageCtx::enter`].

use alloc::string::ToString;
#[cfg(any(test, feature = "test-utils"))]
use alloc::vec::Vec;
#[cfg(all(not(feature = "std"), target_has_atomic = "ptr"))]
use core::sync::atomic::{AtomicPtr, Ordering};
#[cfg(feature = "std")]
use core::{cell::RefCell, mem};
#[cfg(not(feature = "std"))]
use core::{
    cell::{Cell, UnsafeCell},
    ptr,
};

use alloy_primitives::{Address, B256, Bytes, LogData, U256};
use alloy_sol_types::SolInterface;
use revm::{
    context::journaled_state::JournalCheckpoint,
    precompile::{PrecompileOutput, PrecompileResult},
    state::{AccountInfo, Bytecode},
};
#[cfg(feature = "std")]
use scoped_tls::scoped_thread_local;

use crate::{
    error::{BasePrecompileError, Result},
    provider::PrecompileStorageProvider,
};

#[cfg(feature = "std")]
scoped_thread_local!(static STORAGE: RefCell<&mut dyn PrecompileStorageProvider>);
#[cfg(all(not(feature = "std"), target_has_atomic = "ptr"))]
static STORAGE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
#[cfg(all(not(feature = "std"), not(target_has_atomic = "ptr")))]
static mut STORAGE: *mut () = ptr::null_mut();

#[cfg(all(not(feature = "std"), target_has_atomic = "ptr"))]
fn storage_load() -> *mut () {
    STORAGE.load(Ordering::SeqCst)
}

#[cfg(all(not(feature = "std"), not(target_has_atomic = "ptr")))]
fn storage_load() -> *mut () {
    // SAFETY: `no_std` execution is scoped through `StorageCtx::enter`. Targets without
    // pointer atomics cannot support concurrent mutation here, so callers must not enter
    // the storage context concurrently.
    unsafe { STORAGE }
}

#[cfg(all(not(feature = "std"), target_has_atomic = "ptr"))]
fn storage_replace(ptr: *mut ()) -> *mut () {
    STORAGE.swap(ptr, Ordering::SeqCst)
}

#[cfg(all(not(feature = "std"), not(target_has_atomic = "ptr")))]
fn storage_replace(ptr: *mut ()) -> *mut () {
    // SAFETY: See `storage_load`.
    unsafe {
        let parent = STORAGE;
        STORAGE = ptr;
        parent
    }
}

#[cfg(all(not(feature = "std"), target_has_atomic = "ptr"))]
fn storage_store(ptr: *mut ()) {
    STORAGE.store(ptr, Ordering::SeqCst);
}

#[cfg(all(not(feature = "std"), not(target_has_atomic = "ptr")))]
fn storage_store(ptr: *mut ()) {
    // SAFETY: See `storage_load`.
    unsafe {
        STORAGE = ptr;
    }
}

#[cfg(not(feature = "std"))]
struct StorageScope<'a> {
    storage: UnsafeCell<&'a mut dyn PrecompileStorageProvider>,
    borrowed: Cell<bool>,
}

#[cfg(not(feature = "std"))]
struct StorageScopeGuard {
    parent: *mut (),
}

#[cfg(not(feature = "std"))]
impl Drop for StorageScopeGuard {
    fn drop(&mut self) {
        storage_store(self.parent);
    }
}

#[cfg(not(feature = "std"))]
struct StorageBorrowGuard<'a> {
    borrowed: &'a Cell<bool>,
}

#[cfg(not(feature = "std"))]
impl Drop for StorageBorrowGuard<'_> {
    fn drop(&mut self) {
        self.borrowed.set(false);
    }
}

/// Zero-size token providing access to the scoped [`PrecompileStorageProvider`].
///
/// Must be used within a [`StorageCtx::enter`] closure.
#[derive(Debug, Default, Clone, Copy)]
pub struct StorageCtx;

impl StorageCtx {
    /// Enter the storage context. All storage operations must happen within the closure.
    pub fn enter<S, R>(storage: &mut S, f: impl FnOnce() -> R) -> R
    where
        S: PrecompileStorageProvider,
    {
        #[cfg(feature = "std")]
        {
            let storage: &mut dyn PrecompileStorageProvider = storage;
            // SAFETY: `scoped_tls` ensures the pointer is only accessible within the closure scope.
            // The reference is erased to 'static, but scoped_tls guarantees it never escapes `f`.
            let storage_static: &mut (dyn PrecompileStorageProvider + 'static) =
                unsafe { mem::transmute(storage) };
            let cell = RefCell::new(storage_static);
            STORAGE.set(&cell, f)
        }

        #[cfg(not(feature = "std"))]
        {
            let storage: &mut dyn PrecompileStorageProvider = storage;
            let scope =
                StorageScope { storage: UnsafeCell::new(storage), borrowed: Cell::new(false) };
            let scope_ptr = (&scope as *const StorageScope<'_>).cast_mut().cast::<()>();
            let parent = storage_replace(scope_ptr);
            let _guard = StorageScopeGuard { parent };
            f()
        }
    }

    fn with_storage<F, R>(f: F) -> R
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> R,
    {
        #[cfg(feature = "std")]
        {
            assert!(
                STORAGE.is_set(),
                "No storage context. 'StorageCtx::enter' must be called first"
            );
            STORAGE.with(|cell| {
                let mut guard = cell.borrow_mut();
                f(&mut **guard)
            })
        }

        #[cfg(not(feature = "std"))]
        {
            let storage = Self::current_storage();
            let guard = Self::borrow_storage(storage);
            // SAFETY: `StorageScope::storage` points to the provider passed to the active
            // `StorageCtx::enter` call. The borrow guard prevents overlapping mutable access.
            let result = unsafe {
                let storage = &mut *storage.storage.get();
                f(&mut **storage)
            };
            drop(guard);
            result
        }
    }

    fn try_with_storage<F, R>(f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn PrecompileStorageProvider) -> Result<R>,
    {
        #[cfg(feature = "std")]
        {
            if !STORAGE.is_set() {
                return Err(BasePrecompileError::Fatal(
                    "No storage context. 'StorageCtx::enter' must be called first".to_string(),
                ));
            }
            STORAGE.with(|cell| {
                let mut guard = cell.borrow_mut();
                f(&mut **guard)
            })
        }

        #[cfg(not(feature = "std"))]
        {
            let Some(storage) = Self::try_current_storage() else {
                return Err(BasePrecompileError::Fatal(
                    "No storage context. 'StorageCtx::enter' must be called first".to_string(),
                ));
            };
            let guard = Self::borrow_storage(storage);
            // SAFETY: `StorageScope::storage` points to the provider passed to the active
            // `StorageCtx::enter` call. The borrow guard prevents overlapping mutable access.
            let result = unsafe {
                let storage = &mut *storage.storage.get();
                f(&mut **storage)
            };
            drop(guard);
            result
        }
    }

    #[cfg(not(feature = "std"))]
    fn try_current_storage() -> Option<&'static StorageScope<'static>> {
        let ptr = storage_load();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `StorageCtx::enter` stores a pointer to a stack-local `StorageScope` and
        // `StorageScopeGuard` restores the previous pointer before that stack frame exits.
        Some(unsafe { &*ptr.cast::<StorageScope<'static>>() })
    }

    #[cfg(not(feature = "std"))]
    fn current_storage() -> &'static StorageScope<'static> {
        Self::try_current_storage()
            .expect("No storage context. 'StorageCtx::enter' must be called first")
    }

    #[cfg(not(feature = "std"))]
    fn borrow_storage(storage: &'static StorageScope<'static>) -> StorageBorrowGuard<'static> {
        assert!(!storage.borrowed.replace(true), "already borrowed");
        StorageBorrowGuard { borrowed: &storage.borrowed }
    }

    // --- Provider method delegates ---

    /// Executes a closure with account info, returning the closure's result.
    pub fn with_account_info<T>(
        &self,
        address: Address,
        mut f: impl FnMut(&AccountInfo) -> Result<T>,
    ) -> Result<T> {
        let mut result: Option<Result<T>> = None;
        Self::try_with_storage(|s| {
            s.with_account_info(address, &mut |info| {
                result = Some(f(info));
            })
        })?;
        result.unwrap()
    }

    /// Returns the current chain ID.
    pub fn chain_id(&self) -> u64 {
        Self::with_storage(|s| s.chain_id())
    }
    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> U256 {
        Self::with_storage(|s| s.timestamp())
    }
    /// Returns the block beneficiary (coinbase).
    pub fn beneficiary(&self) -> Address {
        Self::with_storage(|s| s.beneficiary())
    }
    /// Returns the current block number.
    pub fn block_number(&self) -> u64 {
        Self::with_storage(|s| s.block_number())
    }

    /// Sets the bytecode at the given address.
    pub fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        Self::try_with_storage(|s| s.set_code(address, code))
    }

    /// Performs an SLOAD (persistent storage read).
    pub fn sload(&self, address: Address, key: U256) -> Result<U256> {
        Self::try_with_storage(|s| s.sload(address, key))
    }

    /// Performs a TLOAD (transient storage read).
    pub fn tload(&self, address: Address, key: U256) -> Result<U256> {
        Self::try_with_storage(|s| s.tload(address, key))
    }

    /// Performs an SSTORE (persistent storage write).
    pub fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        Self::try_with_storage(|s| s.sstore(address, key, value))
    }

    /// Performs a TSTORE (transient storage write).
    pub fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        Self::try_with_storage(|s| s.tstore(address, key, value))
    }

    /// Emits an event from the given contract address.
    pub fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        Self::try_with_storage(|s| s.emit_event(address, event))
    }

    /// Adds gas to the refund counter.
    pub fn refund_gas(&mut self, gas: i64) {
        Self::with_storage(|s| s.refund_gas(gas))
    }
    /// Returns the gas limit for this precompile call.
    pub fn gas_limit(&self) -> u64 {
        Self::with_storage(|s| s.gas_limit())
    }
    /// Returns the gas used so far.
    pub fn gas_used(&self) -> u64 {
        Self::with_storage(|s| s.gas_used())
    }
    /// Returns the gas refunded so far.
    pub fn gas_refunded(&self) -> i64 {
        Self::with_storage(|s| s.gas_refunded())
    }
    /// Returns whether the current call context is static.
    pub fn is_static(&self) -> bool {
        Self::with_storage(|s| s.is_static())
    }
    /// Returns the address that called this precompile.
    pub fn caller(&self) -> Address {
        Self::with_storage(|s| s.caller())
    }

    /// Deducts gas from the remaining gas, returning `OutOfGas` if insufficient.
    pub fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        Self::try_with_storage(|s| s.deduct_gas(gas))
    }

    /// Computes keccak256 and charges the appropriate gas.
    pub fn keccak256(&self, data: &[u8]) -> Result<B256> {
        Self::try_with_storage(|s| s.keccak256(data))
    }

    /// Creates a journal checkpoint and returns a RAII guard that auto-reverts on drop.
    pub fn checkpoint(&mut self) -> CheckpointGuard {
        let checkpoint = Self::with_storage(|s| s.checkpoint());
        CheckpointGuard { checkpoint: Some(checkpoint) }
    }

    /// Returns a success [`PrecompileOutput`] with the current gas used.
    pub fn success_output(&self, output: Bytes) -> PrecompileOutput {
        PrecompileOutput::new(self.gas_used(), output)
    }

    /// Returns an ABI-encoded success output.
    pub fn abi_success(&self, output: impl SolInterface) -> PrecompileOutput {
        self.success_output(output.abi_encode().into())
    }

    /// Returns a revert [`PrecompileOutput`] with the current gas used.
    pub fn revert_output(&self, output: Bytes) -> PrecompileOutput {
        PrecompileOutput::new_reverted(self.gas_used(), output)
    }

    /// Reverts with an ABI-encoded error.
    pub fn abi_revert(&self, error: impl SolInterface) -> PrecompileOutput {
        self.revert_output(error.abi_encode().into())
    }

    /// Returns a [`PrecompileResult`] constructed from the given error.
    pub fn error_result(&self, error: impl Into<BasePrecompileError>) -> PrecompileResult {
        error.into().into_precompile_result(self.gas_used())
    }
}

/// RAII guard for atomic state mutation batching.
///
/// On drop, automatically reverts all state changes made since the checkpoint
/// unless [`commit`](CheckpointGuard::commit) is called.
#[derive(Debug)]
pub struct CheckpointGuard {
    checkpoint: Option<JournalCheckpoint>,
}

impl CheckpointGuard {
    /// Commits all state changes since the checkpoint.
    pub fn commit(mut self) {
        if let Some(cp) = self.checkpoint.take() {
            StorageCtx::with_storage(|s| s.checkpoint_commit(cp));
        }
    }
}

impl Drop for CheckpointGuard {
    fn drop(&mut self) {
        if let Some(cp) = self.checkpoint.take() {
            StorageCtx::with_storage(|s| s.checkpoint_revert(cp));
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
use crate::hashmap::HashMapStorageProvider;

#[cfg(any(test, feature = "test-utils"))]
impl StorageCtx {
    #[allow(clippy::mut_from_ref)]
    fn as_hashmap(&self) -> &mut HashMapStorageProvider {
        Self::with_storage(|s| {
            // SAFETY: Test code always uses HashMapStorageProvider. The reference is valid
            // for the duration of the StorageCtx::enter closure.
            unsafe {
                extend_lifetime_mut(
                    &mut *(s as *mut dyn PrecompileStorageProvider as *mut HashMapStorageProvider),
                )
            }
        })
    }

    /// Returns account info for the given address (test-utils only).
    pub fn get_account_info(&self, address: Address) -> Option<&AccountInfo> {
        self.as_hashmap().get_account_info(address)
    }

    /// Returns emitted events for the given address (test-utils only).
    pub fn get_events(&self, address: Address) -> &Vec<LogData> {
        self.as_hashmap().get_events(address)
    }

    /// Sets the nonce for the given address (test-utils only).
    pub fn set_nonce(&mut self, address: Address, nonce: u64) {
        self.as_hashmap().set_nonce(address, nonce)
    }

    /// Overrides the block timestamp (test-utils only).
    pub fn set_timestamp(&mut self, timestamp: U256) {
        self.as_hashmap().set_timestamp(timestamp)
    }

    /// Overrides the block beneficiary (test-utils only).
    pub fn set_beneficiary(&mut self, beneficiary: Address) {
        self.as_hashmap().set_beneficiary(beneficiary)
    }

    /// Overrides the block number (test-utils only).
    pub fn set_block_number(&mut self, block_number: u64) {
        self.as_hashmap().set_block_number(block_number)
    }

    /// Clears all transient storage (test-utils only).
    pub fn clear_transient(&mut self) {
        self.as_hashmap().clear_transient()
    }
    /// Clears emitted events for the given address (test-utils only).
    pub fn clear_events(&mut self, address: Address) {
        self.as_hashmap().clear_events(address);
    }
    /// Returns the SLOAD counter (test-utils only).
    pub fn counter_sload(&self) -> u64 {
        self.as_hashmap().counter_sload()
    }
    /// Returns the SSTORE counter (test-utils only).
    pub fn counter_sstore(&self) -> u64 {
        self.as_hashmap().counter_sstore()
    }
    /// Resets the SLOAD/SSTORE counters (test-utils only).
    pub fn reset_counters(&mut self) {
        self.as_hashmap().reset_counters()
    }

    /// Returns true if the contract at the given address has non-empty bytecode (test-utils only).
    pub fn has_bytecode(&self, address: Address) -> Result<bool> {
        self.with_account_info(address, |info| Ok(!info.is_empty_code_hash()))
    }
}

// SAFETY: Caller must ensure the reference remains valid for the extended lifetime.
#[cfg(any(test, feature = "test-utils"))]
unsafe fn extend_lifetime_mut<'b, T: ?Sized>(r: &mut T) -> &'b mut T {
    // SAFETY: Upheld by caller.
    unsafe { &mut *(r as *mut T) }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn test_reentrant_with_storage_panics() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, || {
            StorageCtx::with_storage(|_| StorageCtx::with_storage(|_| ()))
        });
    }

    #[test]
    fn test_checkpoint_commit_and_revert() {
        let mut storage = crate::hashmap::HashMapStorageProvider::new(1);
        let addr = Address::ZERO;
        let key = U256::from(1);

        StorageCtx::enter(&mut storage, || {
            let mut ctx = StorageCtx;
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
