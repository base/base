//! Core types for simulation result decoding

use alloy_primitives::{Address, Bytes, U256};
use std::fmt;

/// Decoded result from a simulation revert
///
/// This represents the different outcomes from decoding simulation revert data.
#[derive(Debug, Clone)]
pub enum SimulationRevertDecoded {
    /// Successful execution - the simulation completed
    /// (For v0.6, this comes from the ExecutionResult revert)
    ExecutionResult(ExecutionResult),

    /// Successful validation - from v0.6 simulateValidation
    /// (This comes from the ValidationResult revert, not simulateHandleOp)
    ValidationResult(ValidationResultDecoded),

    /// Validation failed - the UserOp was rejected
    ValidationRevert(ValidationRevert),

    /// Unknown revert data that couldn't be decoded
    Unknown(Bytes),
}

impl SimulationRevertDecoded {
    /// Returns `Some(ExecutionResult)` if this is a successful execution
    pub fn as_execution_result(&self) -> Option<&ExecutionResult> {
        match self {
            Self::ExecutionResult(r) => Some(r),
            _ => None,
        }
    }

    /// Returns `Some(ValidationResultDecoded)` if this is a successful validation result
    pub fn as_validation_result(&self) -> Option<&ValidationResultDecoded> {
        match self {
            Self::ValidationResult(r) => Some(r),
            _ => None,
        }
    }

    /// Returns `Some(ValidationRevert)` if this is a validation failure
    pub fn as_validation_revert(&self) -> Option<&ValidationRevert> {
        match self {
            Self::ValidationRevert(r) => Some(r),
            _ => None,
        }
    }

    /// Returns true if this represents a successful execution or validation
    pub fn is_success(&self) -> bool {
        matches!(self, Self::ExecutionResult(_) | Self::ValidationResult(_))
    }
}

/// Result of a successful simulation execution
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Gas used during pre-operation phase (validation + setup)
    pub pre_op_gas: U256,
    /// Total gas paid
    pub paid: U256,
    /// Whether the account call (target) succeeded
    pub target_success: bool,
    /// Return data from the account call
    pub target_result: Bytes,
    /// Packed validation data from account (contains validAfter/validUntil/aggregator)
    pub account_validation_data: U256,
    /// Packed validation data from paymaster
    pub paymaster_validation_data: U256,
}

/// Result of a successful v0.6 validation (from simulateValidation, not simulateHandleOp)
/// This is returned when calling simulateValidation directly.
#[derive(Debug, Clone)]
pub struct ValidationResultDecoded {
    /// Gas used during pre-operation phase
    pub pre_op_gas: U256,
    /// Required prefund amount
    pub prefund: U256,
    /// Whether signature validation failed
    pub sig_failed: bool,
    /// Timestamp after which the operation is valid
    pub valid_after: u64,
    /// Timestamp until which the operation is valid
    pub valid_until: u64,
    /// Context returned by paymaster (empty if no paymaster)
    pub paymaster_context: Bytes,
    /// Optional aggregator address (from ValidationResultWithAggregation)
    pub aggregator: Option<Address>,
}

impl ExecutionResult {
    /// Extract validAfter timestamp from account validation data
    /// Format: 20 bytes aggregator | 6 bytes validUntil | 6 bytes validAfter
    pub fn valid_after(&self) -> u64 {
        // validAfter is in the lowest 6 bytes (48 bits)
        let masked: U256 = self.account_validation_data & U256::from(0xFFFFFFFFFFFFu64);
        // Convert to u64 by taking the low limb
        masked.as_limbs()[0]
    }

    /// Extract validUntil timestamp from account validation data
    pub fn valid_until(&self) -> u64 {
        // validUntil is in bytes 6-12 (48 bits shifted by 48)
        let shifted: U256 = self.account_validation_data >> 48;
        let masked: U256 = shifted & U256::from(0xFFFFFFFFFFFFu64);
        // Convert to u64 by taking the low limb
        masked.as_limbs()[0]
    }

    /// Check if signature validation failed (aggregator == address(1))
    pub fn signature_failed(&self) -> bool {
        let aggregator: U256 = self.account_validation_data >> 96;
        aggregator == U256::from(1)
    }
}

