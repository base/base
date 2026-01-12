//! EntryPoint v0.6 Contract Definitions
//!
//! v0.6 originally reverts with ExecutionResult error. We use EntryPointSimulationsV06
//! with state override to return results like v0.7+ for unified handling.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::sol;

/// EntryPoint v0.6 address
pub const ENTRYPOINT_V06_ADDRESS: Address =
    alloy_primitives::address!("5FF137D4b0FDCD49DcA30c7CF57E578a026d2789");

/// EntryPointSimulationsV06 bytecode for state override
/// This bytecode includes simulateHandleOp that RETURNS instead of reverts
/// Source: contracts/solidity/v0_6/src/EntryPointSimulationsV06.sol
pub use super::bytecode::ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE as ENTRYPOINT_V06_SIMULATIONS_BYTECODE;

// Define the UserOperation struct and EntryPoint interface for v0.6
sol! {
    /// UserOperation for EntryPoint v0.6
    #[derive(Debug, Default)]
    struct UserOperationV06Packed {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        uint256 callGasLimit;
        uint256 verificationGasLimit;
        uint256 preVerificationGas;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        bytes paymasterAndData;
        bytes signature;
    }

    /// Return info from simulateHandleOp
    #[derive(Debug)]
    struct ExecutionResultV06 {
        uint256 preOpGas;
        uint256 paid;
        uint48 validAfter;
        uint48 validUntil;
        bool targetSuccess;
        bytes targetResult;
    }

    /// Revert reason for failed validation/execution
    #[derive(Debug)]
    error FailedOpV06(uint256 opIndex, string reason);

    /// Revert reason containing execution result (v0.6 style revert)
    #[derive(Debug)]
    error ExecutionResultRevert(
        uint256 preOpGas,
        uint256 paid,
        uint48 validAfter,
        uint48 validUntil,
        bool targetSuccess,
        bytes targetResult
    );

    /// Validation result info
    #[derive(Debug)]
    struct ValidationResultV06 {
        uint256 preOpGas;
        uint256 prefund;
        bool sigFailed;
        uint48 validAfter;
        uint48 validUntil;
        bytes paymasterContext;
    }

    /// Revert reason containing validation result (v0.6 style revert)
    #[derive(Debug)]
    error ValidationResultRevert(
        uint256 preOpGas,
        uint256 prefund,
        bool sigFailed,
        uint48 validAfter,
        uint48 validUntil,
        bytes paymasterContext
    );

    /// EntryPoint v0.6 interface (original - reverts with results)
    interface IEntryPointV06 {
        /// Simulate the full execution of a UserOperation
        /// @dev This function always reverts with ExecutionResultRevert on success
        function simulateHandleOp(
            UserOperationV06Packed calldata op,
            address target,
            bytes calldata targetCallData
        ) external;

        /// Simulate only the validation phase
        /// @dev This function always reverts with ValidationResultRevert on success
        function simulateValidation(
            UserOperationV06Packed calldata op
        ) external;

        /// Get the deposit for an account
        function balanceOf(address account) external view returns (uint256);

        /// Execute a batch of UserOperations
        function handleOps(
            UserOperationV06Packed[] calldata ops,
            address payable beneficiary
        ) external;
    }

    /// ExecutionResult for EntryPointSimulationsV06 (unified with v0.7+ format)
    /// This struct matches the return value from our custom EntryPointSimulationsV06 contract
    #[derive(Debug)]
    struct ExecutionResultV06Sim {
        uint256 preOpGas;
        uint256 paid;
        uint256 accountValidationData;    // packed validation data
        uint256 paymasterValidationData;  // packed validation data  
        bool targetSuccess;
        bytes targetResult;
    }

    /// EntryPointSimulationsV06 interface (deployed via state override, returns instead of reverts)
    interface IEntryPointSimulationsV06 {
        /// Simulate the full execution of a UserOperation
        /// @dev Unlike base simulateHandleOp, this RETURNS the result
        function simulateHandleOp(
            UserOperationV06Packed calldata op,
            address target,
            bytes calldata targetCallData
        ) external returns (ExecutionResultV06Sim memory);

        /// Simulate only the validation phase
        /// @dev Unlike base simulateValidation, this RETURNS the result
        function simulateValidation(
            UserOperationV06Packed calldata userOp
        ) external returns (ValidationResultV06 memory);
    }
}

/// Convert our RPC UserOperationV06 to the packed format for contract calls
pub fn pack_user_op_v06(
    sender: Address,
    nonce: U256,
    init_code: Bytes,
    call_data: Bytes,
    call_gas_limit: U256,
    verification_gas_limit: U256,
    pre_verification_gas: U256,
    max_fee_per_gas: U256,
    max_priority_fee_per_gas: U256,
    paymaster_and_data: Bytes,
    signature: Bytes,
) -> UserOperationV06Packed {
    UserOperationV06Packed {
        sender,
        nonce,
        initCode: init_code,
        callData: call_data,
        callGasLimit: call_gas_limit,
        verificationGasLimit: verification_gas_limit,
        preVerificationGas: pre_verification_gas,
        maxFeePerGas: max_fee_per_gas,
        maxPriorityFeePerGas: max_priority_fee_per_gas,
        paymasterAndData: paymaster_and_data,
        signature,
    }
}
