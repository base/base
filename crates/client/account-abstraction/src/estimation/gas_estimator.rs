//! Gas Estimator for UserOperations
//!
//! Implements gas estimation using simulation and binary search.

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::state::StateOverride;
use alloy_rpc_types_trace::geth::{GethDebugTracingCallOptions, GethTrace};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, warn};

use crate::decoding::{
    decode_simulation_revert, ExecutionResult as DecodedExecutionResult, SimulationRevertDecoded,
    ValidationRevert,
};
use crate::entrypoint::{get_entrypoint_version, EntryPointVersion, EntryPointVersionResolver};
use crate::estimation::PreVerificationGasCalculator;
use crate::rpc::UserOperation;

/// Errors that can occur during gas estimation
#[derive(Debug, Error)]
pub enum GasEstimationError {
    #[error("Unsupported EntryPoint: {0}")]
    UnsupportedEntryPoint(Address),

    #[error("UserOperation version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: String, got: String },

    #[error("Simulation failed: {0}")]
    SimulationFailed(String),

    #[error("Validation failed: {0}")]
    ValidationFailed(ValidationRevert),

    #[error("Binary search failed to converge")]
    BinarySearchFailed,

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("PreVerificationGas calculation failed: {0}")]
    PreVerificationGasError(String),
}

/// Result of gas estimation
#[derive(Debug, Clone)]
pub struct GasEstimationResult {
    /// Gas used during pre-verification (calldata cost, etc.)
    pub pre_verification_gas: U256,
    /// Gas limit for the validation phase
    pub verification_gas_limit: U256,
    /// Gas limit for the execution phase
    pub call_gas_limit: U256,
    /// (v0.7+) Gas limit for paymaster verification
    pub paymaster_verification_gas_limit: Option<U256>,
    /// (v0.7+) Gas limit for paymaster post-op
    pub paymaster_post_op_gas_limit: Option<U256>,
}

/// Configuration for gas estimation
#[derive(Debug, Clone)]
pub struct GasEstimationConfig {
    /// Maximum number of binary search iterations
    pub max_iterations: u32,
    /// Minimum gas limit for call gas
    pub min_call_gas_limit: U256,
    /// Maximum gas limit for call gas
    pub max_call_gas_limit: U256,
    /// Minimum verification gas limit
    pub min_verification_gas_limit: U256,
    /// Maximum verification gas limit
    pub max_verification_gas_limit: U256,
    /// Multiplier for gas estimates (e.g., 1.1 = 10% buffer)
    pub gas_buffer_percent: u32,
}

impl Default for GasEstimationConfig {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            min_call_gas_limit: U256::from(21000),
            max_call_gas_limit: U256::from(20_000_000),  // 20M call gas limit
            min_verification_gas_limit: U256::from(50000),
            max_verification_gas_limit: U256::from(5_000_000),  // 5M verification gas limit
            gas_buffer_percent: 10, // 10% buffer
        }
    }
}

/// Result of a simulation call
#[derive(Debug, Clone)]
pub enum SimulationCallResult {
    /// Call succeeded with return data
    Success(Bytes),
    /// Call reverted with revert data
    Revert(Bytes),
}

/// Trait for executing eth_call simulations
/// This allows the gas estimator to be used with different providers
#[async_trait::async_trait]
pub trait SimulationProvider: Send + Sync {
    /// Execute an eth_call with optional state overrides
    /// Returns Success(data) if the call returns normally,
    /// or Revert(data) if the call reverts with revert data
    async fn eth_call(
        &self,
        to: Address,
        data: Bytes,
        block_id: BlockId,
        state_override: Option<StateOverride>,
    ) -> Result<SimulationCallResult, String>;

    /// Get the code at an address
    ///
    /// Used for EIP-7702 delegation detection - checking if code starts with 0xef0100
    async fn eth_get_code(
        &self,
        address: Address,
        block_id: BlockId,
    ) -> Result<Bytes, String>;