/// Validation failure reason
///
/// This enum represents all the ways a UserOperation validation can fail.
#[derive(Debug, Clone)]
pub enum ValidationRevert {
    /// EntryPoint rejected the operation with a reason code (AA** errors)
    EntryPoint(String),

    /// Operation failed with additional inner revert data
    /// (FailedOpWithRevert - contains nested error)
    OperationRevert {
        reason: String,
        inner: Bytes,
    },

    /// Aggregator signature validation failed
    SignatureValidationFailed {
        aggregator: Address,
    },

    /// Standard Solidity revert with message
    Revert(String),

    /// Solidity panic (assert failure, division by zero, etc.)
    Panic(PanicCode),

    /// Unknown revert that couldn't be decoded
    Unknown(Bytes),
}

impl ValidationRevert {
    /// Create from a FailedOp error
    pub fn failed_op(op_index: U256, reason: String) -> Self {
        // Include op_index in the message for context
        Self::EntryPoint(format!("FailedOp({}, {})", op_index, reason))
    }

    /// Create from a FailedOpWithRevert error
    pub fn failed_op_with_revert(op_index: U256, reason: String, inner: Bytes) -> Self {
        Self::OperationRevert {
            reason: format!("FailedOp({}, {})", op_index, reason),
            inner,
        }
    }

    /// Get a user-friendly error message
    pub fn message(&self) -> String {
        match self {
            Self::EntryPoint(msg) => msg.clone(),
            Self::OperationRevert { reason, inner } => {
                if inner.is_empty() {
                    reason.clone()
                } else {
                    format!("{} (inner: 0x{})", reason, hex::encode(inner))
                }
            }
            Self::SignatureValidationFailed { aggregator } => {
                format!("Aggregator signature validation failed: {}", aggregator)
            }
            Self::Revert(msg) => format!("Revert: {}", msg),
            Self::Panic(code) => format!("Panic: {}", code),
            Self::Unknown(data) => format!("Unknown revert: 0x{}", hex::encode(data)),
        }
    }
}

impl fmt::Display for ValidationRevert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for ValidationRevert {}

/// Solidity panic codes
///
/// See: https://docs.soliditylang.org/en/latest/control-structures.html#panic-via-assert-and-error-via-require
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicCode {
    /// Generic compiler inserted panic
    Generic,
    /// Assert failed
    AssertFailed,
    /// Arithmetic overflow/underflow
    ArithmeticOverflow,
    /// Division by zero
    DivisionByZero,
    /// Invalid enum value
    InvalidEnumValue,
    /// Invalid storage array access
    InvalidStorageAccess,
    /// Pop on empty array
    PopEmptyArray,
    /// Array index out of bounds
    ArrayOutOfBounds,
    /// Out of memory
    OutOfMemory,
    /// Zero-initialized function pointer
    ZeroFunctionPointer,
    /// Unknown panic code
    Unknown(U256),
}

impl PanicCode {
    /// Create from a uint256 panic code
    pub fn from_code(code: U256) -> Self {
        match code.to::<u64>() {
            0x00 => Self::Generic,
            0x01 => Self::AssertFailed,
            0x11 => Self::ArithmeticOverflow,
            0x12 => Self::DivisionByZero,
            0x21 => Self::InvalidEnumValue,
            0x22 => Self::InvalidStorageAccess,
            0x31 => Self::PopEmptyArray,
            0x32 => Self::ArrayOutOfBounds,
            0x41 => Self::OutOfMemory,
            0x51 => Self::ZeroFunctionPointer,
            _ => Self::Unknown(code),
        }
    }
}

impl fmt::Display for PanicCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic => write!(f, "generic panic"),
            Self::AssertFailed => write!(f, "assert failed"),
            Self::ArithmeticOverflow => write!(f, "arithmetic overflow/underflow"),
            Self::DivisionByZero => write!(f, "division by zero"),
            Self::InvalidEnumValue => write!(f, "invalid enum value"),
            Self::InvalidStorageAccess => write!(f, "invalid storage access"),
            Self::PopEmptyArray => write!(f, "pop on empty array"),
            Self::ArrayOutOfBounds => write!(f, "array index out of bounds"),
            Self::OutOfMemory => write!(f, "out of memory"),
            Self::ZeroFunctionPointer => write!(f, "zero-initialized function pointer"),
            Self::Unknown(code) => write!(f, "unknown panic code: {}", code),
        }
    }
}

