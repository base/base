//! Engine-neutral precompile boundary types.
//!
//! Base-owned equivalents of the execution-engine types that surface at the
//! precompile boundary: the precompile result/output/error/halt types, the
//! `AccountInfo`/`Bytecode` state types read and written by native precompiles,
//! and the checkpoint handle used for atomic state batches.
//!
//! The enshrined precompile logic (b20, policy, nonce, tx-context) is written
//! against these types instead of naming an engine directly, so the same logic
//! serves every execution engine. The per-engine conversions live next to each
//! engine's provider (`revm_compat`, `evm2_compat`) and are exact,
//! field-for-field mirrors of the engine types.

use alloc::{string::String, vec::Vec};
use core::result;

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};

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
    /// Hash of the account bytecode, or [`KECCAK256_EMPTY`] for an account without code.
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

/// Engine-neutral account bytecode.
///
/// Holds exactly what native precompiles read and write: the original code bytes, or the delegate
/// of an EIP-7702 delegation designator. Engines rebuild their own bytecode with the matching
/// `new_legacy` / `new_eip7702` constructor, so the stored code and its hash are identical across
/// engines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bytecode {
    /// Legacy bytecode, kept as its original bytes. Empty bytes mean the account has no code.
    Legacy(Bytes),
    /// EIP-7702 delegation designator (`0xef0100 ‖ delegate`).
    Eip7702(Address),
}

impl Default for Bytecode {
    fn default() -> Self {
        Self::Legacy(Bytes::new())
    }
}

impl Bytecode {
    /// EIP-7702 designator prefix: magic bytes followed by the version byte.
    const EIP7702_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

    /// Creates legacy bytecode from its original bytes.
    pub const fn new_legacy(raw: Bytes) -> Self {
        Self::Legacy(raw)
    }

    /// Creates an EIP-7702 delegation designator pointing at `delegate`.
    pub const fn new_eip7702(delegate: Address) -> Self {
        Self::Eip7702(delegate)
    }

    /// Returns the delegate address if this is an EIP-7702 designator.
    pub const fn eip7702_address(&self) -> Option<Address> {
        match self {
            Self::Legacy(_) => None,
            Self::Eip7702(delegate) => Some(*delegate),
        }
    }

    /// Returns the original code bytes as stored in the account.
    pub fn original_bytes(&self) -> Bytes {
        match self {
            Self::Legacy(raw) => raw.clone(),
            Self::Eip7702(delegate) => {
                [&Self::EIP7702_PREFIX[..], delegate.as_slice()].concat().into()
            }
        }
    }

    /// Returns the length of the original code bytes.
    pub fn len(&self) -> usize {
        match self {
            Self::Legacy(raw) => raw.len(),
            Self::Eip7702(_) => Self::EIP7702_PREFIX.len() + size_of::<Address>(),
        }
    }

    /// Returns true if the account has no code.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Computes the code hash: the Keccak256 hash of the original bytes.
    pub fn hash_slow(&self) -> B256 {
        if self.is_empty() { KECCAK256_EMPTY } else { keccak256(self.original_bytes()) }
    }
}

/// Handle to a provider checkpoint.
///
/// Checkpoints nest: they are committed or reverted most-recent first. A handle stays valid until
/// its checkpoint, or an older one, is committed or reverted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StorageCheckpoint(usize);

impl StorageCheckpoint {
    /// Returns the handle for the checkpoint opened at nesting `depth` (zero for the outermost).
    pub const fn new(depth: usize) -> Self {
        Self(depth)
    }

    /// Returns the nesting depth of this checkpoint.
    pub const fn depth(self) -> usize {
        self.0
    }
}

/// Stack of engine journal checkpoints addressed by [`StorageCheckpoint`] handles.
///
/// Lets engine-backed providers hand out neutral handles while keeping each engine's own
/// checkpoint type internal.
#[derive(Clone, Debug)]
pub struct CheckpointStack<C> {
    checkpoints: Vec<C>,
}

impl<C> Default for CheckpointStack<C> {
    fn default() -> Self {
        Self { checkpoints: Vec::new() }
    }
}

impl<C> CheckpointStack<C> {
    /// Records an engine checkpoint and returns its handle.
    pub fn push(&mut self, checkpoint: C) -> StorageCheckpoint {
        self.checkpoints.push(checkpoint);
        StorageCheckpoint::new(self.checkpoints.len() - 1)
    }

    /// Forgets the most recent checkpoint, keeping its changes.
    pub fn commit_latest(&mut self) {
        self.checkpoints.pop();
    }

    /// Removes `checkpoint` and every newer checkpoint, returning the engine checkpoint to
    /// revert to.
    pub fn take(&mut self, checkpoint: StorageCheckpoint) -> Option<C> {
        if checkpoint.depth() >= self.checkpoints.len() {
            return None;
        }
        self.checkpoints.drain(checkpoint.depth()..).next()
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
/// Field-for-field mirror of `revm`'s `PrecompileOutput`, including the
/// EIP-8037 state-gas accounting fields, so every engine rebuilds the same
/// frame gas from it.
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

/// Result of a native precompile execution.
pub type PrecompileResult = result::Result<PrecompileOutput, PrecompileError>;

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn eip7702_bytecode_round_trips_through_original_bytes() {
        let delegate = address!("0x00000000000000000000000000000000000000dd");
        let code = Bytecode::new_eip7702(delegate);

        let original = code.original_bytes();
        assert_eq!(original.len(), code.len());
        assert_eq!(&original[..3], &[0xef, 0x01, 0x00]);
        assert_eq!(&original[3..], delegate.as_slice());
        assert_eq!(code.hash_slow(), keccak256(&original));
        assert_eq!(code.eip7702_address(), Some(delegate));
    }

    #[test]
    fn empty_bytecode_hashes_to_empty_code_hash() {
        assert!(Bytecode::default().is_empty());
        assert_eq!(Bytecode::default().hash_slow(), KECCAK256_EMPTY);
        assert_eq!(Bytecode::new_legacy(Bytes::from_static(&[0xef])).len(), 1);
    }

    #[test]
    fn checkpoint_stack_reverts_to_requested_depth() {
        let mut stack = CheckpointStack::default();
        let outer = stack.push("outer");
        let inner = stack.push("inner");
        assert_eq!(inner.depth(), 1);

        assert_eq!(stack.take(outer), Some("outer"));
        assert_eq!(stack.take(inner), None);
    }
}
