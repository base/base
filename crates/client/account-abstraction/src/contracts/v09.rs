//! EntryPoint v0.9 Contract Definitions
//!
//! v0.9 is ABI-compatible with v0.7 and v0.8 with some new features:
//!
//! - `paymasterSignature` field for parallelizable Paymaster signing
//! - Block number validity range (highest bit indicates block numbers)
//! - `initCode` ignored without reverting if Account already exists
//! - `getCurrentUserOpHash` function available during execution
//! - `EIP7702AccountInitialized` event for EIP-7702 delegation target
//!
//! See: https://github.com/eth-infinitism/account-abstraction/releases/tag/v0.9.0
//!
//! Simulation methods are in a separate EntryPointSimulations contract.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::sol;

/// EntryPoint v0.9 address
/// Source: https://github.com/eth-infinitism/account-abstraction/releases/tag/v0.9.0
pub const ENTRYPOINT_V09_ADDRESS: Address =
    alloy_primitives::address!("433709009B8330FDa32311DF1C2AFA402eD8D009");

/// SenderCreator v0.9 address
pub const SENDER_CREATOR_V09_ADDRESS: Address =
    alloy_primitives::address!("0A630a99Df908A81115A3022927Be82f9299987e");

/// EntryPointSimulations v0.9 bytecode for state override
/// This bytecode includes simulateHandleOp and simulateValidation methods
/// Source: https://github.com/eth-infinitism/account-abstraction/tree/v0.9.0/contracts/core/EntryPointSimulations.sol
///
/// The bytecode is compiled at build time from Solidity sources.
/// Run `./scripts/fetch-aa-contracts.sh` to download sources, then rebuild.
pub use super::bytecode::ENTRYPOINT_V09_SIMULATIONS_DEPLOYED_BYTECODE as ENTRYPOINT_V09_SIMULATIONS_BYTECODE;

// v0.9 uses the same packed format as v0.7/v0.8
sol! {
    /// PackedUserOperation for EntryPoint v0.9
    /// Same format as v0.7/v0.8
    #[derive(Debug, Default)]
    struct PackedUserOperationV09 {
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
    struct ExecutionResultV09 {
        uint256 preOpGas;
        uint256 paid;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bool targetSuccess;
        bytes targetResult;
    }

    /// Return value of simulateValidation
    #[derive(Debug)]
    struct ValidationResultV09 {
        uint256 preOpGas;
        uint256 prefund;
        uint256 accountValidationData;
        uint256 paymasterValidationData;
        bytes paymasterContext;
    }

    /// Revert reason for failed validation/execution
    #[derive(Debug)]
    error FailedOpV09(uint256 opIndex, string reason);

    /// Revert reason for failed validation/execution with revert bytes
    #[derive(Debug)]
    error FailedOpWithRevertV09(uint256 opIndex, string reason, bytes inner);

    /// New in v0.9: Event emitted when initCode is ignored (account already exists)
    #[derive(Debug)]
    event IgnoredInitCode(address indexed sender);

    /// New in v0.9: Event emitted when EIP-7702 account is initialized
    #[derive(Debug)]
    event EIP7702AccountInitialized(address indexed sender, address indexed delegate);

    /// EntryPoint v0.9 interface
    interface IEntryPointV09 {
        /// Execute a batch of UserOperations
        function handleOps(
            PackedUserOperationV09[] calldata ops,
            address payable beneficiary
        ) external;

        /// Get the deposit for an account
        function balanceOf(address account) external view returns (uint256);

        /// Get user operation hash
        function getUserOpHash(
            PackedUserOperationV09 calldata userOp
        ) external view returns (bytes32);

        /// New in v0.9: Get the current UserOperation hash during execution
        function getCurrentUserOpHash() external view returns (bytes32);
    }

    /// EntryPointSimulations v0.9 interface (deployed via state override)
    interface IEntryPointSimulationsV09 {
        /// Simulate the full execution of a UserOperation
        function simulateHandleOp(
            PackedUserOperationV09 calldata op,
            address target,
            bytes calldata targetCallData
        ) external returns (ExecutionResultV09 memory);

        /// Simulate only the validation phase
        function simulateValidation(
            PackedUserOperationV09 calldata userOp
        ) external returns (ValidationResultV09 memory);
    }
}

/// Re-export v0.7 packing functions since v0.9 uses the same format
pub use super::v07::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

/// Convert our RPC UserOperationV07 to the v0.9 packed format for contract calls
/// (v0.9 uses the same format as v0.7/v0.8)
pub fn pack_user_op_v09(
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
) -> PackedUserOperationV09 {
    // Pack initCode: factory (20 bytes) | factoryData
    let init_code = if factory.is_zero() {
        Bytes::default()
    } else {
        let mut ic = Vec::with_capacity(20 + factory_data.len());
        ic.extend_from_slice(factory.as_slice());
        ic.extend_from_slice(&factory_data);
        Bytes::from(ic)
    };

    PackedUserOperationV09 {
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
