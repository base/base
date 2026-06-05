use alloc::string::{String, ToString};
use core::result;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{Panic, PanicKind, SolError, sol};

sol! {
    /// Precompile cannot be executed via delegatecall or callcode.
    error DelegateCallNotAllowed();
}
use revm::{
    context::journaled_state::JournalLoadError,
    precompile::{PrecompileError, PrecompileHalt, PrecompileOutput, PrecompileResult},
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
    pub fn into_precompile_result(
        self,
        gas: u64,
        state_gas: u64,
        reservoir: u64,
    ) -> PrecompileResult {
        let bytes: Bytes = match self {
            Self::Revert(bytes) => bytes,
            Self::Panic(kind) => Panic { code: U256::from(kind as u32) }.abi_encode().into(),
            Self::OutOfGas => {
                return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
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
        let mut out = PrecompileOutput::revert(gas, bytes, reservoir);
        out.state_gas_used = state_gas;
        Ok(out)
    }
}

/// Extension trait to convert `Result<T, BasePrecompileError>` into a [`PrecompileResult`].
pub trait IntoPrecompileResult<T> {
    /// Converts `self` into a [`PrecompileResult`] using `encode_ok` for the success path.
    fn into_precompile_result(
        self,
        gas: u64,
        state_gas: u64,
        reservoir: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult;
}

impl<T> IntoPrecompileResult<T> for Result<T> {
    fn into_precompile_result(
        self,
        gas: u64,
        state_gas: u64,
        reservoir: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult {
        match self {
            Ok(res) => {
                let mut out = PrecompileOutput::new(gas, encode_ok(res), reservoir);
                out.state_gas_used = state_gas;
                Ok(out)
            }
            Err(err) => err.into_precompile_result(gas, state_gas, reservoir),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolError;

    use super::*;

    #[test]
    fn delegate_call_not_allowed_encodes_to_typed_revert() {
        let expected: Bytes = DelegateCallNotAllowed {}.abi_encode().into();
        let result =
            BasePrecompileError::revert(DelegateCallNotAllowed {}).into_precompile_result(0, 0, 0);
        let output = result.unwrap();
        assert!(output.is_revert());
        assert_eq!(output.bytes, expected);
    }

    // --- BasePrecompileError::into_precompile_result ---

    #[test]
    fn base_error_revert_sets_reservoir_and_state_gas_used() {
        let result =
            BasePrecompileError::Revert(Bytes::from("err")).into_precompile_result(1000, 200, 750);
        let out = result.unwrap();
        assert!(out.is_revert());
        assert_eq!(out.gas_used, 1000);
        assert_eq!(out.state_gas_used, 200);
        assert_eq!(out.reservoir, 750);
        assert_eq!(out.bytes, Bytes::from("err"));
    }

    #[test]
    fn base_error_panic_sets_reservoir_and_state_gas_used() {
        let result = BasePrecompileError::Panic(PanicKind::UnderOverflow)
            .into_precompile_result(500, 100, 400);
        let out = result.unwrap();
        assert!(out.is_revert(), "panic encodes as a revert");
        assert_eq!(out.state_gas_used, 100);
        assert_eq!(out.reservoir, 400);
    }

    #[test]
    fn base_error_oog_halt_carries_remaining_reservoir() {
        let result = BasePrecompileError::OutOfGas.into_precompile_result(999, 0, 300);
        let out = result.unwrap();
        assert!(out.is_halt());
        // The reservoir value at OOG time must be returned so the EVM can account for it.
        assert_eq!(out.reservoir, 300);
    }

    #[test]
    fn base_error_oog_with_zero_reservoir() {
        let result = BasePrecompileError::OutOfGas.into_precompile_result(0, 0, 0);
        let out = result.unwrap();
        assert!(out.is_halt());
        assert_eq!(out.reservoir, 0);
    }

    #[test]
    fn base_error_static_call_violation_sets_reservoir() {
        let result = BasePrecompileError::StaticCallViolation.into_precompile_result(100, 50, 600);
        let out = result.unwrap();
        assert!(out.is_revert());
        assert_eq!(out.state_gas_used, 50);
        assert_eq!(out.reservoir, 600);
    }

    #[test]
    fn base_error_fatal_propagates_as_err() {
        let result =
            BasePrecompileError::Fatal("db error".into()).into_precompile_result(0, 0, 500);
        assert!(result.is_err(), "Fatal must not produce a PrecompileOutput");
    }

    // --- IntoPrecompileResult<T> for Result<T> ---

    #[test]
    fn into_precompile_result_success_sets_state_gas_and_reservoir() {
        let ok: Result<Bytes> = Ok(Bytes::from("output"));
        let result = ok.into_precompile_result(1500, 300, 800, |b| b);
        let out = result.unwrap();

        assert!(out.is_success());
        assert_eq!(out.gas_used, 1500);
        assert_eq!(out.state_gas_used, 300);
        assert_eq!(out.reservoir, 800);
        assert_eq!(out.bytes, Bytes::from("output"));
    }

    #[test]
    fn into_precompile_result_success_with_zero_reservoir() {
        let ok: Result<Bytes> = Ok(Bytes::new());
        let out = ok.into_precompile_result(100, 0, 0, |b| b).unwrap();

        assert!(out.is_success());
        assert_eq!(out.state_gas_used, 0);
        assert_eq!(out.reservoir, 0);
    }

    #[test]
    fn into_precompile_result_err_revert_propagates_reservoir_and_state_gas() {
        let err: Result<Bytes> = Err(BasePrecompileError::Revert(Bytes::from("revert")));
        let out = err.into_precompile_result(200, 50, 900, |b| b).unwrap();

        assert!(out.is_revert());
        assert_eq!(out.gas_used, 200);
        assert_eq!(out.state_gas_used, 50);
        assert_eq!(out.reservoir, 900);
    }

    #[test]
    fn into_precompile_result_err_oog_carries_reservoir() {
        let err: Result<Bytes> = Err(BasePrecompileError::OutOfGas);
        let out = err.into_precompile_result(0, 0, 1234, |b| b).unwrap();

        assert!(out.is_halt());
        assert_eq!(out.reservoir, 1234);
    }

    #[test]
    fn into_precompile_result_err_fatal_propagates_as_err() {
        let err: Result<Bytes> = Err(BasePrecompileError::Fatal("crash".into()));
        let result = err.into_precompile_result(0, 0, 0, |b| b);
        assert!(result.is_err());
    }

    #[test]
    fn into_precompile_result_encoder_applied_on_success() {
        let ok: Result<u64> = Ok(42u64);
        let out =
            ok.into_precompile_result(0, 0, 0, |v| Bytes::from(v.to_le_bytes().to_vec())).unwrap();

        assert!(out.is_success());
        assert_eq!(out.bytes, Bytes::from(42u64.to_le_bytes().to_vec()));
    }
}
