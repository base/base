//! v0.6 EntryPoint revert data decoding
//!
//! v0.6 simulateHandleOp always reverts:
//! - Success: Reverts with `ExecutionResult` error containing simulation results
//! - Failure: Reverts with `FailedOp`, `SignatureValidationFailed`, etc.

use alloy_primitives::{Bytes, B256, U256};
use alloy_sol_types::{sol, SolError};

use super::types::{PanicCode, SimulationRevertDecoded, ValidationRevert, ValidationResultDecoded};

/// Marker for inner call revert on out of gas (from EntryPoint)
/// When the inner call runs out of gas, EntryPoint catches it and reverts with this sentinel
const INNER_OUT_OF_GAS: B256 = B256::new([
    0xde, 0xad, 0xde, 0xad, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
]);

// Define v0.6 error types using sol! macro
sol! {
    /// ExecutionResult error - indicates successful simulation in v0.6
    /// Selector: 0x8b7ac980
    #[derive(Debug)]
    error ExecutionResult(
        uint256 preOpGas,
        uint256 paid,
        uint48 validAfter,
        uint48 validUntil,
        bool targetSuccess,
        bytes targetResult
    );

    /// FailedOp error - validation failed
    #[derive(Debug)]
    error FailedOp(uint256 opIndex, string reason);

    /// FailedOpWithRevert error - validation failed with inner revert
    #[derive(Debug)]
    error FailedOpWithRevert(uint256 opIndex, string reason, bytes inner);

    /// SignatureValidationFailed error - aggregator signature check failed
    #[derive(Debug)]
    error SignatureValidationFailed(address aggregator);

    /// ReturnInfo struct - gas and time-range returned values
    /// (uint256 preOpGas, uint256 prefund, bool sigFailed, uint48 validAfter, uint48 validUntil, bytes paymasterContext)
    #[derive(Debug)]
    struct ReturnInfo {
        uint256 preOpGas;
        uint256 prefund;
        bool sigFailed;
        uint48 validAfter;
        uint48 validUntil;
        bytes paymasterContext;
    }

    /// StakeInfo struct - stake information
    /// (uint256 stake, uint256 unstakeDelaySec)
    #[derive(Debug)]
    struct StakeInfo {
        uint256 stake;
        uint256 unstakeDelaySec;
    }

    /// AggregatorStakeInfo struct - aggregator and its stake info
    #[derive(Debug)]
    struct AggregatorStakeInfo {
        address aggregator;
        StakeInfo stakeInfo;
    }

    /// ValidationResult error - from simulateValidation (not simulateHandleOp)
    /// Selector: 0xe0cff05f
    #[derive(Debug)]
    error ValidationResult(
        ReturnInfo returnInfo,
        StakeInfo senderInfo,
        StakeInfo factoryInfo,
        StakeInfo paymasterInfo
    );

    /// ValidationResultWithAggregation error - validation with aggregator
    #[derive(Debug)]
    error ValidationResultWithAggregation(
        ReturnInfo returnInfo,
        StakeInfo senderInfo,
        StakeInfo factoryInfo,
        StakeInfo paymasterInfo,
        AggregatorStakeInfo aggregatorInfo
    );

    /// CallPhaseReverted - from EntryPointSimulationsV06
    /// This is thrown when the call phase runs out of gas or reverts
    /// Selector: 0x462c71b2
    #[derive(Debug)]
    error CallPhaseReverted(bytes reason);

    /// Standard Solidity Error(string)
    #[derive(Debug)]
    error Error(string message);

    /// Standard Solidity Panic(uint256)
    #[derive(Debug)]
    error Panic(uint256 code);
}

