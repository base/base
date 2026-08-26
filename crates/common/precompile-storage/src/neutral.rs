//! Engine-neutral precompile boundary types.
//!
//! Base-owned equivalents of the execution-engine types that surface at the
//! precompile boundary: the precompile result/output/error/halt types and the
//! `AccountInfo`/`Bytecode` state types read and written by native precompiles.
//!
//! The enshrined precompile logic (b20, policy, nonce, tx-context) is written
//! against these types instead of naming `revm` directly, so the same logic can
//! be reused across execution engines. This module is the single place that
//! knows how to convert a base type into a concrete engine type: today only the
//! `revm` conversions exist; an EVM2 backend adds the parallel conversions here
//! without touching any enshrined logic.
//!
//! The conversions are exact, field-for-field mirrors of the corresponding
//! `revm` constructors so that neutralizing the shared logic is behavior
//! preserving.

use alloc::string::String;
use core::result;

use alloy_primitives::{Address, B256, Bytes, U256};
use revm::{
    precompile::{
        PrecompileError as RevmPrecompileError, PrecompileHalt as RevmPrecompileHalt,
        PrecompileOutput as RevmPrecompileOutput, PrecompileStatus as RevmPrecompileStatus,
    },
    primitives::KECCAK_EMPTY,
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};

/// Engine-neutral account bytecode.
///
/// Base-owned equivalent of the execution engine's bytecode type, covering the
/// two representations Base writes and reads through `set_code` /
/// `with_account_code`: legacy (analyzed) bytecode and EIP-7702 delegation
/// designators. Conversions map one-to-one onto the corresponding `revm`
/// constructors so neutralizing the callers is behavior preserving.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Bytecode {
    /// Legacy bytecode, analyzed into a jump table on conversion to the engine type.
    Legacy(Bytes),
    /// EIP-7702 delegation designator pointing at the delegated address.
    Eip7702(Address),
}

impl Default for Bytecode {
    fn default() -> Self {
        Self::Legacy(Bytes::new())
    }
}

impl Bytecode {
    /// Creates legacy bytecode from raw bytes.
    pub const fn new_legacy(raw: Bytes) -> Self {
        Self::Legacy(raw)
    }

    /// Creates an EIP-7702 delegation designator for `address`.
    pub const fn new_eip7702(address: Address) -> Self {
        Self::Eip7702(address)
    }

    /// Returns the EIP-7702 delegated address, if this is a delegation designator.
    pub const fn eip7702_address(&self) -> Option<Address> {
        match self {
            Self::Eip7702(address) => Some(*address),
            Self::Legacy(_) => None,
        }
    }

    /// Returns whether the bytecode is empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Legacy(raw) => raw.is_empty(),
            Self::Eip7702(_) => false,
        }
    }

    /// Returns the original (unpadded) bytecode bytes.
    pub fn original_bytes(&self) -> Bytes {
        match self {
            Self::Legacy(raw) => raw.clone(),
            Self::Eip7702(address) => {
                // EIP-7702 delegation designator: 0xEF0100 || 20-byte address.
                let mut raw = [0u8; 23];
                raw[..3].copy_from_slice(&[0xEF, 0x01, 0x00]);
                raw[3..].copy_from_slice(address.as_slice());
                Bytes::copy_from_slice(&raw)
            }
        }
    }
}

impl From<Bytecode> for RevmBytecode {
    fn from(value: Bytecode) -> Self {
        match value {
            Bytecode::Legacy(raw) => Self::new_legacy(raw),
            Bytecode::Eip7702(address) => Self::new_eip7702(address),
        }
    }
}

impl From<&RevmBytecode> for Bytecode {
    fn from(value: &RevmBytecode) -> Self {
        value.eip7702_address().map_or_else(|| Self::Legacy(value.original_bytes()), Self::Eip7702)
    }
}

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
    /// Hash of the account bytecode, or [`KECCAK_EMPTY`] for an empty account.
    pub code_hash: B256,
}

impl Default for AccountInfo {
    fn default() -> Self {
        Self { balance: U256::ZERO, nonce: 0, code_hash: KECCAK_EMPTY }
    }
}

impl AccountInfo {
    /// Returns true if the code hash is the Keccak256 hash of the empty string.
    ///
    /// Matches `revm`'s `AccountInfo::is_empty_code_hash`.
    pub fn is_empty_code_hash(&self) -> bool {
        self.code_hash == KECCAK_EMPTY
    }
}

