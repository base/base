use alloc::string::String;
use core::result;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{Panic, PanicKind, SolError, sol};

sol! {
    /// Precompile cannot be executed via delegatecall or callcode.
    error DelegateCallNotAllowed();
}

use super::{PrecompileError, PrecompileOutput, PrecompileResult, StorageFeatures};

/// Top-level error type for all Base native precompile operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasePrecompileError {
    /// EVM panic (arithmetic under/overflow, out-of-bounds access, enum conversion).
    Panic(PanicKind),

    /// Gas limit exceeded during precompile execution.
    OutOfGas,

    /// The calldata's 4-byte selector does not match any known precompile function.
    UnknownFunctionSelector([u8; 4]),

    /// The calldata selector is known, but its arguments failed ABI decoding.
    AbiDecodeFailed {
        /// The matched calldata selector.
        selector: [u8; 4],
        /// The ABI decoder error.
        error: String,
    },

    /// Storage slot arithmetic overflow.
    SlotOverflow,

    /// State mutation attempted inside a STATICCALL context.
    ///
    /// Reverts the current call frame without consuming all gas, matching the EVM's
    /// `StateChangeDuringStaticCall` behaviour for SSTORE/LOG in static contexts.
    StaticCallViolation,

    /// ABI-encoded revert from a contract-defined error (e.g. `InvalidSender`).
    Revert(Bytes),

    /// Unrecoverable internal error (e.g. database failure).
    Fatal(String),
}

impl From<PanicKind> for BasePrecompileError {
    fn from(kind: PanicKind) -> Self {
        Self::Panic(kind)
    }
}

impl core::fmt::Display for BasePrecompileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Panic(kind) => write!(f, "Panic({kind:?})"),
            Self::OutOfGas => f.write_str("Gas limit exceeded"),
            Self::UnknownFunctionSelector(selector) => {
                write!(f, "Unknown function selector: {selector:?}")
            }
            Self::AbiDecodeFailed { selector, error } => {
                write!(f, "ABI decode failed for selector {selector:?}: {error}")
            }
            Self::SlotOverflow => f.write_str("Slot overflow"),
            Self::StaticCallViolation => f.write_str("State mutation in static call"),
            Self::Revert(_) => f.write_str("Revert"),
            Self::Fatal(message) => write!(f, "Fatal precompile error: {message:?}"),
        }
    }
}

impl core::error::Error for BasePrecompileError {}

/// Result type alias for Base native precompile operations.
pub type Result<T> = result::Result<T, BasePrecompileError>;

impl BasePrecompileError {
    /// Returns true if this error must be propagated rather than turned into a revert.
    pub const fn is_system_error(&self) -> bool {
        matches!(self, Self::OutOfGas | Self::Fatal(_) | Self::Panic(_) | Self::SlotOverflow)
    }

    /// ABI-encodes a contract-defined error and wraps it as a [`Revert`](Self::Revert).
    pub fn revert(error: impl SolError) -> Self {
        Self::Revert(error.abi_encode().into())
    }

    /// Creates an arithmetic under/overflow panic error.
    pub const fn under_overflow() -> Self {
        Self::Panic(PanicKind::UnderOverflow)
    }

    /// Creates an enum conversion error panic (Solidity Panic `0x21`).
    pub const fn enum_conversion_error() -> Self {
        Self::Panic(PanicKind::EnumConversionError)
    }

    /// Creates an assertion-failure panic error (Solidity `Panic(0x01)`).
    ///
    /// Use for internal invariants that must always hold. It is classified as a
    /// system error (see [`Self::is_system_error`]) so it propagates and fails
    /// the transaction rather than surfacing as a catchable revert.
    pub const fn assert_failed() -> Self {
        Self::Panic(PanicKind::Assert)
    }

    /// Converts this error into a [`PrecompileResult`] (revert, out-of-gas halt or fatal error)
    /// using `gas` used and the fork-dependent `features`; the output carries no refund.
    pub fn into_precompile_result(self, gas: u64, features: StorageFeatures) -> PrecompileResult {
        let bytes: Bytes = match self {
            Self::Revert(bytes) => bytes,
            Self::Panic(kind) => Panic { code: U256::from(kind as u32) }.abi_encode().into(),
            Self::OutOfGas => {
                return Ok(PrecompileOutput::halt());
            }
            Self::SlotOverflow => {
                return Err(PrecompileError::Fatal("slot overflow".into()));
            }
            Self::Fatal(msg) => {
                return Err(PrecompileError::Fatal(msg));
            }
            Self::StaticCallViolation => Bytes::new(),
            Self::UnknownFunctionSelector(sel) => sel.to_vec().into(),
            Self::AbiDecodeFailed { selector, error } => {
                let mut bytes = selector.to_vec();
                if !features.selector_only_abi_decode_errors_enabled() {
                    bytes.extend_from_slice(error.as_bytes());
                }
                bytes.into()
            }
        };
        Ok(PrecompileOutput::revert(gas, bytes))
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolError;

    use super::*;

    #[test]
    fn delegate_call_not_allowed_encodes_to_typed_revert() {
        let expected: Bytes = DelegateCallNotAllowed {}.abi_encode().into();
        let result = BasePrecompileError::revert(DelegateCallNotAllowed {})
            .into_precompile_result(0, StorageFeatures::Legacy);
        let output = result.unwrap();
        assert!(output.is_revert());
        assert_eq!(output.bytes, expected);
    }

    #[test]
    fn abi_decode_failure_preserves_legacy_message_before_cobalt() {
        let selector = [0xde, 0xad, 0xbe, 0xef];
        let output =
            BasePrecompileError::AbiDecodeFailed { selector, error: "decoder error".into() }
                .into_precompile_result(0, StorageFeatures::Legacy)
                .unwrap();

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from([selector.as_slice(), b"decoder error"].concat()));
    }

    #[test]
    fn abi_decode_failure_encodes_as_selector_only_at_cobalt() {
        let selector = [0xde, 0xad, 0xbe, 0xef];
        let output =
            BasePrecompileError::AbiDecodeFailed { selector, error: "decoder error".into() }
                .into_precompile_result(0, StorageFeatures::Cobalt)
                .unwrap();

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(selector.to_vec()));
    }
}
