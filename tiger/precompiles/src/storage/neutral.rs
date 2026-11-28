//! Engine-neutral precompile boundary types.
//!
//! Base-owned types that surface at the precompile boundary: the precompile
//! result/output/error types, the [`AccountInfo`] native precompiles read, and
//! the opaque journal [`Checkpoint`]. The enshrined precompile logic is written against these
//! types; the tiger evm crate converts them to evm2 types at the single
//! precompile-dispatch boundary.
//!
//! The types mirror the parts of the revm types base-reth uses that the
//! precompiles and the evm crate read, so the ported logic behaves exactly as it
//! does there.

use alloc::string::String;
use core::result;

use alloy_primitives::{B256, Bytes, KECCAK256_EMPTY, U256};

/// Engine-neutral account information.
///
/// Mirrors the fields the enshrined precompiles read; the closure callbacks in
/// [`crate::StorageCtx`] only observe `balance`, `nonce`, `code_hash`, and
/// [`AccountInfo::is_empty_code_hash`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountInfo {
    /// Account balance.
    pub balance: U256,
    /// Account nonce.
    pub nonce: u64,
    /// Hash of the account bytecode, or [`KECCAK256_EMPTY`] for an empty account.
    pub code_hash: B256,
}

impl Default for AccountInfo {
    fn default() -> Self {
        Self { balance: U256::ZERO, nonce: 0, code_hash: KECCAK256_EMPTY }
    }
}

impl AccountInfo {
    /// Returns true if the code hash is the Keccak256 hash of the empty string.
    pub fn is_empty_code_hash(&self) -> bool {
        self.code_hash == KECCAK256_EMPTY
    }
}

/// Status of a precompile execution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileStatus {
    /// Precompile executed successfully.
    Success,
    /// Precompile reverted (non-fatal, returns remaining gas).
    Revert,
    /// Precompile ran out of gas, the only halt native precompiles report.
    Halt,
}

/// Fatal precompile error that halts EVM execution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileError {
    /// Unrecoverable error that halts EVM execution.
    Fatal(String),
}

/// Precompile execution output with gas accounting and status.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrecompileOutput {
    /// Status of the precompile execution.
    pub status: PrecompileStatus,
    /// Regular gas used by the precompile.
    pub gas_used: u64,
    /// Gas refunded by the precompile.
    pub gas_refunded: i64,
    /// Output bytes.
    pub bytes: Bytes,
}

impl PrecompileOutput {
    /// Returns a new successful precompile output.
    pub const fn new(gas_used: u64, bytes: Bytes) -> Self {
        Self { status: PrecompileStatus::Success, gas_used, gas_refunded: 0, bytes }
    }

    /// Returns a new out-of-gas precompile output.
    pub const fn halt() -> Self {
        Self { status: PrecompileStatus::Halt, gas_used: 0, gas_refunded: 0, bytes: Bytes::new() }
    }

    /// Returns a new reverted precompile output.
    pub const fn revert(gas_used: u64, bytes: Bytes) -> Self {
        Self { status: PrecompileStatus::Revert, gas_used, gas_refunded: 0, bytes }
    }

    /// Returns `true` if the precompile execution was successful.
    pub const fn is_success(&self) -> bool {
        matches!(self.status, PrecompileStatus::Success)
    }

    /// Returns `true` if the precompile reverted.
    pub const fn is_revert(&self) -> bool {
        matches!(self.status, PrecompileStatus::Revert)
    }
}

/// Opaque position in the state journal, returned by
/// [`PrecompileStorageProvider::checkpoint`](super::PrecompileStorageProvider::checkpoint) and
/// passed back to revert to it.
///
/// `journal_len` and `logs_len` are the journal and log lengths at the checkpoint; providers
/// that keep a snapshot stack store the stack index in `journal_len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Checkpoint {
    /// Journal length (or snapshot index) when the checkpoint was taken.
    pub journal_len: usize,
    /// Log count when the checkpoint was taken.
    pub logs_len: usize,
}

/// Result of a native precompile execution.
pub type PrecompileResult = result::Result<PrecompileOutput, PrecompileError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_and_halt_map_to_matching_status() {
        let revert = PrecompileOutput::revert(7, Bytes::new());
        assert!(revert.is_revert());
        assert_eq!(revert.gas_used, 7);

        let halt = PrecompileOutput::halt();
        assert_eq!(halt.status, PrecompileStatus::Halt);
    }
}
