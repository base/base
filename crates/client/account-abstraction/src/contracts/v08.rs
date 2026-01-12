//! EntryPoint v0.8 Contract Definitions
//!
//! v0.8 is similar to v0.7 with some improvements.
//! Simulation methods are in a separate EntryPointSimulations contract.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::sol;

/// EntryPoint v0.8 address
pub const ENTRYPOINT_V08_ADDRESS: Address =
    alloy_primitives::address!("4337084D9e255Ff0702461CF8895CE9E3B5Ff108");

/// EntryPointSimulations v0.8 bytecode for state override
/// This bytecode includes simulateHandleOp and simulateValidation methods
/// Source: https://github.com/eth-infinitism/account-abstraction/blob/v0.8.0/contracts/core/EntryPointSimulations.sol
///
/// The bytecode is compiled at build time from Solidity sources.
/// Run `./scripts/fetch-aa-contracts.sh` to download sources, then rebuild.
pub use super::bytecode::ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE as ENTRYPOINT_V08_SIMULATIONS_BYTECODE;

// v0.8 uses the same packed format as v0.7
sol! {
    /// PackedUserOperation for EntryPoint v0.8
    /// Same format as v0.7
    #[derive(Debug, Default)]
    struct PackedUserOperationV08 {
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
    struct ExecutionResultV08 {
        uint256 preOpGas;
        uint256 paid;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bool targetSuccess;
        bytes targetResult;
    }

    /// Return value of simulateValidation
    #[derive(Debug)]
    struct ValidationResultV08 {
        uint256 preOpGas;
        uint256 prefund;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bytes paymasterContext;
    }

    /// Revert reason for failed validation/execution
    #[derive(Debug)]
    error FailedOpV08(uint256 opIndex, string reason);

    /// Revert reason for failed validation/execution with revert bytes
    #[derive(Debug)]
    error FailedOpWithRevertV08(uint256 opIndex, string reason, bytes inner);

    /// EntryPoint v0.8 interface
    interface IEntryPointV08 {
        /// Execute a batch of UserOperations
        function handleOps(
            PackedUserOperationV08[] calldata ops,
            address payable beneficiary
        ) external;

        /// Get the deposit for an account
        function balanceOf(address account) external view returns (uint256);

        /// Get user operation hash
        function getUserOpHash(
            PackedUserOperationV08 calldata userOp
        ) external view returns (bytes32);
    }

    /// EntryPointSimulations v0.8 interface (deployed via state override)
    interface IEntryPointSimulationsV08 {
        /// Simulate the full execution of a UserOperation
        function simulateHandleOp(
            PackedUserOperationV08 calldata op,
            address target,
            bytes calldata targetCallData
        ) external returns (ExecutionResultV08 memory);

        /// Simulate only the validation phase
        function simulateValidation(
            PackedUserOperationV08 calldata userOp
        ) external returns (ValidationResultV08 memory);
    }
}

/// Re-export v0.7 packing functions since v0.8 uses the same format
pub use super::v07::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

/// Convert our RPC UserOperationV07 to the v0.8 packed format for contract calls
/// (v0.8 uses the same format as v0.7)
pub fn pack_user_op_v08(
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
) -> PackedUserOperationV08 {
    // Pack initCode: factory (20 bytes) | factoryData
    let init_code = if factory.is_zero() {
        Bytes::default()
    } else {
        let mut ic = Vec::with_capacity(20 + factory_data.len());
        ic.extend_from_slice(factory.as_slice());
        ic.extend_from_slice(&factory_data);
        Bytes::from(ic)
    };

    PackedUserOperationV08 {
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
