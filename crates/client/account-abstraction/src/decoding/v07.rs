//! v0.7+ EntryPoint revert data decoding
//!
//! v0.7+ simulateHandleOp returns ExecutionResult on success.
//! Reverts only occur on validation failure.
//!
//! This decoder is also used for v0.8 which has the same error format.

use alloy_primitives::Bytes;
use alloy_sol_types::{sol, SolError};

use super::types::{PanicCode, SimulationRevertDecoded, ValidationRevert};

// Define v0.7 error types using sol! macro
sol! {
    /// FailedOp error - validation failed
    #[derive(Debug)]
    error FailedOp(uint256 opIndex, string reason);

    /// FailedOpWithRevert error - validation failed with inner revert
    #[derive(Debug)]
    error FailedOpWithRevert(uint256 opIndex, string reason, bytes inner);

    /// SignatureValidationFailed error - aggregator signature check failed
    #[derive(Debug)]
    error SignatureValidationFailed(address aggregator);

    /// PostOpReverted error - postOp callback failed
    #[derive(Debug)]
    error PostOpReverted(bytes returnData);

    /// Standard Solidity Error(string)
    #[derive(Debug)]
    error Error(string message);

    /// Standard Solidity Panic(uint256)
    #[derive(Debug)]
    error Panic(uint256 code);
}

/// Decode v0.7/v0.8 simulation revert data
///
/// For v0.7+, simulateHandleOp returns ExecutionResult on success.
/// This function only decodes the failure cases (reverts).
pub fn decode_v07_simulation_revert(revert_data: &Bytes) -> SimulationRevertDecoded {
    // Need at least 4 bytes for selector
    if revert_data.len() < 4 {
        return SimulationRevertDecoded::Unknown(revert_data.clone());
    }

    // v0.7 doesn't encode execution results in revert data
    // Any revert is a validation failure

    if let Ok(failed) = FailedOp::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::failed_op(
            failed.opIndex,
            failed.reason,
        ));
    }

    if let Ok(failed) = FailedOpWithRevert::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::failed_op_with_revert(
            failed.opIndex,
            failed.reason,
            failed.inner,
        ));
    }

    if let Ok(sig_failed) = SignatureValidationFailed::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::SignatureValidationFailed {
            aggregator: sig_failed.aggregator,
        });
    }

    if let Ok(post_op) = PostOpReverted::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::OperationRevert {
            reason: "PostOp reverted".to_string(),
            inner: post_op.returnData,
        });
    }

    // Try standard Solidity errors
    if let Ok(err) = Error::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::Revert(err.message));
    }

    if let Ok(panic) = Panic::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::Panic(
            PanicCode::from_code(panic.code),
        ));
    }

    // Unknown revert - treat as validation failure
    SimulationRevertDecoded::ValidationRevert(ValidationRevert::Unknown(revert_data.clone()))
}

/// Decode raw revert data into a ValidationRevert
///
/// This is a convenience function for cases where you know the call failed
/// and just want the validation revert reason.
pub fn decode_validation_revert(revert_data: &Bytes) -> ValidationRevert {
    match decode_v07_simulation_revert(revert_data) {
        SimulationRevertDecoded::ValidationRevert(v) => v,
        SimulationRevertDecoded::ExecutionResult(_) => {
            // This shouldn't happen for v0.7+, but handle it gracefully
            ValidationRevert::EntryPoint("Unexpected ExecutionResult in revert".to_string())
        }
        SimulationRevertDecoded::ValidationResult(_) => {
            // ValidationResult is a v0.6 type, shouldn't appear in v0.7 decoding
            ValidationRevert::EntryPoint("Unexpected ValidationResult in v0.7 revert".to_string())
        }
        SimulationRevertDecoded::Unknown(data) => ValidationRevert::Unknown(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes, U256};

    #[test]
    fn test_decode_failed_op() {
        let failed = FailedOp {
            opIndex: U256::from(0),
            reason: "AA24 signature error".to_string(),
        };
        let encoded = failed.abi_encode();

        let decoded = decode_v07_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::EntryPoint(msg)) => {
                assert!(msg.contains("AA24"));
            }
            _ => panic!("Expected ValidationRevert::EntryPoint, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_failed_op_with_revert() {
        let failed = FailedOpWithRevert {
            opIndex: U256::from(0),
            reason: "AA33 reverted".to_string(),
            inner: bytes!("08c379a0").into(),
        };
        let encoded = failed.abi_encode();

        let decoded = decode_v07_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::OperationRevert {
                reason,
                inner,
            }) => {
                assert!(reason.contains("AA33"));
                assert!(!inner.is_empty());
            }
            _ => panic!("Expected ValidationRevert::OperationRevert, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_signature_validation_failed() {
        let sig_failed = SignatureValidationFailed {
            aggregator: address!("1234567890123456789012345678901234567890"),
        };
        let encoded = sig_failed.abi_encode();

        let decoded = decode_v07_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::SignatureValidationFailed {
                aggregator,
            }) => {
                assert_eq!(aggregator, address!("1234567890123456789012345678901234567890"));
            }
            _ => panic!("Expected SignatureValidationFailed, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_standard_revert() {
        let err = Error {
            message: "Not authorized".to_string(),
        };
        let encoded = err.abi_encode();

        let decoded = decode_v07_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::Revert(msg)) => {
                assert_eq!(msg, "Not authorized");
            }
            _ => panic!("Expected Revert, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_panic() {
        let panic_err = Panic {
            code: U256::from(0x01), // Assert failed
        };
        let encoded = panic_err.abi_encode();

        let decoded = decode_v07_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::Panic(code)) => {
                assert_eq!(code, PanicCode::AssertFailed);
            }
            _ => panic!("Expected Panic, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_unknown() {
        let unknown = bytes!("deadbeef");
        let decoded = decode_v07_simulation_revert(&unknown);
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::Unknown(_)) => {}
            _ => panic!("Expected Unknown, got {:?}", decoded),
        }
    }
}

