//! Engine-neutral precompile boundary types.
//!
//! Base-owned types that surface at the precompile boundary: the precompile
//! result/output/error/halt types, the [`AccountInfo`] and [`Bytecode`] state
//! types read and written by native precompiles, and the opaque journal
//! [`Checkpoint`]. The enshrined precompile logic is written against these
//! types; the tiger evm crate converts them to evm2 types at the single
//! precompile-dispatch boundary.
//!
//! The types are field-for-field mirrors of the revm types base-reth uses, so
//! the ported logic behaves exactly as it does there.

use alloc::string::String;
use core::result;

use alloy_primitives::{B256, Bytes, KECCAK256_EMPTY, U256, keccak256};

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
    ///
    pub fn is_empty_code_hash(&self) -> bool {
        self.code_hash == KECCAK256_EMPTY
    }
}

/// Non-fatal precompile halt reason.
///
/// Neutral enshrined logic only ever halts with [`PrecompileHalt::OutOfGas`];
/// the stock-crypto precompile wrappers keep using the engine's own halt enum
/// directly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileHalt {
    /// The precompile ran out of gas.
    OutOfGas,
}

/// Status of a precompile execution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileStatus {
    /// Precompile executed successfully.
    Success,
    /// Precompile reverted (non-fatal, returns remaining gas).
    Revert,
    /// Precompile halted with a specific reason.
    Halt(PrecompileHalt),
}

/// Fatal precompile error that halts EVM execution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileError {
    /// Unrecoverable error that halts EVM execution.
    Fatal(String),
}

/// Rich precompile execution output with gas accounting and status support.
///
/// Field-for-field mirror of revm's `PrecompileOutput`, including the EIP-8037
/// state-gas accounting fields, so results compare one to one with base-reth.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrecompileOutput {
    /// Status of the precompile execution.
    pub status: PrecompileStatus,
    /// Regular gas used by the precompile.
    pub gas_used: u64,
    /// Gas refunded by the precompile.
    pub gas_refunded: i64,
    /// State gas used by the precompile.
    pub state_gas_used: i64,
    /// State gas drawn from regular gas because the reservoir was empty.
    pub state_gas_spilled: u64,
    /// Reservoir gas for EIP-8037.
    pub reservoir: u64,
    /// Output bytes.
    pub bytes: Bytes,
}

impl PrecompileOutput {
    /// Returns a new successful precompile output.
    ///
    /// `reservoir` is the EIP-8037 state-gas reservoir field; Base's call sites
    /// (`StorageCtx::success_output`, `IntoPrecompileResult::into_precompile_result`)
    /// pass the transaction's cumulative state gas here, matching the field the
    /// engine's frame handler reads. The name mirrors the underlying engine type.
    pub const fn new(gas_used: u64, bytes: Bytes, reservoir: u64) -> Self {
        Self {
            status: PrecompileStatus::Success,
            gas_used,
            gas_refunded: 0,
            state_gas_used: 0,
            state_gas_spilled: 0,
            reservoir,
            bytes,
        }
    }

    /// Returns a new halted precompile output with the given halt reason.
    pub const fn halt(reason: PrecompileHalt, reservoir: u64) -> Self {
        Self {
            status: PrecompileStatus::Halt(reason),
            gas_used: 0,
            gas_refunded: 0,
            state_gas_used: 0,
            state_gas_spilled: 0,
            reservoir,
            bytes: Bytes::new(),
        }
    }

    /// Returns a new reverted precompile output.
    pub const fn revert(gas_used: u64, bytes: Bytes, reservoir: u64) -> Self {
        Self {
            status: PrecompileStatus::Revert,
            gas_used,
            gas_refunded: 0,
            state_gas_used: 0,
            state_gas_spilled: 0,
            reservoir,
            bytes,
        }
    }

    /// Returns `true` if the precompile execution was successful.
    pub const fn is_success(&self) -> bool {
        matches!(self.status, PrecompileStatus::Success)
    }

    /// Returns `true` if the precompile reverted.
    pub const fn is_revert(&self) -> bool {
        matches!(self.status, PrecompileStatus::Revert)
    }

    /// Returns `true` if the precompile halted.
    pub const fn is_halt(&self) -> bool {
        matches!(self.status, PrecompileStatus::Halt(_))
    }
}

/// Account bytecode as native precompiles see it: the original (unpadded) code bytes.
///
/// Mirrors the parts of revm's `Bytecode` the precompiles use: `len` and `is_empty` count the
/// original bytes and `hash_slow` hashes them, exactly as revm does for legacy bytecode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytecode {
    bytes: Bytes,
}

impl Bytecode {
    /// Creates legacy bytecode from its original bytes.
    pub const fn new_legacy(bytes: Bytes) -> Self {
        Self { bytes }
    }

    /// Returns the original code bytes.
    pub const fn original_bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Returns the number of original code bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the code is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the Keccak-256 code hash ([`KECCAK256_EMPTY`] for empty code).
    pub fn hash_slow(&self) -> B256 {
        if self.bytes.is_empty() { KECCAK256_EMPTY } else { keccak256(&self.bytes) }
    }
}

/// Opaque position in the state journal, returned by
/// [`PrecompileStorageProvider::checkpoint`](crate::PrecompileStorageProvider::checkpoint) and
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
        let revert = PrecompileOutput::revert(7, Bytes::new(), 3);
        assert!(revert.is_revert());
        assert_eq!(revert.gas_used, 7);

        let halt = PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0);
        assert!(halt.is_halt());
    }

    #[test]
    fn bytecode_hashes_original_bytes() {
        let code = Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        assert_eq!(code.len(), 1);
        assert_eq!(code.hash_slow(), keccak256([0xef]));
        assert_eq!(Bytecode::default().hash_slow(), KECCAK256_EMPTY);
        assert!(AccountInfo::default().is_empty_code_hash());
    }
}
