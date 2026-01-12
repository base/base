//! EntryPoint Version Traits and Implementations
//!
//! Provides version-specific logic for simulation and gas estimation.

use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::state::StateOverride;
use alloy_sol_types::SolCall;

use crate::contracts::{
    create_code_override,
    // v0.6 - uses Pimlico-style code override (still reverts, but better error messages)
    IEntryPointV06, UserOperationV06Packed, ENTRYPOINT_V06_ADDRESS,
    ENTRYPOINT_V06_SIMULATIONS_BYTECODE,
    // v0.7
    IEntryPointSimulationsV07, PackedUserOperationV07, ENTRYPOINT_V07_ADDRESS,
    ENTRYPOINT_V07_SIMULATIONS_BYTECODE,
    // v0.8
    IEntryPointSimulationsV08, PackedUserOperationV08, ENTRYPOINT_V08_ADDRESS,
    ENTRYPOINT_V08_SIMULATIONS_BYTECODE,
};

use crate::rpc::{UserOperation, UserOperationV07};

/// Supported EntryPoint versions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointVersion {
    V06,
    V07,
    V08,
}

impl EntryPointVersion {
    /// Get the contract address for this version
    pub fn address(&self) -> Address {
        match self {
            Self::V06 => ENTRYPOINT_V06_ADDRESS,
            Self::V07 => ENTRYPOINT_V07_ADDRESS,
            Self::V08 => ENTRYPOINT_V08_ADDRESS,
        }
    }

    /// Check if this version requires a simulation bytecode override
    /// v0.6 has private internal functions and reverts with ExecutionResult error
    /// v0.7+ use EntryPointSimulations contract with state override
    pub fn needs_simulation_override(&self) -> bool {
        match self {
            Self::V06 => false, // v0.6 reverts with ExecutionResult - parse revert data
            Self::V07 | Self::V08 => true, // v0.7+ use EntryPointSimulations contract
        }
    }

    /// Check if this version reverts with execution result (vs returning it)
    pub fn simulation_reverts(&self) -> bool {
        match self {
            Self::V06 => true,  // v0.6 simulateHandleOp always reverts
            Self::V07 | Self::V08 => false, // v0.7+ return ExecutionResult
        }
    }
}

/// Result of a simulation call
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Gas used during pre-operation phase (validation)
    pub pre_op_gas: U256,
    /// Total gas paid
    pub paid: U256,
    /// Whether the target call succeeded
    pub target_success: bool,
    /// Result data from target call
    pub target_result: Bytes,
    /// Validation data from account (contains validAfter/validUntil)
    pub account_validation_data: Option<U256>,
    /// Validation data from paymaster
    pub paymaster_validation_data: Option<U256>,
}

/// Calldata and state override for simulation
#[derive(Debug, Clone)]
pub struct SimulationCallData {
    /// The target address to call
    pub to: Address,
    /// The calldata for the simulation
    pub data: Bytes,
    /// State overrides needed for simulation (if any)
    pub state_override: Option<StateOverride>,
}

/// Resolver for EntryPoint version-specific operations
pub struct EntryPointVersionResolver;

impl EntryPointVersionResolver {
    /// Build simulation calldata for simulateHandleOp
    ///
    /// # Arguments
    /// * `version` - The EntryPoint version
    /// * `user_op` - The user operation to simulate
    /// * `target` - Target address for the simulation callback (usually zero address)
    /// * `target_call_data` - Calldata for the simulation callback (usually empty)
    pub fn build_simulate_handle_op_calldata(
        version: EntryPointVersion,
        user_op: &UserOperation,
        target: Address,
        target_call_data: Bytes,
    ) -> Result<SimulationCallData, String> {
        match version {
            EntryPointVersion::V06 => {
                Self::build_v06_simulate_handle_op(user_op, target, target_call_data)
            }
            EntryPointVersion::V07 => {
                Self::build_v07_simulate_handle_op(user_op, target, target_call_data)
            }
            EntryPointVersion::V08 => {
                Self::build_v08_simulate_handle_op(user_op, target, target_call_data)
            }
        }
    }

