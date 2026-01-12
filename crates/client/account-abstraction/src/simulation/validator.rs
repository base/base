//! UserOperation Validator
//!
//! This module provides the main orchestrator for validating UserOperations.
//! It combines simulation, ERC-7562 tracing, rule checking, and entity info collection.

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_eth::state::StateOverride;

/// EIP-7702 delegation marker prefix (0xef0100)
/// Delegated accounts have code: 0xef0100 + 20-byte delegate address
pub const EIP7702_DELEGATION_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

/// Check if code represents an EIP-7702 delegation
///
/// EIP-7702 delegated accounts have code in the format:
/// 0xef0100 + 20-byte delegate address (total 23 bytes)
pub fn is_eip7702_delegation(code: &Bytes) -> bool {
    code.len() == 23 && code.starts_with(&EIP7702_DELEGATION_PREFIX)
}

/// Extract the delegate address from EIP-7702 delegation code
///
/// Returns the 20-byte delegate address if the code is a valid delegation,
/// or None if the code is not a delegation.
pub fn get_eip7702_delegate(code: &Bytes) -> Option<Address> {
    if is_eip7702_delegation(code) {
        // The delegate address is bytes 3..23 (after the 0xef0100 prefix)
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&code[3..23]);
        Some(Address::from(addr_bytes))
    } else {
        None
    }
}
use alloy_rpc_types_trace::geth::{
    erc7562::{Erc7562Config, Erc7562Frame},
    GethDebugTracerType, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethTrace,
};
use alloy_sol_types::SolCall;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::contracts::{
    create_code_override,
    IEntryPointV06, UserOperationV06Packed,
    ENTRYPOINT_V06_ADDRESS, ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE,
    IEntryPointSimulationsV07, PackedUserOperationV07,
    ENTRYPOINT_V07_ADDRESS, ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE,
    IEntryPointSimulationsV08, PackedUserOperationV08,
    ENTRYPOINT_V08_ADDRESS, ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE,
};
use crate::entrypoint::{get_entrypoint_version, EntryPointVersion};
use crate::estimation::SimulationProvider;
use crate::rpc::{UserOperation, UserOperationV06, UserOperationV07};

use super::entity::EntityInfoProvider;
use super::precheck::{Prechecker, PrecheckConfig};
use super::rules::Erc7562RuleChecker;
use super::types::{EntityCodeHashes, ReturnInfo, StakingConfig, ValidationError, ValidationOutput};

/// Configuration for the UserOperation validator
#[derive(Debug, Clone)]
pub struct ValidationConfig {
    /// Whether to perform ERC-7562 tracing
    pub enable_tracing: bool,

    /// Whether to run prechecks before simulation
    /// These are fast checks that fail early on obvious issues (DoS protection)
    pub enable_prechecks: bool,

    /// Staking requirements configuration
    pub staking_config: StakingConfig,

    /// Precheck configuration (gas limits, deposit checks, etc.)
    pub precheck_config: PrecheckConfig,

    /// ERC-7562 tracer configuration
    pub tracer_config: Erc7562Config,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enable_tracing: true,
            enable_prechecks: true,
            staking_config: StakingConfig::default(),
            precheck_config: PrecheckConfig::default(),
            tracer_config: Erc7562Config {
                stack_top_items_size: None,
                ignored_opcodes: vec![],
                with_log: Some(false),
            },
        }
    }
}

/// UserOperation validator
///
/// Performs validation simulation and ERC-7562 rule checking.
pub struct UserOperationValidator<P> {
    provider: Arc<P>,
    entity_provider: EntityInfoProvider<P>,
    prechecker: Prechecker<P>,
    config: ValidationConfig,
}

