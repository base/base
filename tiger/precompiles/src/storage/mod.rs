//! Storage and the provider boundary for the native precompiles.
//!
//! [`PrecompileStorageProvider`] is the boundary an execution engine implements (storage,
//! transient storage, logs, account and code access, gas, checkpoints), and [`GasSchedule`] the
//! fixed gas table base-reth charges for that work. [`StorageCtx`] is the scoped handle a
//! precompile call works through, created by [`StorageCtx::enter`]. [`StorageLayout`] and the
//! `StorageCtx` read/write helpers in `layout.rs` implement the precompiles' Solidity storage
//! layouts.

use alloc::string::ToString;
use core::{cell::RefCell, fmt};

use alloy_primitives::{Address, B256, Bytes, LogData, U256};

mod error;
pub use error::{BasePrecompileError, DelegateCallNotAllowed, Result};

mod neutral;
pub use neutral::{
    AccountInfo, Checkpoint, PrecompileError, PrecompileOutput, PrecompileResult, PrecompileStatus,
};

mod gas;
pub use gas::{GasSchedule, SstoreValues};

mod layout;
pub use layout::{StorageLayout, StorageValue};

mod provider;
pub use provider::{PrecompileStorageProvider, StorageFeatures};

#[cfg(any(test, feature = "test-utils"))]
mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::{HashMapStorageProvider, setup_storage};

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
    fn with_storage<R>(&self, f: impl FnOnce(&mut dyn PrecompileStorageProvider) -> R) -> R {
        let mut guard = self.storage.borrow_mut();
        f(&mut **guard)
    }

    fn try_with_storage<R>(
        &self,
        f: impl FnOnce(&mut dyn PrecompileStorageProvider) -> Result<R>,
    ) -> Result<R> {
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
        let mut result = None;
        let mut record = |info: &AccountInfo| result = Some(f(info));
        self.try_with_storage(|s| s.with_account_info(address, &mut record))?;
        let message = "with_account_info callback was not invoked";
        result.unwrap_or_else(|| Err(BasePrecompileError::Fatal(message.to_string())))
    }

    /// Returns the current chain ID.
    pub fn chain_id(&self) -> u64 {
        self.with_storage(|s| s.chain_id())
    }
    /// Returns the current block timestamp.
    pub fn timestamp(&self) -> U256 {
        self.with_storage(|s| s.timestamp())
    }
    /// Returns the transaction origin (`tx.origin`).
    pub fn origin(&self) -> Address {
        self.with_storage(|s| s.origin())
    }

    /// Sets the bytecode at the given address.
    pub fn set_code(&self, address: Address, code: Bytes) -> Result<()> {
        self.try_with_storage(|s| s.set_code(address, code))
    }

    /// Performs an SLOAD (persistent storage read).
    pub fn sload(&self, address: Address, key: U256) -> Result<U256> {
        self.try_with_storage(|s| s.sload(address, key))
    }

    /// Reads transient storage without exposing a TLOAD opcode charge.
    ///
    /// Use this only when the native operation applies its own gas schedule.
    pub fn tload_unmetered(&self, address: Address, key: U256) -> Result<U256> {
        self.try_with_storage(|s| s.tload_unmetered(address, key))
    }

    /// Performs an SSTORE (persistent storage write).
    pub fn sstore(&self, address: Address, key: U256, value: U256) -> Result<()> {
        self.try_with_storage(|s| s.sstore(address, key, value))
    }

    /// Checks whether the current call context permits storage writes.
    pub fn ensure_writable(&self) -> Result<()> {
        if self.is_static() { Err(BasePrecompileError::StaticCallViolation) } else { Ok(()) }
    }

    /// Performs a TSTORE (transient storage write).
    pub fn tstore(&self, address: Address, key: U256, value: U256) -> Result<()> {
        self.try_with_storage(|s| s.tstore(address, key, value))
    }

    /// Emits an event from the given contract address.
    pub fn emit_event(&self, address: Address, event: LogData) -> Result<()> {
        self.try_with_storage(|s| s.emit_event(address, event))
    }

    /// Returns the gas used so far.
    pub fn gas_used(&self) -> u64 {
        self.with_storage(|s| s.gas_used())
    }
    /// Returns the gas refunded so far.
    pub fn gas_refunded(&self) -> i64 {
        self.with_storage(|s| s.gas_refunded())
    }
    /// Returns the active persistent-storage features.
    pub fn storage_features(&self) -> StorageFeatures {
        self.with_storage(|s| s.storage_features())
    }
    /// Returns whether the current call context is static.
    pub fn is_static(&self) -> bool {
        self.with_storage(|s| s.is_static())
    }
    /// Returns the native value (in wei) attached to this precompile call.
    pub fn call_value(&self) -> U256 {
        self.with_storage(|s| s.call_value())
    }
    /// Returns the address that called this precompile.
    pub fn caller(&self) -> Address {
        self.with_storage(|s| s.caller())
    }

    /// Executes `f` with a temporary caller override, restoring the previous caller on exit.
    pub fn with_caller<R>(&self, caller: Address, f: impl FnOnce() -> R) -> R {
        let previous = self.with_storage(|s| s.replace_caller(caller));
        let _guard = CallerGuard { storage: *self, previous: Some(previous) };
        f()
    }

    /// Deducts gas from the remaining gas, returning `OutOfGas` if insufficient.
    pub fn deduct_gas(&self, gas: u64) -> Result<()> {
        self.try_with_storage(|s| s.deduct_gas(gas))
    }

    /// Computes keccak256 and charges the appropriate gas.
    pub fn metered_keccak256(&self, data: &[u8]) -> Result<B256> {
        self.try_with_storage(|s| s.metered_keccak256(data))
    }

    /// Creates a journal checkpoint and returns a RAII guard that auto-reverts on drop.
    pub fn checkpoint(&self) -> CheckpointGuard<'a> {
        let checkpoint = self.with_storage(|s| s.checkpoint());
        CheckpointGuard { storage: *self, checkpoint: Some(checkpoint) }
    }

    /// Converts a call's result into a [`PrecompileResult`] with its gas accounting.
    ///
    /// On success, the output carries the encoded return value, `gas_used` and the
    /// accumulated `gas_refunded`, which the frame handler adds to the transaction's refund
    /// counter (where the EIP-3529 cap applies). On error, the output carries no refund; see
    /// [`BasePrecompileError::into_precompile_result`].
    pub fn result_output(&self, result: Result<Bytes>) -> PrecompileResult {
        match result {
            Ok(output) => {
                let mut out = PrecompileOutput::new(self.gas_used(), output);
                out.gas_refunded = self.gas_refunded();
                Ok(out)
            }
            Err(error) => error.into_precompile_result(self.gas_used(), self.storage_features()),
        }
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
            // The cell is only already borrowed while unwinding from a panic; skip then.
            if let Ok(mut guard) = self.storage.storage.try_borrow_mut() {
                guard.replace_caller(previous);
            }
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
    checkpoint: Option<Checkpoint>,
}

