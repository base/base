//! EntryPoint v0.7 Contract Definitions
//!
//! v0.7 moved simulation methods to a separate EntryPointSimulations contract.
//! We provide the simulation bytecode for state override deployment.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::sol;

/// EntryPoint v0.7 address
pub const ENTRYPOINT_V07_ADDRESS: Address =
    alloy_primitives::address!("0000000071727De22E5E9d8BAf0edAc6f37da032");

/// EntryPointSimulations v0.7 bytecode for state override
/// This bytecode includes simulateHandleOp and simulateValidation methods
/// Source: https://github.com/eth-infinitism/account-abstraction/blob/v0.7.0/contracts/core/EntryPointSimulations.sol
///
/// The bytecode is compiled at build time from Solidity sources.
/// Run `./scripts/fetch-aa-contracts.sh` to download sources, then rebuild.
pub use super::bytecode::ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE as ENTRYPOINT_V07_SIMULATIONS_BYTECODE;

// Define the PackedUserOperation struct and EntryPoint interface for v0.7
sol! {
    /// PackedUserOperation for EntryPoint v0.7+
    /// Gas limits and fees are packed into bytes32 values
    #[derive(Debug, Default)]
    struct PackedUserOperationV07 {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        bytes32 accountGasLimits;  // verificationGasLimit (16 bytes) | callGasLimit (16 bytes)
        uint256 preVerificationGas;
        bytes32 gasFees;           // maxPriorityFeePerGas (16 bytes) | maxFeePerGas (16 bytes)
        bytes paymasterAndData;
        bytes signature;
    }

    /// Return info from simulateHandleOp
    #[derive(Debug)]
    struct ExecutionResultV07 {
        uint256 preOpGas;
        uint256 paid;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bool targetSuccess;
        bytes targetResult;
    }

    /// Return value of simulateValidation
    #[derive(Debug)]
    struct ValidationResultV07 {
        uint256 preOpGas;
        uint256 prefund;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bytes paymasterContext;
    }

    /// Revert reason for failed validation/execution
    #[derive(Debug)]
    error FailedOpV07(uint256 opIndex, string reason);

    /// Revert reason for failed validation/execution with revert bytes
    #[derive(Debug)]
    error FailedOpWithRevertV07(uint256 opIndex, string reason, bytes inner);

    /// EntryPoint v0.7 interface
    interface IEntryPointV07 {
        /// Execute a batch of UserOperations
        function handleOps(
            PackedUserOperationV07[] calldata ops,
            address payable beneficiary
        ) external;

        /// Get the deposit for an account
        function balanceOf(address account) external view returns (uint256);

        /// Get user operation hash
        function getUserOpHash(
            PackedUserOperationV07 calldata userOp
        ) external view returns (bytes32);
    }

    /// EntryPointSimulations v0.7 interface (deployed via state override)
    interface IEntryPointSimulationsV07 {
        /// Simulate the full execution of a UserOperation
        function simulateHandleOp(
            PackedUserOperationV07 calldata op,
            address target,
            bytes calldata targetCallData
        ) external returns (ExecutionResultV07 memory);

        /// Simulate only the validation phase
        function simulateValidation(
            PackedUserOperationV07 calldata userOp
        ) external returns (ValidationResultV07 memory);
    }
}

/// Pack gas limits into bytes32 format for v0.7
/// Format: verificationGasLimit (16 bytes) | callGasLimit (16 bytes)
pub fn pack_account_gas_limits(verification_gas_limit: U256, call_gas_limit: U256) -> [u8; 32] {
    let mut result = [0u8; 32];
    // verificationGasLimit in upper 16 bytes
    let vgl_bytes: [u8; 32] = verification_gas_limit.to_be_bytes();
    result[..16].copy_from_slice(&vgl_bytes[16..32]);
    // callGasLimit in lower 16 bytes
    let cgl_bytes: [u8; 32] = call_gas_limit.to_be_bytes();
    result[16..32].copy_from_slice(&cgl_bytes[16..32]);
    result
}

/// Pack gas fees into bytes32 format for v0.7
/// Format: maxPriorityFeePerGas (16 bytes) | maxFeePerGas (16 bytes)
pub fn pack_gas_fees(max_priority_fee_per_gas: U256, max_fee_per_gas: U256) -> [u8; 32] {
    let mut result = [0u8; 32];
    // maxPriorityFeePerGas in upper 16 bytes
    let mpf_bytes: [u8; 32] = max_priority_fee_per_gas.to_be_bytes();
    result[..16].copy_from_slice(&mpf_bytes[16..32]);
    // maxFeePerGas in lower 16 bytes
    let mfg_bytes: [u8; 32] = max_fee_per_gas.to_be_bytes();
    result[16..32].copy_from_slice(&mfg_bytes[16..32]);
    result
}

/// Pack paymaster and data for v0.7
/// Format: paymaster (20 bytes) | paymasterVerificationGasLimit (16 bytes) | paymasterPostOpGasLimit (16 bytes) | paymasterData
pub fn pack_paymaster_and_data(
    paymaster: Address,
    paymaster_verification_gas_limit: U256,
    paymaster_post_op_gas_limit: U256,
    paymaster_data: Bytes,
) -> Bytes {
    if paymaster.is_zero() {
        return Bytes::default();
    }

    let mut result = Vec::with_capacity(20 + 16 + 16 + paymaster_data.len());

    // Paymaster address (20 bytes)
    result.extend_from_slice(paymaster.as_slice());

    // paymasterVerificationGasLimit (16 bytes)
    let pvgl_bytes: [u8; 32] = paymaster_verification_gas_limit.to_be_bytes();
    result.extend_from_slice(&pvgl_bytes[16..32]);

    // paymasterPostOpGasLimit (16 bytes)
    let ppol_bytes: [u8; 32] = paymaster_post_op_gas_limit.to_be_bytes();
    result.extend_from_slice(&ppol_bytes[16..32]);

    // paymasterData
    result.extend_from_slice(&paymaster_data);

    Bytes::from(result)
}

/// Convert our RPC UserOperationV07 to the packed format for contract calls
pub fn pack_user_op_v07(
    sender: Address,
    nonce: U256,
    factory: Address,
    factory_data: Bytes,
    call_data: Bytes,
    call_gas_limit: U256,
    verification_gas_limit: U256,
    pre_verification_gas: U256,
    max_fee_per_gas: U256,
    max_priority_fee_per_gas: U256,
    paymaster: Address,
    paymaster_verification_gas_limit: U256,
    paymaster_post_op_gas_limit: U256,
    paymaster_data: Bytes,
    signature: Bytes,
) -> PackedUserOperationV07 {
    // Pack initCode: factory (20 bytes) | factoryData
    let init_code = if factory.is_zero() {
        Bytes::default()
    } else {
        let mut ic = Vec::with_capacity(20 + factory_data.len());
        ic.extend_from_slice(factory.as_slice());
        ic.extend_from_slice(&factory_data);
        Bytes::from(ic)
    };

    PackedUserOperationV07 {
        sender,
        nonce,
        initCode: init_code,
        callData: call_data,
        accountGasLimits: pack_account_gas_limits(verification_gas_limit, call_gas_limit).into(),
        preVerificationGas: pre_verification_gas,
        gasFees: pack_gas_fees(max_priority_fee_per_gas, max_fee_per_gas).into(),
        paymasterAndData: pack_paymaster_and_data(
            paymaster,
            paymaster_verification_gas_limit,
            paymaster_post_op_gas_limit,
            paymaster_data,
        ),
        signature,
    }
}
