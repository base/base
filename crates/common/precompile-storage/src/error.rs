use alloc::string::{String, ToString};
use core::result;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{Panic, PanicKind, SolError};
use revm::{
    context::journaled_state::JournalLoadError,
    precompile::{PrecompileError, PrecompileOutput, PrecompileResult},
};

/// Top-level error type for all Base native precompile operations.
#[derive(
    Debug, Clone, PartialEq, Eq, thiserror::Error, derive_more::From, derive_more::TryInto,
)]
pub enum BasePrecompileError {
    /// EVM panic (arithmetic under/overflow, out-of-bounds access, enum conversion).
    #[error("Panic({0:?})")]
    Panic(PanicKind),

    /// Gas limit exceeded during precompile execution.
    #[error("Gas limit exceeded")]
    OutOfGas,

    /// The calldata's 4-byte selector does not match any known precompile function.
    #[error("Unknown function selector: {0:?}")]
    UnknownFunctionSelector([u8; 4]),

    /// The calldata selector is known, but its arguments failed ABI decoding.
    #[error("ABI decode failed for selector {selector:?}: {error}")]
    AbiDecodeFailed {
        /// The matched calldata selector.
        selector: [u8; 4],
        /// The ABI decoder error.
        error: String,
    },

    /// Storage slot arithmetic overflow.
    #[error("Slot overflow")]
    SlotOverflow,

    /// State mutation attempted inside a STATICCALL context.
    ///
    /// Reverts the current call frame without consuming all gas, matching the EVM's
    /// `StateChangeDuringStaticCall` behaviour for SSTORE/LOG in static contexts.
    #[error("State mutation in static call")]
    StaticCallViolation,

    /// ABI-encoded revert from a contract-defined error (e.g. `InvalidSender`).
    #[error("Revert")]
    #[from(skip)]
    Revert(Bytes),

    /// Unrecoverable internal error (e.g. database failure).
    #[error("Fatal precompile error: {0:?}")]
    #[from(skip)]
    Fatal(String),
}

impl From<JournalLoadError<revm::context::ErasedError>> for BasePrecompileError {
    fn from(value: JournalLoadError<revm::context::ErasedError>) -> Self {
        match value {
            JournalLoadError::DBError(e) => Self::Fatal(e.to_string()),
            JournalLoadError::ColdLoadSkipped => Self::OutOfGas,
        }
    }
}

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

    /// Creates an array out-of-bounds panic error.
    pub const fn array_oob() -> Self {
        Self::Panic(PanicKind::ArrayOutOfBounds)
    }

    /// ABI-encodes this error and wraps it as a [`PrecompileResult`] (revert or fatal error).
    ///
    /// Internal dispatch diagnostics use compact, non-ABI revert data: unknown selectors return the
    /// raw selector bytes, and decode failures return `selector || utf8_error_string`.
    pub fn into_precompile_result(self, gas: u64) -> PrecompileResult {
        let bytes: Bytes = match self {
            Self::Revert(bytes) => bytes,
            Self::Panic(kind) => Panic { code: U256::from(kind as u32) }.abi_encode().into(),
            Self::OutOfGas => {
                // revm 32.x: OutOfGas is returned as Err, not Ok-Halt
                return Err(PrecompileError::OutOfGas);
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
                bytes.extend_from_slice(error.as_bytes());
                bytes.into()
            }
        };
        // revm 32.x: revert is Ok with reverted=true
        Ok(PrecompileOutput::new_reverted(gas, bytes))
    }
}

/// Extension trait to convert `Result<T, BasePrecompileError>` into a [`PrecompileResult`].
pub trait IntoPrecompileResult<T> {
    /// Converts `self` into a [`PrecompileResult`] using `encode_ok` for the success path.
    fn into_precompile_result(
        self,
        gas: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult;
}

impl<T> IntoPrecompileResult<T> for Result<T> {
    fn into_precompile_result(
        self,
        gas: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult {
        match self {
            Ok(res) => Ok(PrecompileOutput::new(gas, encode_ok(res))),
            Err(err) => err.into_precompile_result(gas),
        }
    }
}
