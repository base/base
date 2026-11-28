//! The storage provider boundary for Base native precompiles.
//!
//! Defines [`PrecompileStorageProvider`] (the EVM access boundary) and [`StorageFeatures`].

use alloy_primitives::{Address, B256, Bytes, LogData, U256};

use super::{AccountInfo, Checkpoint, Result};

/// Low-level storage provider for interacting with the EVM.
///
/// Abstracted over both production EVM journal and test `HashMap` backends.
pub trait PrecompileStorageProvider {
    /// Returns the current chain ID.
    fn chain_id(&self) -> u64;
    /// Returns the current block timestamp.
    fn timestamp(&self) -> U256;
    /// Returns the transaction origin (`tx.origin`).
    fn origin(&self) -> Address;

    /// Sets the (legacy) bytecode at the given address.
    fn set_code(&mut self, address: Address, code: Bytes) -> Result<()>;

    /// Executes a closure with access to the account info for the given address.
    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()>;

    /// Performs an SLOAD operation (persistent storage read).
    fn sload(&mut self, address: Address, key: U256) -> Result<U256>;
    /// Reads transient storage without a TLOAD charge: the native precompiles that read
    /// transient storage price the enclosing operation themselves.
    fn tload_unmetered(&mut self, address: Address, key: U256) -> Result<U256>;
    /// Performs an SSTORE operation (persistent storage write).
    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()>;
    /// Performs a TSTORE operation (transient storage write).
    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()>;

    /// Emits an event from the given contract address.
    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()>;

    /// Deducts gas from the remaining gas and returns an error if insufficient.
    fn deduct_gas(&mut self, gas: u64) -> Result<()>;
    /// Returns the gas used so far.
    fn gas_used(&self) -> u64;
    /// Returns the gas refunded so far.
    fn gas_refunded(&self) -> i64;

    /// Returns the active persistent-storage features.
    fn storage_features(&self) -> StorageFeatures;

    /// Returns whether the current call context is static.
    fn is_static(&self) -> bool;

    /// Returns the native value (in wei) attached to this precompile call.
    fn call_value(&self) -> U256;

    /// Returns the address that called this precompile.
    fn caller(&self) -> Address;
    /// Replaces the current caller address and returns the previous caller.
    fn replace_caller(&mut self, caller: Address) -> Address;

    /// Creates a new journal checkpoint for atomic state management.
    fn checkpoint(&mut self) -> Checkpoint;
    /// Commits the most recently created active checkpoint.
    ///
    /// Keeps all state changes made since that checkpoint was opened. Commit is
    /// always stack-top / LIFO and does not target an older checkpoint token.
    fn commit_latest_checkpoint(&mut self);
    /// Asserts that `checkpoint` is the most recently created active checkpoint.
    ///
    /// Test providers use this hook to catch out-of-order guard commits before
    /// [`Self::commit_latest_checkpoint`] resolves the wrong stack entry.
    #[cfg(any(test, feature = "test-utils"))]
    fn assert_latest_checkpoint(&self, _checkpoint: Checkpoint) {}
    /// Reverts all state changes back to the given checkpoint.
    fn checkpoint_revert(&mut self, checkpoint: Checkpoint);

    /// Computes keccak256 and charges [`GasSchedule::keccak256`](super::GasSchedule::keccak256).
    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256>;
}

/// Resolved native precompile storage behaviors, independent of the chain's schedule.
///
/// `Legacy` and `Cobalt` retain the historical profile names for callers and fixtures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorageFeatures {
    /// Preserve precompile behavior from before Cobalt activation.
    #[default]
    Legacy,
    /// Apply Cobalt behavior, including dynamic tail cleanup and compact ABI decode errors.
    Cobalt,
}

impl StorageFeatures {
    /// Returns whether dynamic storage tail cleanup is enabled.
    pub const fn dynamic_storage_tail_cleanup_enabled(self) -> bool {
        matches!(self, Self::Cobalt)
    }

    /// Returns whether ABI decode failures should omit the decoder message.
    pub const fn selector_only_abi_decode_errors_enabled(self) -> bool {
        matches!(self, Self::Cobalt)
    }
}