    /// Execute a debug_traceCall for detailed execution trace
    /// Uses reth's DebugApi internally for on-node tracing
    ///
    /// # Arguments
    /// * `to` - Target address
    /// * `data` - Call data
    /// * `block_id` - Block to execute against
    /// * `state_override` - Optional state overrides
    /// * `opts` - Geth debug tracing options (tracer type, config, etc.)
    ///
    /// # Returns
    /// * `Ok(Some(trace))` - Trace result (GethTrace from alloy)
    /// * `Ok(None)` - Debug tracing not supported by this provider
    /// * `Err(...)` - Tracing failed
    async fn debug_trace_call(
        &self,
        to: Address,
        data: Bytes,
        block_id: BlockId,
        state_override: Option<StateOverride>,
        opts: GethDebugTracingCallOptions,
    ) -> Result<Option<GethTrace>, String> {
        // Default implementation returns None (not supported)
        let _ = (to, data, block_id, state_override, opts);
        Ok(None)
    }
}

/// Gas estimator for UserOperations
pub struct GasEstimator<P, C> {
    /// Provider for simulation calls
    provider: Arc<P>,
    /// PreVerificationGas calculator
    pvg_calculator: Arc<C>,
    /// Configuration
    config: GasEstimationConfig,
}

impl<P, C> GasEstimator<P, C>
where
    P: SimulationProvider,
    C: PreVerificationGasCalculator,
{
    /// Create a new gas estimator
    pub fn new(provider: Arc<P>, pvg_calculator: Arc<C>, config: GasEstimationConfig) -> Self {
        Self {
            provider,
            pvg_calculator,
            config,
        }
    }

    /// Estimate gas for a UserOperation
    pub async fn estimate_gas(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
        state_override: Option<StateOverride>,
    ) -> Result<GasEstimationResult, GasEstimationError> {
        let version = get_entrypoint_version(entry_point)
            .ok_or(GasEstimationError::UnsupportedEntryPoint(entry_point))?;

        debug!(
            target: "aa-estimation",
            entry_point = %entry_point,
            version = ?version,
            "Starting gas estimation"
        );

        // 1. Calculate preVerificationGas
        let pre_verification_gas = self
            .pvg_calculator
            .calculate(user_op, entry_point)
            .await
            .map_err(|e| GasEstimationError::PreVerificationGasError(e))?;

        // 2. Estimate verification gas using simulation
        let verification_gas_limit = self
            .estimate_verification_gas(user_op, entry_point, version, state_override.clone())
            .await?;

        // 3. Estimate call gas using binary search
        let call_gas_limit = self
            .estimate_call_gas_binary_search(user_op, entry_point, version, state_override.clone())
            .await?;

        // 4. For v0.7+, estimate paymaster gas limits
        let (paymaster_verification_gas_limit, paymaster_post_op_gas_limit) = match user_op {
            UserOperation::V07(op) if !op.paymaster.is_zero() => {
                let pm_verification = self
                    .estimate_paymaster_verification_gas(
                        user_op,
                        entry_point,
                        version,
                        state_override.clone(),
                    )
                    .await?;
                let pm_post_op = self
                    .estimate_paymaster_post_op_gas(
                        user_op,
                        entry_point,
                        version,
                        state_override,
                    )
                    .await?;
                (Some(pm_verification), Some(pm_post_op))
            }
            _ => (None, None),
        };

        // Apply buffer to gas estimates
        let buffer_multiplier = U256::from(100 + self.config.gas_buffer_percent);
        let divisor = U256::from(100);

        let result = GasEstimationResult {
            pre_verification_gas,
            verification_gas_limit: (verification_gas_limit * buffer_multiplier) / divisor,
            call_gas_limit: (call_gas_limit * buffer_multiplier) / divisor,
            paymaster_verification_gas_limit: paymaster_verification_gas_limit
                .map(|g| (g * buffer_multiplier) / divisor),
            paymaster_post_op_gas_limit: paymaster_post_op_gas_limit
                .map(|g| (g * buffer_multiplier) / divisor),
        };

        debug!(
            target: "aa-estimation",
            pre_verification_gas = %result.pre_verification_gas,
            verification_gas_limit = %result.verification_gas_limit,
            call_gas_limit = %result.call_gas_limit,
            "Gas estimation complete"
        );

        Ok(result)
    }

    /// Debug trace a UserOperation simulation
    ///
    /// This method performs a debug_traceCall on the simulateHandleOp call,
    /// which is useful for understanding why a simulation failed.
    ///
    /// Uses reth's DebugApi internally for on-node tracing (no RPC calls).
    ///
    /// # Arguments
    /// * `user_op` - The UserOperation to trace
    /// * `entry_point` - Address of the EntryPoint contract
    /// * `state_override` - Optional state overrides
    /// * `opts` - Geth debug tracing options (tracer config, etc.)
    ///
    /// # Returns
    /// * `Ok(Some(trace))` - GethTrace result from alloy
    /// * `Ok(None)` - Debug tracing not supported by provider
    /// * `Err(...)` - Tracing failed
    pub async fn debug_trace_simulation(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
        state_override: Option<StateOverride>,
        opts: GethDebugTracingCallOptions,
    ) -> Result<Option<GethTrace>, GasEstimationError> {
        let version = get_entrypoint_version(entry_point)
            .ok_or(GasEstimationError::UnsupportedEntryPoint(entry_point))?;

        debug!(
            target: "aa-estimation",
            entry_point = %entry_point,
            version = ?version,
            "Starting debug trace for simulation"
        );

        // Create a modified user op with high gas limits for simulation
        let sim_user_op = self.create_simulation_user_op(user_op, version);

        // Build simulation calldata
        let sim_data = EntryPointVersionResolver::build_simulate_handle_op_calldata(
            version,
            &sim_user_op,
            Address::ZERO,
            Bytes::default(),
        )
        .map_err(|e| GasEstimationError::SimulationFailed(e))?;

        // Merge state overrides
        let merged_override = self.merge_overrides(state_override, sim_data.state_override);

        // Execute debug trace using the provider's DebugApi
        let trace_result = self
            .provider
            .debug_trace_call(
                sim_data.to,
                sim_data.data,
                BlockId::pending(),
                merged_override,
                opts,
            )
            .await
            .map_err(|e| GasEstimationError::ProviderError(e))?;

        if trace_result.is_some() {
            debug!(
                target: "aa-estimation",
                "Debug trace completed successfully"
            );
        } else {
            debug!(
                target: "aa-estimation",
                "Debug tracing not supported by provider"
            );
        }

        Ok(trace_result)
    }

    /// Estimate verification gas using simulation
    async fn estimate_verification_gas(
        &self,
        user_op: &UserOperation,
        _entry_point: Address,
        version: EntryPointVersion,
        state_override: Option<StateOverride>,
    ) -> Result<U256, GasEstimationError> {
        // Create a modified user op with high gas limits for simulation
        let sim_user_op = self.create_simulation_user_op(user_op, version);

        // Build simulation calldata
        let sim_data = EntryPointVersionResolver::build_simulate_handle_op_calldata(
            version,
            &sim_user_op,
            Address::ZERO,
            Bytes::default(),
        )
        .map_err(|e| GasEstimationError::SimulationFailed(e))?;

        // Merge state overrides
        let merged_override = self.merge_overrides(state_override, sim_data.state_override);

        // Execute simulation
        let sim_result = self
            .provider
            .eth_call(
                sim_data.to,
                sim_data.data,
                BlockId::pending(),
                merged_override,
            )
            .await
            .map_err(|e| GasEstimationError::ProviderError(e))?;

        // Parse simulation result to extract verification gas
        let exec_result = self.decode_simulation_result(version, sim_result)?;
        Ok(exec_result.pre_op_gas)
    }

    /// Estimate call gas using binary search
    async fn estimate_call_gas_binary_search(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
        version: EntryPointVersion,
        state_override: Option<StateOverride>,
    ) -> Result<U256, GasEstimationError> {
        let mut low = self.config.min_call_gas_limit;
        let mut high = self.config.max_call_gas_limit;
        let mut iterations = 0;

        debug!(
            target: "aa-estimation",
            low = %low,
            high = %high,
            "Starting binary search for call gas"
        );

        while low < high && iterations < self.config.max_iterations {
            let mid = (low + high) / U256::from(2);

            let success = self
                .simulate_with_call_gas(user_op, entry_point, version, mid, state_override.clone())
                .await?;

            if success {
                high = mid;
            } else {
                low = mid + U256::from(1);
            }

            iterations += 1;
        }

        if iterations >= self.config.max_iterations {
            warn!(
                target: "aa-estimation",
                iterations = iterations,
                low = %low,
                high = %high,
                "Binary search reached max iterations"
            );
        }

        debug!(
            target: "aa-estimation",
            result = %low,
            iterations = iterations,
            "Binary search complete"
        );

        Ok(low)
    }

    /// Simulate with a specific call gas limit
    /// Returns true if simulation completed with enough gas
    /// Returns false if simulation ran out of gas (need more)
    async fn simulate_with_call_gas(
        &self,
        user_op: &UserOperation,
        _entry_point: Address,
        version: EntryPointVersion,
        call_gas_limit: U256,
        state_override: Option<StateOverride>,
    ) -> Result<bool, GasEstimationError> {
        // Create modified user op with specific call gas limit
        let sim_user_op = self.modify_call_gas_limit(user_op, call_gas_limit);

        // Build simulation calldata
        let sim_data = EntryPointVersionResolver::build_simulate_handle_op_calldata(
            version,
            &sim_user_op,
            Address::ZERO,
            Bytes::default(),
        )
        .map_err(|e| GasEstimationError::SimulationFailed(e))?;

        // Merge state overrides
        let merged_override = self.merge_overrides(state_override, sim_data.state_override);

        // Execute simulation
        let sim_result = self
            .provider
            .eth_call(
                sim_data.to,
                sim_data.data,
                BlockId::pending(),
                merged_override,
            )
            .await
            .map_err(|e| GasEstimationError::ProviderError(e))?;

        // Decode and check if we had enough gas
        // Binary search logic:
        // - ExecutionResult returned = we had enough gas = try lower (return true)
        // - Any revert/error = not enough gas = try higher (return false)
        // 
        // Note: We should have already validated with max gas before binary search,
        // so any revert here means insufficient gas, not a real validation failure.
        match self.decode_simulation_result(version, sim_result) {
            Ok(exec_result) => {
                // ExecutionResult returned - we had enough gas!
                // target_success may be false if inner call reverts (business logic),
                // but that doesn't mean we need more gas
                debug!(
                    target: "aa-estimation",
                    call_gas_limit = %call_gas_limit,
                    target_success = exec_result.target_success,
                    "Simulation completed - gas was sufficient"
                );
                Ok(true)
            }
            Err(GasEstimationError::ValidationFailed(revert)) => {
                // Any validation failure during binary search = not enough gas
                // This includes:
                // - CallPhaseReverted (call phase ran out of gas)
                // - AA40/AA51/AA95 (various gas-related errors)
                // - Any other revert (likely gas-related since validation passed with max gas)
                debug!(
                    target: "aa-estimation",
                    call_gas_limit = %call_gas_limit,
                    error = %revert.message(),
                    "Simulation failed - need more gas"
                );
                Ok(false)
            }
            Err(GasEstimationError::SimulationFailed(msg)) => {
                // Unknown revert data = likely not enough gas
                debug!(
                    target: "aa-estimation",
                    call_gas_limit = %call_gas_limit,
                    error = %msg,
                    "Simulation failed with unknown error - need more gas"
                );
                Ok(false)
            }
            Err(e) => {
                // Provider error or other unexpected error - propagate
                Err(e)
            }
        }
    }

    /// Estimate paymaster verification gas
    async fn estimate_paymaster_verification_gas(
        &self,
        _user_op: &UserOperation,
        _entry_point: Address,
        _version: EntryPointVersion,
        _state_override: Option<StateOverride>,
    ) -> Result<U256, GasEstimationError> {
        // Default estimate for paymaster verification
        // In a full implementation, this would simulate the paymaster validation
        Ok(U256::from(100000))
    }

    /// Estimate paymaster post-op gas
    async fn estimate_paymaster_post_op_gas(
        &self,
        _user_op: &UserOperation,
        _entry_point: Address,
        _version: EntryPointVersion,
        _state_override: Option<StateOverride>,
    ) -> Result<U256, GasEstimationError> {
        // Default estimate for paymaster post-op
        // In a full implementation, this would simulate the paymaster postOp
        Ok(U256::from(50000))
    }

    /// Create a UserOperation with high gas limits for simulation
    fn create_simulation_user_op(&self, user_op: &UserOperation, _version: EntryPointVersion) -> UserOperation {
        match user_op {
            UserOperation::V06(op) => {
                let mut sim_op = op.clone();
                sim_op.call_gas_limit = self.config.max_call_gas_limit;
                sim_op.verification_gas_limit = self.config.max_verification_gas_limit;
                UserOperation::V06(sim_op)
            }
            UserOperation::V07(op) => {
                let mut sim_op = op.clone();
                sim_op.call_gas_limit = self.config.max_call_gas_limit;
                sim_op.verification_gas_limit = self.config.max_verification_gas_limit;
                sim_op.paymaster_verification_gas_limit = self.config.max_verification_gas_limit;
                sim_op.paymaster_post_op_gas_limit = U256::from(1_000_000);
                UserOperation::V07(sim_op)
            }
        }
    }

    /// Modify a UserOperation with a specific call gas limit
    fn modify_call_gas_limit(&self, user_op: &UserOperation, call_gas_limit: U256) -> UserOperation {
        match user_op {
            UserOperation::V06(op) => {
                let mut modified = op.clone();
                modified.call_gas_limit = call_gas_limit;
                modified.verification_gas_limit = self.config.max_verification_gas_limit;
                UserOperation::V06(modified)
            }
            UserOperation::V07(op) => {
                let mut modified = op.clone();
                modified.call_gas_limit = call_gas_limit;
                modified.verification_gas_limit = self.config.max_verification_gas_limit;
                modified.paymaster_verification_gas_limit = self.config.max_verification_gas_limit;
                modified.paymaster_post_op_gas_limit = U256::from(1_000_000);
                UserOperation::V07(modified)
            }
        }
    }

    /// Merge user-provided state overrides with simulation overrides
    fn merge_overrides(
        &self,
        user_override: Option<StateOverride>,
        sim_override: Option<StateOverride>,
    ) -> Option<StateOverride> {
        match (user_override, sim_override) {
            (None, None) => None,
            (Some(o), None) | (None, Some(o)) => Some(o),
            (Some(mut user), Some(sim)) => {
                for (address, override_data) in sim {
                    user.insert(address, override_data);
                }
                Some(user)
            }
        }
    }

    /// Decode simulation result (success or revert) into ExecutionResult
    ///
    /// Handles both:
    /// - v0.6: simulateHandleOp always reverts with ExecutionResult on success
    /// - v0.7+: simulateHandleOp returns ExecutionResult on success, reverts on failure
    fn decode_simulation_result(
        &self,
        version: EntryPointVersion,
        sim_result: SimulationCallResult,
    ) -> Result<DecodedExecutionResult, GasEstimationError> {
        match sim_result {
            SimulationCallResult::Success(data) => {
                debug!(
                    target: "aa-estimation",
                    version = ?version,
                    data_len = data.len(),
                    "Simulation returned success, decoding return data"
                );
                // v0.7+ success path: decode the return data as ExecutionResult
                // For v0.6 with our modified simulation contract, this also works
                self.decode_execution_result_from_return(&data, version)
            }
            SimulationCallResult::Revert(revert_data) => {
                debug!(
                    target: "aa-estimation",
                    version = ?version,
                    revert_len = revert_data.len(),
                    revert_hex = %format!("0x{}", hex::encode(revert_data.as_ref())),
                    "Simulation reverted, decoding revert data"
                );
                // Decode the revert data using our decoding module
                let decoded = decode_simulation_revert(version, &revert_data);

                match decoded {
                    SimulationRevertDecoded::ExecutionResult(exec) => {
                        debug!(
                            target: "aa-estimation",
                            pre_op_gas = %exec.pre_op_gas,
                            paid = %exec.paid,
                            "Decoded ExecutionResult from revert (v0.6 success case)"
                        );
                        // v0.6 success case: ExecutionResult is in revert data
                        Ok(exec)
                    }
                    SimulationRevertDecoded::ValidationResult(_) => {
                        // This shouldn't happen - gas estimation uses simulateHandleOp which
                        // returns ExecutionResult, not ValidationResult (from simulateValidation)
                        Err(GasEstimationError::SimulationFailed(
                            "Unexpected ValidationResult from simulateHandleOp".to_string()
                        ))
                    }
                    SimulationRevertDecoded::ValidationRevert(revert) => {
                        debug!(
                            target: "aa-estimation",
                            revert_msg = %revert.message(),
                            "Decoded ValidationRevert - validation failed"
                        );
                        // Validation failed
                        Err(GasEstimationError::ValidationFailed(revert))
                    }
                    SimulationRevertDecoded::Unknown(data) => {
                        warn!(
                            target: "aa-estimation",
                            data_len = data.len(),
                            data_hex = %format!("0x{}", hex::encode(&data)),
                            "Unknown revert data - could not decode"
                        );
                        Err(GasEstimationError::SimulationFailed(format!(
                            "Unknown revert data: 0x{}",
                            hex::encode(&data)
                        )))
                    }
                }
            }
        }
    }

    /// Decode ExecutionResult from successful call return data
    fn decode_execution_result_from_return(
        &self,
        data: &Bytes,
        version: EntryPointVersion,
    ) -> Result<DecodedExecutionResult, GasEstimationError> {
        use crate::contracts::{ExecutionResultV06Sim, ExecutionResultV07, ExecutionResultV08};
        use alloy_sol_types::SolType;

        debug!(
            target: "aa-estimation",
            version = ?version,
            data_len = data.len(),
            data_hex = %format!("0x{}", hex::encode(data.as_ref())),
            "Decoding execution result from return data"
        );

        if data.len() < 32 {
            warn!(
                target: "aa-estimation",
                version = ?version,
                data_len = data.len(),
                data_hex = %format!("0x{}", hex::encode(data.as_ref())),
                "Simulation result too short - expected at least 32 bytes"
            );
            return Err(GasEstimationError::SimulationFailed(
                format!("Invalid simulation result: too short ({} bytes, data: 0x{})", 
                    data.len(), 
                    hex::encode(data.as_ref())
                ),
            ));
        }

        match version {
            EntryPointVersion::V06 => {
                let exec = ExecutionResultV06Sim::abi_decode(data)
                    .map_err(|e| GasEstimationError::SimulationFailed(format!(
                        "Failed to decode v0.6 ExecutionResult: {}",
                        e
                    )))?;
                Ok(DecodedExecutionResult {
                    pre_op_gas: exec.preOpGas,
                    paid: exec.paid,
                    target_success: exec.targetSuccess,
                    target_result: exec.targetResult,
                    account_validation_data: exec.accountValidationData,
                    paymaster_validation_data: exec.paymasterValidationData,
                })
            }
            EntryPointVersion::V07 => {
                let exec = ExecutionResultV07::abi_decode(data)
                    .map_err(|e| GasEstimationError::SimulationFailed(format!(
                        "Failed to decode v0.7 ExecutionResult: {}",
                        e
                    )))?;
                Ok(DecodedExecutionResult {
                    pre_op_gas: exec.preOpGas,
                    paid: exec.paid,
                    target_success: exec.targetSuccess,
                    target_result: exec.targetResult,
                    account_validation_data: exec.accountValidationData,
                    paymaster_validation_data: exec.paymasterValidationData,
                })
            }
            // v0.8 and v0.9 are ABI-compatible
            EntryPointVersion::V08 | EntryPointVersion::V09 => {
                let exec = ExecutionResultV08::abi_decode(data)
                    .map_err(|e| GasEstimationError::SimulationFailed(format!(
                        "Failed to decode v0.8/v0.9 ExecutionResult: {}",
                        e
                    )))?;
                Ok(DecodedExecutionResult {
                    pre_op_gas: exec.preOpGas,
                    paid: exec.paid,
                    target_success: exec.targetSuccess,
                    target_result: exec.targetResult,
                    account_validation_data: exec.accountValidationData,
                    paymaster_validation_data: exec.paymasterValidationData,
                })
            }
        }
    }
}