/// Decode v0.6 simulation revert data
///
/// v0.6 always reverts from simulateHandleOp. The revert data tells us:
/// - `ExecutionResult`: Simulation succeeded
/// - `FailedOp`, `SignatureValidationFailed`: Validation failed
/// - `ValidationResult`: Unexpected (from simulateValidation, not simulateHandleOp)
/// - `INNER_OUT_OF_GAS` (0xdeaddead...): Inner call ran out of gas (AA95)
pub fn decode_v06_simulation_revert(revert_data: &Bytes) -> SimulationRevertDecoded {
    // Need at least 4 bytes for selector
    if revert_data.len() < 4 {
        return SimulationRevertDecoded::Unknown(revert_data.clone());
    }

    // Check for INNER_OUT_OF_GAS sentinel (0xdeaddead padded to 32 bytes)
    // This is returned when the inner call runs out of gas
    if revert_data.len() >= 32 {
        let mut sentinel = [0u8; 32];
        sentinel[..revert_data.len().min(32)].copy_from_slice(&revert_data[..revert_data.len().min(32)]);
        if B256::from(sentinel) == INNER_OUT_OF_GAS {
            return SimulationRevertDecoded::ValidationRevert(ValidationRevert::EntryPoint(
                "AA95 out of gas (inner call ran out of gas during simulation)".to_string(),
            ));
        }
    }

    // Try to decode ExecutionResult (success case)
    // Selector: 0x8b7ac980
    if let Ok(exec) = ExecutionResult::abi_decode(revert_data) {
        // Pack validAfter and validUntil into validation data format
        // Format: 20 bytes aggregator (0) | 6 bytes validUntil | 6 bytes validAfter
        let valid_until: u64 = exec.validUntil.to();
        let valid_after: u64 = exec.validAfter.to();
        let account_validation_data = (U256::from(valid_until) << 48) | U256::from(valid_after);

        return SimulationRevertDecoded::ExecutionResult(super::types::ExecutionResult {
            pre_op_gas: exec.preOpGas,
            paid: exec.paid,
            target_success: exec.targetSuccess,
            target_result: exec.targetResult,
            account_validation_data,
            paymaster_validation_data: U256::ZERO, // v0.6 doesn't return this separately
        });
    }

    // Try to decode validation failure errors
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

    // Handle CallPhaseReverted from EntryPointSimulationsV06
    // This means the call phase ran out of gas or reverted
    // Selector: 0x462c71b2
    if let Ok(call_reverted) = CallPhaseReverted::abi_decode(revert_data) {
        // The inner reason may contain more details about why it reverted
        let reason_str = if call_reverted.reason.is_empty() {
            "call phase reverted (out of gas or execution failure)".to_string()
        } else {
            format!(
                "call phase reverted: 0x{}",
                hex::encode(&call_reverted.reason)
            )
        };
        return SimulationRevertDecoded::ValidationRevert(ValidationRevert::EntryPoint(reason_str));
    }

    // Handle ValidationResult from simulateValidation
    // Selector: 0xe0cff05f
    if let Ok(result) = ValidationResult::abi_decode(revert_data) {
        return SimulationRevertDecoded::ValidationResult(ValidationResultDecoded {
            pre_op_gas: result.returnInfo.preOpGas,
            prefund: result.returnInfo.prefund,
            sig_failed: result.returnInfo.sigFailed,
            valid_after: result.returnInfo.validAfter.to::<u64>(),
            valid_until: result.returnInfo.validUntil.to::<u64>(),
            paymaster_context: result.returnInfo.paymasterContext,
            aggregator: None,
        });
    }

    // Handle ValidationResultWithAggregation from simulateValidation
    if let Ok(result) = ValidationResultWithAggregation::abi_decode(revert_data) {
        let aggregator = if result.aggregatorInfo.aggregator.is_zero() {
            None
        } else {
            Some(result.aggregatorInfo.aggregator)
        };
        return SimulationRevertDecoded::ValidationResult(ValidationResultDecoded {
            pre_op_gas: result.returnInfo.preOpGas,
            prefund: result.returnInfo.prefund,
            sig_failed: result.returnInfo.sigFailed,
            valid_after: result.returnInfo.validAfter.to::<u64>(),
            valid_until: result.returnInfo.validUntil.to::<u64>(),
            paymaster_context: result.returnInfo.paymasterContext,
            aggregator,
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

    // Unknown revert data
    SimulationRevertDecoded::Unknown(revert_data.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{bytes, Uint};

    #[test]
    fn test_decode_execution_result() {
        // Encode an ExecutionResult error (selector: 0x8b7ac980)
        let exec = ExecutionResult {
            preOpGas: U256::from(50000),
            paid: U256::from(100000),
            validAfter: Uint::<48, 1>::from(0u64),
            validUntil: Uint::<48, 1>::from(0xFFFFFFFFFFFFu64),
            targetSuccess: true,
            targetResult: Bytes::default(),
        };
        let encoded = exec.abi_encode();

        let decoded = decode_v06_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ExecutionResult(result) => {
                assert_eq!(result.pre_op_gas, U256::from(50000));
                assert_eq!(result.paid, U256::from(100000));
                assert!(result.target_success);
            }
            _ => panic!("Expected ExecutionResult, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_failed_op() {
        let failed = FailedOp {
            opIndex: U256::from(0),
            reason: "AA21 didn't pay prefund".to_string(),
        };
        let encoded = failed.abi_encode();

        let decoded = decode_v06_simulation_revert(&Bytes::from(encoded));
        match decoded {
            SimulationRevertDecoded::ValidationRevert(ValidationRevert::EntryPoint(msg)) => {
                assert!(msg.contains("AA21"));
            }
            _ => panic!("Expected ValidationRevert::EntryPoint, got {:?}", decoded),
        }
    }

    #[test]
    fn test_decode_unknown() {
        let unknown = bytes!("deadbeef");
        let decoded = decode_v06_simulation_revert(&unknown);
        assert!(matches!(decoded, SimulationRevertDecoded::Unknown(_)));
    }
}