impl CheckpointGuard<'_> {
    /// Commits all state changes since the checkpoint.
    ///
    /// Uses `borrow_mut` (panicking) intentionally: `commit` is an explicit
    /// caller action, so a conflicting borrow here is a logic bug that should
    /// surface loudly. Contrast with `drop` below, which uses `try_borrow_mut`
    /// because `drop` may run during unwinding where a second panic would abort.
    pub fn commit(mut self) {
        let checkpoint = self.checkpoint.take();
        if let Some(checkpoint) = checkpoint {
            self.storage.with_storage(|storage| {
                #[cfg(any(test, feature = "test-utils"))]
                storage.assert_latest_checkpoint(checkpoint);
                #[cfg(not(any(test, feature = "test-utils")))]
                let _ = checkpoint;
                storage.commit_latest_checkpoint();
            });
        }
    }
}

impl Drop for CheckpointGuard<'_> {
    fn drop(&mut self) {
        if let Some(cp) = self.checkpoint.take() {
            // The cell is only already borrowed while unwinding from a panic; skip then.
            if let Ok(mut guard) = self.storage.storage.try_borrow_mut() {
                guard.checkpoint_revert(cp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn test_reentrant_with_storage_panics() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| ctx.with_storage(|_| ctx.with_storage(|_| ())));
    }

    #[test]
    fn test_with_caller_restores_previous_caller() {
        let mut storage = HashMapStorageProvider::new(1);
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
    fn test_checkpoint_commit_and_revert() {
        let mut storage = HashMapStorageProvider::new(1);
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

    #[test]
    #[should_panic(expected = "out-of-order checkpoint commit (expected top of stack)")]
    fn checkpoint_commit_rejects_non_lifo_order() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            let outer = ctx.checkpoint();
            let _inner = ctx.checkpoint();

            outer.commit();
        });
    }

    #[test]
    fn result_output_success_propagates_gas_refunded() {
        let mut storage = HashMapStorageProvider::new(1);
        let addr = Address::ZERO;
        let key = U256::from(1);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            // Write a non-zero value then clear it to generate an EIP-3529 refund.
            ctx.sstore(addr, key, U256::from(42)).unwrap();
            ctx.sstore(addr, key, U256::ZERO).unwrap();

            ctx.result_output(Ok(Bytes::new()))
        })
        .unwrap();

        assert!(output.is_success());
        assert!(output.gas_refunded > 0, "clearing a slot must propagate a non-zero refund");
    }

    #[test]
    fn result_output_error_does_not_expose_refund() {
        let mut storage = HashMapStorageProvider::new(1);
        let addr = Address::ZERO;
        let key = U256::from(1);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            ctx.sstore(addr, key, U256::from(1)).unwrap();
            ctx.sstore(addr, key, U256::ZERO).unwrap();

            ctx.result_output(Err(BasePrecompileError::Revert(Bytes::new())))
        })
        .unwrap();

        assert!(output.is_revert());
        assert_eq!(output.gas_refunded, 0, "error path must not propagate gas_refunded");
    }

    #[test]
    fn result_output_gates_abi_decode_error_encoding_at_cobalt() {
        let selector = [0xde, 0xad, 0xbe, 0xef];
        let error =
            || BasePrecompileError::AbiDecodeFailed { selector, error: "decoder error".into() };

        let mut legacy = HashMapStorageProvider::new(1);
        let legacy_output = StorageCtx::enter(&mut legacy, |ctx| {
            ctx.result_output(Err(error()))
        })
        .unwrap();
        assert_eq!(
            legacy_output.bytes,
            Bytes::from([selector.as_slice(), b"decoder error"].concat())
        );

        let mut cobalt = HashMapStorageProvider::new_with_storage_features(
            1,
            StorageFeatures::Cobalt,
        );
        let cobalt_output = StorageCtx::enter(&mut cobalt, |ctx| {
            ctx.result_output(Err(error()))
        })
        .unwrap();
        assert_eq!(cobalt_output.bytes, Bytes::from(selector.to_vec()));
    }

    #[test]
    fn result_output_success_no_refund() {
        let mut storage = HashMapStorageProvider::new(1);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            ctx.result_output(Ok(Bytes::new()))
        })
        .unwrap();

        assert!(output.is_success());
        assert_eq!(output.gas_refunded, 0);
    }
}