impl<P> UserOperationValidator<P>
where
    P: SimulationProvider,
{
    /// Create a new validator
    pub fn new(provider: Arc<P>) -> Self {
        let config = ValidationConfig::default();
        let entity_provider = EntityInfoProvider::new(provider.clone());
        let prechecker = Prechecker::new(provider.clone(), config.precheck_config.clone());
        Self {
            provider,
            entity_provider,
            prechecker,
            config,
        }
    }

    /// Create a validator with custom configuration
    pub fn with_config(provider: Arc<P>, config: ValidationConfig) -> Self {
        let entity_provider =
            EntityInfoProvider::with_staking_config(provider.clone(), config.staking_config.clone());
        let prechecker = Prechecker::new(provider.clone(), config.precheck_config.clone());
        Self {
            provider,
            entity_provider,
            prechecker,
            config,
        }
    }

    /// Validate a UserOperation
    ///
    /// This performs:
    /// 0. Prechecks (if enabled) - fast fail on obvious issues
    /// 1. Entity extraction from the UserOp
    /// 2. Check if sender account exists (for storage rules)
    /// 3. Validation simulation with ERC-7562 tracing
    /// 4. Entity stake/deposit info collection
    /// 5. ERC-7562 rule checking
    /// 6. EIP-7702 delegation rule checking
    ///
    /// # Arguments
    /// * `user_op` - The UserOperation to validate
    /// * `entry_point` - The EntryPoint address
    ///
    /// # Returns
    /// ValidationOutput with results, entity info, and any rule violations
    pub async fn validate(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
    ) -> Result<ValidationOutput, ValidationError> {
        // Get EntryPoint version
        let version = get_entrypoint_version(entry_point)
            .ok_or(ValidationError::UnsupportedEntryPoint(entry_point))?;

        info!(
            target: "aa-validation",
            entry_point = %entry_point,
            version = ?version,
            "Starting UserOperation validation"
        );

        // 0. Run prechecks (fast fail on obvious issues)
        let precheck_result = if self.config.enable_prechecks {
            match self.prechecker.check(user_op, entry_point).await {
                Ok(result) => {
                    debug!(
                        target: "aa-validation",
                        sender_is_7702 = result.sender_is_7702,
                        delegate = ?result.delegate_address,
                        "Prechecks passed"
                    );
                    Some(result)
                }
                Err(violation) => {
                    warn!(
                        target: "aa-validation",
                        violation = %violation,
                        "Precheck failed"
                    );
                    return Err(ValidationError::PrecheckFailed(violation));
                }
            }
        } else {
            None
        };

        // 1. Extract entities from UserOp
        let entities = EntityInfoProvider::<P>::extract_entities(user_op, entry_point);

        debug!(
            target: "aa-validation",
            sender = %entities.sender,
            factory = ?entities.factory,
            paymaster = ?entities.paymaster,
            "Extracted entities from UserOp"
        );

        // 2. Check if the sender account already exists on-chain
        // Use precheck result if available, otherwise check from UserOp structure
        let account_exists = if let Some(ref precheck) = precheck_result {
            // If sender is 7702, it "exists" (has delegation code)
            precheck.sender_is_7702 || !Self::check_account_exists_from_user_op(user_op)
        } else {
            Self::check_account_exists_from_user_op(user_op)
        };

        debug!(
            target: "aa-validation",
            sender = %entities.sender,
            account_exists = account_exists,
            "Checked sender account existence"
        );

        // 3. Run validation simulation with tracing
        let (return_info, trace, aggregator) = self
            .simulate_validation_with_trace(user_op, entry_point, version)
            .await?;

        // Update entities with aggregator if found
        let mut entities = entities;
        entities.aggregator = aggregator;

        // 4. Get entity stake/deposit info
        let entities_info = self
            .entity_provider
            .get_entities_info(&entities, entry_point)
            .await?;

        // 5. Extract code hashes from trace (for COD-010 and mempool)
        let code_hashes = trace.as_ref().map(|t| self.extract_code_hashes(t, &entities));

        // 6. Check ERC-7562 rules against trace (if tracing enabled)
        let mut violations = if let Some(ref trace_frame) = trace {
            let rule_checker = Erc7562RuleChecker::with_account_exists(account_exists);
            rule_checker.check_all_rules(trace_frame, &entities, &entities_info)
        } else {
            if self.config.enable_tracing {
                warn!(
                    target: "aa-validation",
                    "ERC-7562 tracing enabled but no trace data available - rule checking skipped"
                );
            }
            Vec::new()
        };

        // 7. Check EIP-7702 delegation rules (AUTH-020, AUTH-030)
        let auth_violations = self
            .check_eip7702_delegations(&entities, trace.as_ref())
            .await;
        violations.extend(auth_violations);

        if !violations.is_empty() {
            warn!(
                target: "aa-validation",
                violation_count = violations.len(),
                "ERC-7562 rule violations detected"
            );
            for violation in &violations {
                debug!(
                    target: "aa-validation",
                    rule = %violation.rule,
                    entity = %violation.entity,
                    description = %violation.description,
                    "Rule violation"
                );
            }
        }

        // Build output
        let valid = violations.is_empty() && !return_info.sig_failed;

        Ok(ValidationOutput {
            valid,
            return_info,
            sender_info: entities_info.sender,
            factory_info: entities_info.factory,
            paymaster_info: entities_info.paymaster,
            aggregator_info: entities_info.aggregator,
            code_hashes,
            violations,
            trace: trace.map(Box::new),
        })
    }

    /// Extract code hashes for all entities from the trace
    ///
    /// NOTE: The ERC-7562 tracer's `contract_size` map does not include code hashes
    /// in the current alloy types. This returns default hashes for now.
    /// For full COD-010 support, code hashes should be fetched via eth_getCode + keccak256.
    fn extract_code_hashes(
        &self,
        trace: &Erc7562Frame,
        entities: &super::types::Entities,
    ) -> EntityCodeHashes {
        use alloy_primitives::B256;

        // Check if an address was accessed during tracing (exists in contract_size map)
        fn was_accessed(trace: &Erc7562Frame, address: Address) -> bool {
            if trace.contract_size.contains_key(&address) {
                return true;
            }
            for child in &trace.calls {
                if was_accessed(child, address) {
                    return true;
                }
            }
            false
        }

        // For now, return ZERO hashes for accessed entities
        // TODO: Fetch actual code hashes via eth_getCode for COD-010 support
        EntityCodeHashes {
            sender: if was_accessed(trace, entities.sender) { B256::ZERO } else { B256::ZERO },
            factory: entities.factory.filter(|&f| was_accessed(trace, f)).map(|_| B256::ZERO),
            paymaster: entities.paymaster.filter(|&p| was_accessed(trace, p)).map(|_| B256::ZERO),
            aggregator: entities.aggregator.filter(|&a| was_accessed(trace, a)).map(|_| B256::ZERO),
        }
    }

    /// Check EIP-7702 delegation rules (AUTH-020, AUTH-030)
    ///
    /// AUTH-020: Account with EIP-7702 delegation can only be used as Sender
    /// AUTH-030: Account with EIP-7702 delegation can only be accessed if it's the Sender
    ///
    /// This method checks:
    /// 1. If factory, paymaster, or aggregator have EIP-7702 delegation code (AUTH-020)
    /// 2. If any accessed addresses from the trace have delegation and are not the sender (AUTH-030)
    async fn check_eip7702_delegations(
        &self,
        entities: &super::types::Entities,
        trace: Option<&Erc7562Frame>,
    ) -> Vec<super::types::RuleViolation> {
        use super::types::RuleViolation;
        let mut violations = Vec::new();

        // AUTH-020: Check if entities (factory, paymaster, aggregator) have delegation
        // They CANNOT be delegated accounts - only the sender can be delegated
        let entity_addresses = [
            ("factory", entities.factory),
            ("paymaster", entities.paymaster),
            ("aggregator", entities.aggregator),
        ];

        for (entity_type, maybe_addr) in entity_addresses {
            if let Some(addr) = maybe_addr {
                match self.provider.eth_get_code(addr, BlockId::pending()).await {
                    Ok(code) => {
                        if is_eip7702_delegation(&code) {
                            warn!(
                                target: "aa-validation",
                                address = %addr,
                                entity_type = entity_type,
                                "AUTH-020: Entity with EIP-7702 delegation used as {}",
                                entity_type
                            );
                            violations.push(RuleViolation::delegation_not_sender(addr, addr));
                        }
                    }
                    Err(e) => {
                        debug!(
                            target: "aa-validation",
                            address = %addr,
                            error = %e,
                            "Failed to get code for entity, skipping EIP-7702 check"
                        );
                    }
                }
            }
        }

        // AUTH-030: Check if any accessed addresses have delegation and are NOT the sender
        if let Some(trace_frame) = trace {
            let accessed_addresses = self.collect_accessed_addresses(trace_frame);

            for addr in accessed_addresses {
                // Skip the sender - they're allowed to have delegation
                if addr == entities.sender {
                    continue;
                }
                // Skip entities we already checked above
                if entities.factory == Some(addr)
                    || entities.paymaster == Some(addr)
                    || entities.aggregator == Some(addr)
                {
                    continue;
                }

                // Check if this accessed address has EIP-7702 delegation
                match self.provider.eth_get_code(addr, BlockId::pending()).await {
                    Ok(code) => {
                        if is_eip7702_delegation(&code) {
                            warn!(
                                target: "aa-validation",
                                address = %addr,
                                sender = %entities.sender,
                                "AUTH-030: Accessed account with EIP-7702 delegation is not the sender"
                            );
                            violations.push(RuleViolation::delegation_access_not_sender(
                                entities.sender, // The entity doing the access
                                addr,             // The delegated address that was accessed
                            ));
                        }
                    }
                    Err(e) => {
                        debug!(
                            target: "aa-validation",
                            address = %addr,
                            error = %e,
                            "Failed to get code for accessed address, skipping EIP-7702 check"
                        );
                    }
                }
            }
        }

        violations
    }

    /// Collect all addresses accessed during validation from the trace
    ///
    /// This includes addresses from:
    /// - ext_code_access_info (EXTCODESIZE/EXTCODECOPY/EXTCODEHASH)
    /// - contract_size (EXTCODESIZE/EXTCODECOPY)
    /// - calls (child frame destinations)
    fn collect_accessed_addresses(&self, trace: &Erc7562Frame) -> Vec<Address> {
        let mut addresses = Vec::new();

        fn collect_recursive(frame: &Erc7562Frame, addrs: &mut Vec<Address>) {
            // Addresses from ext_code_access_info (stored as strings)
            for addr_str in &frame.ext_code_access_info {
                if let Ok(addr) = addr_str.parse::<Address>() {
                    addrs.push(addr);
                }
            }

            // Addresses from contract_size
            addrs.extend(frame.contract_size.keys().copied());

            // The 'to' address of this frame
            if let Some(to) = frame.to {
                addrs.push(to);
            }

            // Recurse into child calls
            for child in &frame.calls {
                collect_recursive(child, addrs);
            }
        }

        collect_recursive(trace, &mut addresses);

        // Deduplicate
        addresses.sort();
        addresses.dedup();
        addresses
    }

    /// Determine if sender account already exists based on UserOp structure
    ///
    /// An account is considered to "exist" if the UserOp doesn't include
    /// deployment data (factory/initCode). If factory is present, the account
    /// is being created by this UserOp.
    ///
    /// This is used for STO-021/STO-022 storage rules where different rules
    /// apply depending on whether the account is being created or already exists.
    fn check_account_exists_from_user_op(user_op: &UserOperation) -> bool {
        match user_op {
            UserOperation::V06(op) => {
                // v0.6: initCode is empty means account exists
                op.init_code.is_empty()
            }
            UserOperation::V07(op) => {
                // v0.7: factory is zero address means account exists
                op.factory.is_zero()
            }
        }
    }

    /// Simulate validation and collect trace
    async fn simulate_validation_with_trace(
        &self,
        user_op: &UserOperation,
        entry_point: Address,
        version: EntryPointVersion,
    ) -> Result<(ReturnInfo, Option<Erc7562Frame>, Option<Address>), ValidationError> {
        // Build simulation calldata
        let (calldata, state_override) = self.build_simulate_validation_calldata(user_op, version)?;

        debug!(
            target: "aa-validation",
            to = %entry_point,
            calldata_len = calldata.len(),
            has_state_override = state_override.is_some(),
            "Built simulateValidation calldata"
        );

        // Execute with ERC-7562 tracing if enabled
        let (sim_result, trace) = if self.config.enable_tracing {
            self.execute_with_trace(entry_point, calldata.clone(), state_override.clone())
                .await?
        } else {
            let result = self
                .provider
                .eth_call(entry_point, calldata.clone(), BlockId::pending(), state_override.clone())
                .await
                .map_err(|e| ValidationError::ProviderError(e))?;
            (result, None)
        };

        // Handle the simulation result based on EntryPoint version
        let result_bytes = match sim_result {
            crate::estimation::SimulationCallResult::Success(data) => data,
            crate::estimation::SimulationCallResult::Revert(data) => {
                // For v0.6, revert is expected (simulateValidation always reverts)
                // For v0.7+, revert means validation failed - decode the error
                if version == EntryPointVersion::V06 {
                    data
                } else {
                    // Decode v0.7 revert to get the actual error message
                    use crate::decoding::{decode_v07_simulation_revert, SimulationRevertDecoded};
                    match decode_v07_simulation_revert(&data) {
                        SimulationRevertDecoded::ValidationRevert(revert) => {
                            return Err(ValidationError::ValidationReverted(revert.message()));
                        }
                        SimulationRevertDecoded::Unknown(bytes) => {
                            return Err(ValidationError::ValidationReverted(format!(
                                "Unknown validation revert: 0x{}",
                                hex::encode(&bytes)
                            )));
                        }
                        _ => {
                            return Err(ValidationError::ValidationReverted(format!(
                                "Validation reverted: 0x{}",
                                hex::encode(&data)
                            )));
                        }
                    }
                }
            }
        };

        // Decode the result based on version
        let (return_info, aggregator) = self.decode_validation_result(&result_bytes, version)?;

        Ok((return_info, trace, aggregator))
    }

    /// Execute simulation with ERC-7562 tracing
    ///
    /// Uses `erc7562Tracer` JS tracer to trace the validation
    /// and collect opcode usage, storage access, and other ERC-7562 relevant data.
    async fn execute_with_trace(
        &self,
        to: Address,
        data: Bytes,
        state_override: Option<StateOverride>,
    ) -> Result<(crate::estimation::SimulationCallResult, Option<Erc7562Frame>), ValidationError> {
        // Build ERC-7562 tracer options
        // Note: erc7562Tracer is a custom JS tracer, not a built-in tracer
        // See: https://github.com/ethereum/go-ethereum/pull/31006
        let tracer_config = serde_json::to_value(&self.config.tracer_config)
            .map_err(|e| ValidationError::TracingError(format!("Failed to serialize tracer config: {}", e)))?;

        let tracing_opts = GethDebugTracingCallOptions {
            tracing_options: GethDebugTracingOptions {
                tracer: Some(GethDebugTracerType::JsTracer("erc7562Tracer".to_string())),
                tracer_config: tracer_config.into(),
                ..Default::default()
            },
            state_overrides: state_override.clone(),
            block_overrides: None,
            tx_index: None,
        };

        debug!(
            target: "aa-validation",
            to = %to,
            "Executing debug_traceCall with ERC-7562 tracer"
        );

        // Execute debug_traceCall with ERC-7562 tracer
        let trace_result = self
            .provider
            .debug_trace_call(
                to,
                data.clone(),
                BlockId::pending(),
                state_override.clone(),
                tracing_opts,
            )
            .await;

        // Extract Erc7562Frame from GethTrace
        // The erc7562Tracer returns a JS object that we need to deserialize
        let frame = match trace_result {
            Ok(Some(GethTrace::JS(value))) => {
                // Try to deserialize the JS value to Erc7562Frame
                match serde_json::from_value::<Erc7562Frame>(value) {
                    Ok(frame) => {
                        debug!(
                            target: "aa-validation",
                            from = %frame.from,
                            to = ?frame.to,
                            used_opcodes_count = frame.used_opcodes.len(),
                            calls_count = frame.calls.len(),
                            "ERC-7562 trace captured successfully"
                        );
                        Some(frame)
                    }
                    Err(e) => {
                        warn!(
                            target: "aa-validation",
                            error = %e,
                            "Failed to deserialize ERC-7562 trace response"
                        );
                        None
                    }
                }
            }
            Ok(Some(other_trace)) => {
                warn!(
                    target: "aa-validation",
                    trace_type = ?std::mem::discriminant(&other_trace),
                    "debug_traceCall returned unexpected trace type, expected JS"
                );
                None
            }
            Ok(None) => {
                debug!(
                    target: "aa-validation",
                    "debug_traceCall returned None (tracing not supported by provider)"
                );
                None
            }
            Err(e) => {
                warn!(
                    target: "aa-validation",
                    error = %e,
                    "ERC-7562 tracing failed, falling back to eth_call without trace"
                );
                None
            }
        };

        // Also execute eth_call to get the actual result
        // (debug_traceCall doesn't always return the call result in a usable format)
        let result = self
            .provider
            .eth_call(to, data, BlockId::pending(), state_override)
            .await
            .map_err(|e| ValidationError::ProviderError(e))?;

        Ok((result, frame))
    }

    /// Build calldata for simulateValidation
    fn build_simulate_validation_calldata(
        &self,
        user_op: &UserOperation,
        version: EntryPointVersion,
    ) -> Result<(Bytes, Option<StateOverride>), ValidationError> {
        match version {
            EntryPointVersion::V06 => self.build_v06_simulate_validation(user_op),
            EntryPointVersion::V07 => self.build_v07_simulate_validation(user_op),
            // v0.8 and v0.9 are ABI-compatible
            EntryPointVersion::V08 | EntryPointVersion::V09 => {
                self.build_v08_simulate_validation(user_op)
            }
        }
    }

    /// Build v0.6 simulateValidation calldata
    fn build_v06_simulate_validation(
        &self,
        user_op: &UserOperation,
    ) -> Result<(Bytes, Option<StateOverride>), ValidationError> {
        let op = match user_op {
            UserOperation::V06(op) => op,
            UserOperation::V07(_) => {
                return Err(ValidationError::SimulationFailed(
                    "Cannot use v0.7 UserOperation with v0.6 EntryPoint".to_string(),
                ))
            }
        };

        let packed = self.pack_user_op_v06(op);
        let call = IEntryPointV06::simulateValidationCall { op: packed };
        let calldata = Bytes::from(call.abi_encode());

        // Build state override with simulation bytecode
        let state_override = if !ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V06_SIMULATIONS_DEPLOYED_BYTECODE).map_err(|e| {
                    ValidationError::SimulationFailed(format!(
                        "Failed to decode v0.6 simulation bytecode: {}",
                        e
                    ))
                })?);
            Some(create_code_override(ENTRYPOINT_V06_ADDRESS, bytecode))
        } else {
            None
        };

        Ok((calldata, state_override))
    }

    /// Build v0.7 simulateValidation calldata
    fn build_v07_simulate_validation(
        &self,
        user_op: &UserOperation,
    ) -> Result<(Bytes, Option<StateOverride>), ValidationError> {
        let op = match user_op {
            UserOperation::V07(op) => op,
            UserOperation::V06(_) => {
                return Err(ValidationError::SimulationFailed(
                    "Cannot use v0.6 UserOperation with v0.7 EntryPoint".to_string(),
                ))
            }
        };

        let packed = self.pack_user_op_v07(op);
        let call = IEntryPointSimulationsV07::simulateValidationCall { userOp: packed };
        let calldata = Bytes::from(call.abi_encode());

        // Build state override with simulation bytecode
        let state_override = if !ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V07_SIMULATIONS_DEPLOYED_BYTECODE).map_err(|e| {
                    ValidationError::SimulationFailed(format!(
                        "Failed to decode v0.7 simulation bytecode: {}",
                        e
                    ))
                })?);
            Some(create_code_override(ENTRYPOINT_V07_ADDRESS, bytecode))
        } else {
            None
        };

        Ok((calldata, state_override))
    }

    /// Build v0.8 simulateValidation calldata
    fn build_v08_simulate_validation(
        &self,
        user_op: &UserOperation,
    ) -> Result<(Bytes, Option<StateOverride>), ValidationError> {
        let op = match user_op {
            UserOperation::V07(op) => op, // v0.8 uses same format as v0.7
            UserOperation::V06(_) => {
                return Err(ValidationError::SimulationFailed(
                    "Cannot use v0.6 UserOperation with v0.8 EntryPoint".to_string(),
                ))
            }
        };

        let packed = self.pack_user_op_v08(op);
        let call = IEntryPointSimulationsV08::simulateValidationCall { userOp: packed };
        let calldata = Bytes::from(call.abi_encode());

        // Build state override with simulation bytecode
        let state_override = if !ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE.is_empty() {
            let bytecode =
                Bytes::from(hex::decode(ENTRYPOINT_V08_SIMULATIONS_DEPLOYED_BYTECODE).map_err(|e| {
                    ValidationError::SimulationFailed(format!(
                        "Failed to decode v0.8 simulation bytecode: {}",
                        e
                    ))
                })?);
            Some(create_code_override(ENTRYPOINT_V08_ADDRESS, bytecode))
        } else {
            None
        };

        Ok((calldata, state_override))
    }

    /// Decode validation result based on version
    fn decode_validation_result(
        &self,
        data: &Bytes,
        version: EntryPointVersion,
    ) -> Result<(ReturnInfo, Option<Address>), ValidationError> {
        match version {
            EntryPointVersion::V06 => self.decode_v06_validation_result(data),
            EntryPointVersion::V07 | EntryPointVersion::V08 | EntryPointVersion::V09 => {
                self.decode_v07_validation_result(data)
            }
        }
    }

    /// Decode v0.6 validation result (from revert data)
    fn decode_v06_validation_result(
        &self,
        data: &Bytes,
    ) -> Result<(ReturnInfo, Option<Address>), ValidationError> {
        use crate::decoding::{decode_v06_simulation_revert, SimulationRevertDecoded};

        // v0.6 simulateValidation reverts with ValidationResult or ValidationResultWithAggregation
        let decoded = decode_v06_simulation_revert(data);

        match decoded {
            SimulationRevertDecoded::ExecutionResult(result) => {
                // Extract aggregator from validation data (top 20 bytes)
                let aggregator_u256: U256 = result.account_validation_data >> 96;
                let aggregator = if aggregator_u256 == U256::ZERO || aggregator_u256 == U256::from(1) {
                    None
                } else {
                    // Convert U256 to Address
                    let mut addr_bytes = [0u8; 20];
                    let u256_bytes = aggregator_u256.to_be_bytes::<32>();
                    addr_bytes.copy_from_slice(&u256_bytes[12..32]);
                    Some(Address::from(addr_bytes))
                };

                Ok((
                    ReturnInfo {
                        pre_op_gas: result.pre_op_gas.to::<u64>(),
                        prefund: result.paid,
                        sig_failed: result.signature_failed(),
                        valid_after: result.valid_after(),
                        valid_until: result.valid_until(),
                        paymaster_context: Bytes::default(),
                    },
                    aggregator,
                ))
            }
            SimulationRevertDecoded::ValidationResult(result) => {
                // v0.6 ValidationResult from simulateValidation
                Ok((
                    ReturnInfo {
                        pre_op_gas: result.pre_op_gas.to::<u64>(),
                        prefund: result.prefund,
                        sig_failed: result.sig_failed,
                        valid_after: result.valid_after,
                        valid_until: result.valid_until,
                        paymaster_context: result.paymaster_context,
                    },
                    result.aggregator,
                ))
            }
            SimulationRevertDecoded::ValidationRevert(revert) => {
                Err(ValidationError::ValidationReverted(revert.message()))
            }
            SimulationRevertDecoded::Unknown(bytes) => Err(ValidationError::DecodingError(format!(
                "Unknown v0.6 validation result: 0x{}",
                hex::encode(&bytes)
            ))),
        }
    }

    /// Decode v0.7+ validation result (from return data)
    fn decode_v07_validation_result(
        &self,
        data: &Bytes,
    ) -> Result<(ReturnInfo, Option<Address>), ValidationError> {
        // Try to decode as ValidationResultV07
        let result = IEntryPointSimulationsV07::simulateValidationCall::abi_decode_returns(data)
            .map_err(|e| {
                ValidationError::DecodingError(format!("Failed to decode v0.7 validation result: {}", e))
            })?;

        // Extract fields from packed validation data
        // Format: 20 bytes aggregator | 6 bytes validUntil | 6 bytes validAfter
        let account_validation_data = result.accountValidationData;
        let paymaster_validation_data = result.paymasterValidationData;

        let (sig_failed, valid_after, valid_until, aggregator) =
            Self::unpack_validation_data(account_validation_data);

        // Also check paymaster validation data for time bounds
        let (pm_sig_failed, pm_valid_after, pm_valid_until, _) =
            Self::unpack_validation_data(paymaster_validation_data);

        // Intersect time ranges
        let final_valid_after = valid_after.max(pm_valid_after);
        let final_valid_until = if valid_until == 0 {
            pm_valid_until
        } else if pm_valid_until == 0 {
            valid_until
        } else {
            valid_until.min(pm_valid_until)
        };

        Ok((
            ReturnInfo {
                pre_op_gas: result.preOpGas.to::<u64>(),
                prefund: result.prefund,
                sig_failed: sig_failed || pm_sig_failed,
                valid_after: final_valid_after,
                valid_until: final_valid_until,
                paymaster_context: result.paymasterContext,
            },
            aggregator,
        ))
    }

    /// Unpack validation data into its components
    /// Format: 20 bytes aggregator | 6 bytes validUntil | 6 bytes validAfter
    fn unpack_validation_data(validation_data: U256) -> (bool, u64, u64, Option<Address>) {
        // validAfter is in the lowest 6 bytes (48 bits)
        let mask = U256::from(0xFFFFFFFFFFFFu64);
        let valid_after_u256: U256 = validation_data & mask;
        let valid_after: u64 = valid_after_u256.try_into().unwrap_or(0);

        // validUntil is in bytes 6-12 (48 bits shifted by 48)
        let valid_until_u256: U256 = (validation_data >> 48) & mask;
        let valid_until: u64 = valid_until_u256.try_into().unwrap_or(0);

        // Aggregator/sigFailed is in the top 20 bytes (shifted by 96)
        let aggregator_u256: U256 = validation_data >> 96;

        let sig_failed = aggregator_u256 == U256::from(1);
        let aggregator = if aggregator_u256 == U256::ZERO || aggregator_u256 == U256::from(1) {
            None
        } else {
            let mut addr_bytes = [0u8; 20];
            let u256_bytes = aggregator_u256.to_be_bytes::<32>();
            addr_bytes.copy_from_slice(&u256_bytes[12..32]);
            Some(Address::from(addr_bytes))
        };

        (sig_failed, valid_after, valid_until, aggregator)
    }

    /// Pack v0.6 UserOperation
    fn pack_user_op_v06(&self, op: &UserOperationV06) -> UserOperationV06Packed {
        UserOperationV06Packed {
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
        }
    }

    /// Pack v0.7 UserOperation
    fn pack_user_op_v07(&self, op: &UserOperationV07) -> PackedUserOperationV07 {
        use crate::contracts::v07::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

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

    /// Pack v0.8 UserOperation
    fn pack_user_op_v08(&self, op: &UserOperationV07) -> PackedUserOperationV08 {
        use crate::contracts::v08::{pack_account_gas_limits, pack_gas_fees, pack_paymaster_and_data};

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

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // EIP-7702 Helper Tests
    // ============================================================

    #[test]
    fn test_is_eip7702_delegation_valid() {
        // Valid: 0xef0100 + 20-byte address = 23 bytes total
        let delegate = Address::repeat_byte(0xab);
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(delegate.as_slice());
        let bytes = Bytes::from(code);

        assert!(is_eip7702_delegation(&bytes));
    }

    #[test]
    fn test_is_eip7702_delegation_wrong_prefix() {
        // Wrong prefix
        let delegate = Address::repeat_byte(0xab);
        let mut code = vec![0xef, 0x02, 0x00]; // wrong byte
        code.extend_from_slice(delegate.as_slice());
        let bytes = Bytes::from(code);

        assert!(!is_eip7702_delegation(&bytes));
    }

    #[test]
    fn test_is_eip7702_delegation_wrong_length() {
        // Too short (only prefix, no address)
        let bytes = Bytes::from(vec![0xef, 0x01, 0x00]);
        assert!(!is_eip7702_delegation(&bytes));

        // Too long (24 bytes)
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(&[0xab; 21]);
        let bytes = Bytes::from(code);
        assert!(!is_eip7702_delegation(&bytes));

        // Empty
        assert!(!is_eip7702_delegation(&Bytes::new()));
    }

    #[test]
    fn test_is_eip7702_delegation_regular_contract() {
        // Regular contract bytecode (not a delegation)
        let bytes = Bytes::from(vec![0x60, 0x80, 0x60, 0x40, 0x52]); // PUSH1 80 PUSH1 40 MSTORE
        assert!(!is_eip7702_delegation(&bytes));
    }

    #[test]
    fn test_get_eip7702_delegate_valid() {
        let delegate = Address::repeat_byte(0xcd);
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(delegate.as_slice());
        let bytes = Bytes::from(code);

        let result = get_eip7702_delegate(&bytes);
        assert_eq!(result, Some(delegate));
    }

    #[test]
    fn test_get_eip7702_delegate_specific_address() {
        // Test with a specific address
        let delegate = Address::new([
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa,
            0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44,
        ]);
        let mut code = vec![0xef, 0x01, 0x00];
        code.extend_from_slice(delegate.as_slice());
        let bytes = Bytes::from(code);

        let result = get_eip7702_delegate(&bytes);
        assert_eq!(result, Some(delegate));
    }

    #[test]
    fn test_get_eip7702_delegate_invalid() {
        // Not a delegation - should return None
        let bytes = Bytes::from(vec![0x60, 0x80, 0x60, 0x40]);
        assert_eq!(get_eip7702_delegate(&bytes), None);

        // Empty
        assert_eq!(get_eip7702_delegate(&Bytes::new()), None);

        // Wrong prefix
        let mut code = vec![0xef, 0x00, 0x00];
        code.extend_from_slice(&[0xab; 20]);
        let bytes = Bytes::from(code);
        assert_eq!(get_eip7702_delegate(&bytes), None);
    }

    // ============================================================
    // ValidationConfig Tests
    // ============================================================

    #[test]
    fn test_validation_config_default() {
        let config = ValidationConfig::default();

        assert!(config.enable_tracing);
        assert!(config.enable_prechecks);
        assert_eq!(config.tracer_config.with_log, Some(false));
        assert!(config.tracer_config.ignored_opcodes.is_empty());
    }

    #[test]
    fn test_validation_config_custom() {
        let config = ValidationConfig {
            enable_tracing: false,
            enable_prechecks: false,
            staking_config: StakingConfig {
                min_stake_value: U256::from(1000),
                min_unstake_delay: 100,
            },
            precheck_config: PrecheckConfig {
                max_verification_gas: 1_000_000,
                max_call_gas_limit: 5_000_000,
                max_total_gas_cost: U256::from(1_000_000_000_000_000_000u128),
                check_payer_balance: false,
                allow_eip7702: false,
            },
            tracer_config: Erc7562Config {
                stack_top_items_size: Some(3),
                ignored_opcodes: vec![0x31],
                with_log: Some(true),
            },
        };

        assert!(!config.enable_tracing);
        assert!(!config.enable_prechecks);
        assert_eq!(config.staking_config.min_stake_value, U256::from(1000));
        assert_eq!(config.precheck_config.max_verification_gas, 1_000_000);
        assert!(!config.precheck_config.allow_eip7702);
        assert_eq!(config.tracer_config.ignored_opcodes, vec![0x31]);
    }

    // ============================================================
    // EIP-7702 Delegation Prefix Constant Test
    // ============================================================

    #[test]
    fn test_eip7702_delegation_prefix() {
        assert_eq!(EIP7702_DELEGATION_PREFIX, [0xef, 0x01, 0x00]);
    }
}