impl From<&RevmAccountInfo> for AccountInfo {
    fn from(value: &RevmAccountInfo) -> Self {
        Self { balance: value.balance, nonce: value.nonce, code_hash: value.code_hash }
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

impl From<PrecompileHalt> for RevmPrecompileHalt {
    fn from(value: PrecompileHalt) -> Self {
        match value {
            PrecompileHalt::OutOfGas => Self::OutOfGas,
        }
    }
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

impl From<PrecompileStatus> for RevmPrecompileStatus {
    fn from(value: PrecompileStatus) -> Self {
        match value {
            PrecompileStatus::Success => Self::Success,
            PrecompileStatus::Revert => Self::Revert,
            PrecompileStatus::Halt(reason) => Self::Halt(reason.into()),
        }
    }
}

/// Fatal precompile error that halts EVM execution.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PrecompileError {
    /// Unrecoverable error that halts EVM execution.
    Fatal(String),
}

impl From<PrecompileError> for RevmPrecompileError {
    fn from(value: PrecompileError) -> Self {
        match value {
            PrecompileError::Fatal(msg) => Self::Fatal(msg),
        }
    }
}

/// Rich precompile execution output with gas accounting and status support.
///
/// Field-for-field mirror of `revm`'s `PrecompileOutput` so the [`From`]
/// conversion is a plain copy and neutralizing the shared logic preserves
/// behavior exactly, including the EIP-8037 state-gas accounting fields.
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

impl From<PrecompileOutput> for RevmPrecompileOutput {
    fn from(value: PrecompileOutput) -> Self {
        Self {
            status: value.status.into(),
            gas_used: value.gas_used,
            gas_refunded: value.gas_refunded,
            state_gas_used: value.state_gas_used,
            state_gas_spilled: value.state_gas_spilled,
            reservoir: value.reservoir,
            bytes: value.bytes,
        }
    }
}

/// Result of a native precompile execution.
pub type PrecompileResult = result::Result<PrecompileOutput, PrecompileError>;

/// Extension trait converting a base [`PrecompileResult`] into the engine result.
///
/// Applied at the single precompile-dispatch boundary (the `base_precompile!`
/// macro) where a native precompile hands its result back to the engine.
pub trait IntoEnginePrecompileResult {
    /// Converts a base precompile result into the `revm` precompile result.
    fn into_revm(self) -> revm::precompile::PrecompileResult;
}

impl IntoEnginePrecompileResult for PrecompileResult {
    fn into_revm(self) -> revm::precompile::PrecompileResult {
        self.map(Into::into).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precompile_output_conversion_is_field_exact() {
        let base = PrecompileOutput {
            status: PrecompileStatus::Success,
            gas_used: 500,
            gas_refunded: 200,
            state_gas_used: 30,
            state_gas_spilled: 20,
            reservoir: 10,
            bytes: Bytes::from_static(&[1, 2, 3]),
        };
        let revm: RevmPrecompileOutput = base.clone().into();
        assert_eq!(revm.gas_used, 500);
        assert_eq!(revm.gas_refunded, 200);
        assert_eq!(revm.state_gas_used, 30);
        assert_eq!(revm.state_gas_spilled, 20);
        assert_eq!(revm.reservoir, 10);
        assert_eq!(revm.bytes, base.bytes);
        assert!(revm.is_success());
    }

    #[test]
    fn revert_and_halt_map_to_matching_status() {
        let revert: RevmPrecompileOutput = PrecompileOutput::revert(7, Bytes::new(), 3).into();
        assert!(revert.is_revert());
        assert_eq!(revert.gas_used, 7);

        let halt: RevmPrecompileOutput = PrecompileOutput::halt(PrecompileHalt::OutOfGas, 0).into();
        assert!(halt.is_halt());
    }

    #[test]
    fn bytecode_conversions_are_faithful() {
        let raw = Bytes::from_static(&[0x60, 0x00]);
        let legacy: RevmBytecode = Bytecode::new_legacy(raw.clone()).into();
        assert_eq!(legacy.original_bytes(), RevmBytecode::new_legacy(raw.clone()).original_bytes());
        assert_eq!(Bytecode::from(&legacy), Bytecode::Legacy(raw));

        let addr = Address::repeat_byte(0x11);
        let delegation: RevmBytecode = Bytecode::new_eip7702(addr).into();
        assert_eq!(delegation.eip7702_address(), Some(addr));
        assert_eq!(Bytecode::from(&delegation), Bytecode::Eip7702(addr));
        assert_eq!(Bytecode::new_eip7702(addr).eip7702_address(), Some(addr));

        assert!(Bytecode::default().is_empty());
        assert!(!Bytecode::new_eip7702(addr).is_empty());
    }

    #[test]
    fn account_info_reads_match_revm() {
        let revm = RevmAccountInfo {
            balance: U256::from(42u64),
            nonce: 7,
            code_hash: KECCAK_EMPTY,
            code: None,
            ..RevmAccountInfo::default()
        };
        let base = AccountInfo::from(&revm);
        assert_eq!(base.balance, U256::from(42u64));
        assert_eq!(base.nonce, 7);
        assert!(base.is_empty_code_hash());
    }
}