    /// Build v0.6 simulateHandleOp calldata
    /// Uses Pimlico-style EntryPointSimulationsV06 code override for better error messages
    /// Note: v0.6 still REVERTS with ExecutionResult - we parse the revert data
    fn build_v06_simulate_handle_op(
        user_op: &UserOperation,
        target: Address,
        target_call_data: Bytes,
    ) -> Result<SimulationCallData, String> {
        let op = match user_op {
            UserOperation::V06(op) => op,
            UserOperation::V07(_) => {
                return Err("Cannot use v0.7 UserOperation with v0.6 EntryPoint".to_string())
            }
        };

        let packed = UserOperationV06Packed {
            sender: op.sender,
            nonce: op.nonce,
            initCode: op.init_code.clone(),
            callData: op.call_data.clone(),
            callGasLimit: op.call_gas_limit,
            verificationGasLimit: op.verification_gas_limit,
            preVerificationGas: op.pre_verification_gas,
            maxFeePerGas: op.max_fee_per_gas,
            maxPriorityFeePerGas: op.max_priority_fee_per_gas,
            paymasterAndData: op.paymaster_and_data.clone(),
            signature: op.signature.clone(),
        };

        // Use the original v0.6 interface - simulateHandleOp still reverts with ExecutionResult
        let call = IEntryPointV06::simulateHandleOpCall {
            op: packed,
            target,
            targetCallData: target_call_data,
        };

        // Build state override with Pimlico-style simulation bytecode
        // This gives us better error messages (CallPhaseReverted, FailedOpWithRevert)
        // but still reverts with ExecutionResult on success
        let bytecode_len = ENTRYPOINT_V06_SIMULATIONS_BYTECODE.len();
        tracing::debug!(
            target: "aa-entrypoint",
            bytecode_len = bytecode_len,
            has_bytecode = !ENTRYPOINT_V06_SIMULATIONS_BYTECODE.is_empty(),
            "Building v0.6 simulateHandleOp calldata"
        );

        let state_override = if !ENTRYPOINT_V06_SIMULATIONS_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V06_SIMULATIONS_BYTECODE).map_err(|e| {
                    format!("Failed to decode v0.6 simulation bytecode: {}", e)
                })?);
            tracing::debug!(
                target: "aa-entrypoint",
                decoded_bytecode_len = bytecode.len(),
                "Using v0.6 simulation bytecode state override"
            );
            Some(create_code_override(ENTRYPOINT_V06_ADDRESS, bytecode))
        } else {
            tracing::warn!(
                target: "aa-entrypoint",
                "No v0.6 simulation bytecode available - using original EntryPoint (may not work properly)"
            );
            // Fallback: use original EntryPoint (no code override)
            None
        };

        Ok(SimulationCallData {
            to: ENTRYPOINT_V06_ADDRESS,
            data: Bytes::from(call.abi_encode()),
            state_override,
        })
    }

    /// Build v0.7 simulateHandleOp calldata
    fn build_v07_simulate_handle_op(
        user_op: &UserOperation,
        target: Address,
        target_call_data: Bytes,
    ) -> Result<SimulationCallData, String> {
        let op = match user_op {
            UserOperation::V07(op) => op,
            UserOperation::V06(_) => {
                return Err("Cannot use v0.6 UserOperation with v0.7 EntryPoint".to_string())
            }
        };

        let packed = Self::pack_v07_user_op(op);

        let call = IEntryPointSimulationsV07::simulateHandleOpCall {
            op: packed,
            target,
            targetCallData: target_call_data,
        };

        // Build state override with simulation bytecode
        let state_override = if !ENTRYPOINT_V07_SIMULATIONS_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V07_SIMULATIONS_BYTECODE).map_err(|e| {
                    format!("Failed to decode v0.7 simulation bytecode: {}", e)
                })?);
            Some(create_code_override(ENTRYPOINT_V07_ADDRESS, bytecode))
        } else {
            // Fallback: call without override (will fail if EntryPoint doesn't have simulateHandleOp)
            None
        };

        Ok(SimulationCallData {
            to: ENTRYPOINT_V07_ADDRESS,
            data: Bytes::from(call.abi_encode()),
            state_override,
        })
    }

    /// Build v0.8 simulateHandleOp calldata
    fn build_v08_simulate_handle_op(
        user_op: &UserOperation,
        target: Address,
        target_call_data: Bytes,
    ) -> Result<SimulationCallData, String> {
        let op = match user_op {
            UserOperation::V07(op) => op, // v0.8 uses same format as v0.7
            UserOperation::V06(_) => {
                return Err("Cannot use v0.6 UserOperation with v0.8 EntryPoint".to_string())
            }
        };

        let packed = Self::pack_v08_user_op(op);

        let call = IEntryPointSimulationsV08::simulateHandleOpCall {
            op: packed,
            target,
            targetCallData: target_call_data,
        };

        // Build state override with simulation bytecode
        let state_override = if !ENTRYPOINT_V08_SIMULATIONS_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V08_SIMULATIONS_BYTECODE).map_err(|e| {
                    format!("Failed to decode v0.8 simulation bytecode: {}", e)
                })?);
            Some(create_code_override(ENTRYPOINT_V08_ADDRESS, bytecode))
        } else {
            // Fallback: call without override (will fail if EntryPoint doesn't have simulateHandleOp)
            None
        };

        Ok(SimulationCallData {
            to: ENTRYPOINT_V08_ADDRESS,
            data: Bytes::from(call.abi_encode()),
            state_override,
        })
    }

    /// Pack a v0.7 UserOperation into the on-chain format
    fn pack_v07_user_op(op: &UserOperationV07) -> PackedUserOperationV07 {
        use crate::contracts::v07::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

        // Pack initCode: factory (20 bytes) | factoryData
        let init_code = if op.factory.is_zero() {
            Bytes::default()
        } else {
            let mut ic = Vec::with_capacity(20 + op.factory_data.len());
            ic.extend_from_slice(op.factory.as_slice());
            ic.extend_from_slice(&op.factory_data);
            Bytes::from(ic)
        };

        PackedUserOperationV07 {
            sender: op.sender,
            nonce: op.nonce,
            initCode: init_code,
            callData: op.call_data.clone(),
            accountGasLimits: pack_account_gas_limits(op.verification_gas_limit, op.call_gas_limit)
                .into(),
            preVerificationGas: op.pre_verification_gas,
            gasFees: pack_gas_fees(op.max_priority_fee_per_gas, op.max_fee_per_gas).into(),
            paymasterAndData: pack_paymaster_and_data(
                op.paymaster,
                op.paymaster_verification_gas_limit,
                op.paymaster_post_op_gas_limit,
                op.paymaster_data.clone(),
            ),
            signature: op.signature.clone(),
        }
    }

    /// Pack a v0.8 UserOperation into the on-chain format
    fn pack_v08_user_op(op: &UserOperationV07) -> PackedUserOperationV08 {
        use crate::contracts::v08::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

        // Pack initCode: factory (20 bytes) | factoryData
        let init_code = if op.factory.is_zero() {
            Bytes::default()
        } else {
            let mut ic = Vec::with_capacity(20 + op.factory_data.len());
            ic.extend_from_slice(op.factory.as_slice());
            ic.extend_from_slice(&op.factory_data);
            Bytes::from(ic)
        };

        PackedUserOperationV08 {
            sender: op.sender,
            nonce: op.nonce,
            initCode: init_code,
            callData: op.call_data.clone(),
            accountGasLimits: pack_account_gas_limits(op.verification_gas_limit, op.call_gas_limit)
                .into(),
            preVerificationGas: op.pre_verification_gas,
            gasFees: pack_gas_fees(op.max_priority_fee_per_gas, op.max_fee_per_gas).into(),
            paymasterAndData: pack_paymaster_and_data(
                op.paymaster,
                op.paymaster_verification_gas_limit,
                op.paymaster_post_op_gas_limit,
                op.paymaster_data.clone(),
            ),
            signature: op.signature.clone(),
        }
    }
}

