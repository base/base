//! Provider Implementations
//!
//! This module provides various provider implementations for account abstraction:
//!
//! - `EthApiSimulationProvider`: Implements `SimulationProvider` using reth's internal
//!   `EthApi` and `DebugApi` for `eth_call` and `debug_traceCall` operations.
//!
//! - `receipt`: Provides `UserOperationReceiptProvider` for looking up and building
//!   UserOperation receipts, including log filtering and revert reason extraction.

mod receipt;

pub use receipt::{
    extract_revert_reason, filter_logs_for_user_operation, IndexedUserOperation, ReceiptError,
    RethReceiptProvider, UserOperationEvent, UserOperationLookupResult,
    UserOperationReceiptProvider, UserOperationRevertReason,
};

use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types::state::EvmOverrides;
use alloy_rpc_types_eth::state::StateOverride;
use alloy_rpc_types_trace::geth::{GethDebugTracingCallOptions, GethTrace};
use async_trait::async_trait;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::OpTransactionRequest;
use reth_rpc::DebugApi;
use reth_rpc_eth_api::helpers::{EthCall, EthState, FullEthApi, TraceExt};
use reth_rpc_eth_api::EthApiTypes;
use reth_tasks::pool::BlockingTaskGuard;
use tracing::{debug, info, warn};

use crate::estimation::{SimulationCallResult, SimulationProvider};

/// Default number of concurrent debug trace operations
const DEFAULT_MAX_TRACING_REQUESTS: usize = 4;

/// Default gas limit for simulation calls (27M = 5M verification + 20M call + 2M overhead)
const DEFAULT_SIMULATION_GAS_LIMIT: u64 = 27_000_000;

/// Wrapper that implements SimulationProvider using EthApi and DebugApi
///
/// This allows us to use the GasEstimator with reth's internal eth_call and debug_traceCall
/// without making HTTP RPC requests.
pub struct EthApiSimulationProvider<Eth> {
    eth_api: Eth,
    /// DebugApi for on-node debug tracing (debug_traceCall)
    debug_api: DebugApi<Eth>,
}

impl<Eth> EthApiSimulationProvider<Eth>
where
    Eth: Clone,
{
    /// Create a new simulation provider with debug tracing support
    pub fn new(eth_api: Eth) -> Self {
        // Create BlockingTaskGuard to limit concurrent tracing operations
        let blocking_task_guard = BlockingTaskGuard::new(DEFAULT_MAX_TRACING_REQUESTS);
        let debug_api = DebugApi::new(eth_api.clone(), blocking_task_guard);
        Self { eth_api, debug_api }
    }

    /// Create with a custom max tracing requests limit
    #[allow(dead_code)]
    pub fn with_max_tracing_requests(eth_api: Eth, max_tracing_requests: usize) -> Self {
        let blocking_task_guard = BlockingTaskGuard::new(max_tracing_requests);
        let debug_api = DebugApi::new(eth_api.clone(), blocking_task_guard);
        Self { eth_api, debug_api }
    }
}

#[async_trait]
impl<Eth> SimulationProvider for EthApiSimulationProvider<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
{
    async fn eth_call(
        &self,
        to: Address,
        data: Bytes,
        block_id: BlockId,
        state_override: Option<StateOverride>,
    ) -> Result<SimulationCallResult, String> {
        let tx_request = OpTransactionRequest::default()
            .to(to)
            .input(data.clone().into())
            .gas_limit(DEFAULT_SIMULATION_GAS_LIMIT);

        let overrides = EvmOverrides::new(state_override.clone(), None);

        debug!(
            target: "aa-simulation",
            to = %to,
            block_id = ?block_id,
            "Executing eth_call for simulation"
        );

        match EthCall::call(&self.eth_api, tx_request, Some(block_id), overrides).await {
            Ok(result_data) => {
                debug!(
                    target: "aa-simulation",
                    result_len = result_data.len(),
                    result_hex = %if result_data.len() <= 256 { 
                        format!("0x{}", hex::encode(&result_data)) 
                    } else { 
                        format!("0x{}...(truncated)", hex::encode(&result_data[..64])) 
                    },
                    "eth_call returned success"
                );
                Ok(SimulationCallResult::Success(result_data))
            }
            Err(e) => {
                let error_str = e.to_string();
                // Log the full error structure to help debug revert data extraction
                debug!(
                    target: "aa-simulation",
                    error_display = %error_str,
                    error_debug = ?e,
                    "eth_call returned error, attempting to extract revert data"
                );

                // First, try to extract revert data directly from EthApiError if possible
                // The error might contain the revert output data
                if let Some(revert_data) = Self::try_extract_revert_from_error(&e) {
                    debug!(
                        target: "aa-simulation",
                        revert_len = revert_data.len(),
                        revert_hex = %format!("0x{}", hex::encode(&revert_data)),
                        "Extracted revert data from error type"
                    );
                    return Ok(SimulationCallResult::Revert(revert_data));
                }

                // Fallback: Try to extract revert data from the error string
                // The error message might contain hex-encoded revert data
                if let Some(revert_data) = Self::extract_revert_data(&error_str) {
                    debug!(
                        target: "aa-simulation",
                        revert_len = revert_data.len(),
                        revert_hex = %format!("0x{}", hex::encode(&revert_data)),
                        "Extracted revert data from error string"
                    );
                    Ok(SimulationCallResult::Revert(revert_data))
                } else {
                    // Couldn't extract revert data - try debug_traceCall to get more details
                    warn!(
                        target: "aa-simulation",
                        error = %error_str,
                        "Could not extract revert data, falling back to debug_traceCall for details"
                    );

                    // Try to get a trace for debugging
                    self.try_debug_trace_for_error(to, data, block_id, state_override, &error_str).await
                }
            }
        }
    }

    async fn eth_get_code(
        &self,
        address: Address,
        block_id: BlockId,
    ) -> Result<Bytes, String> {
        debug!(
            target: "aa-simulation",
            address = %address,
            block_id = ?block_id,
            "Getting code at address"
        );

        EthState::get_code(&self.eth_api, address, Some(block_id))
            .await
            .map_err(|e| format!("eth_getCode failed: {}", e))
    }

    async fn debug_trace_call(
        &self,
        to: Address,
        data: Bytes,
        block_id: BlockId,
        state_override: Option<StateOverride>,
        opts: GethDebugTracingCallOptions,
    ) -> Result<Option<GethTrace>, String> {
        // Build the transaction request
        let tx_request = OpTransactionRequest::default()
            .to(to)
            .input(data.into())
            .gas_limit(DEFAULT_SIMULATION_GAS_LIMIT);

        debug!(
            target: "aa-simulation",
            to = %to,
            block_id = ?block_id,
            "Executing debug_traceCall for simulation"
        );

        // Merge state overrides into the tracing options
        let tracing_opts = GethDebugTracingCallOptions {
            state_overrides: state_override.or(opts.state_overrides),
            ..opts
        };

        // Use reth's DebugApi directly for on-node tracing
        match self.debug_api.debug_trace_call(tx_request, Some(block_id), tracing_opts).await {
            Ok(trace) => {
                debug!(
                    target: "aa-simulation",
                    "debug_traceCall completed successfully"
                );
                Ok(Some(trace))
            }
            Err(e) => {
                let error_str = e.to_string();
                warn!(
                    target: "aa-simulation",
                    error = %error_str,
                    "debug_traceCall failed"
                );
                Err(error_str)
            }
        }
    }
}

impl<Eth> EthApiSimulationProvider<Eth>
where
    Eth: FullEthApi<NetworkTypes = Optimism> + EthApiTypes + TraceExt + Clone + Send + Sync + 'static,
{
    /// Try to extract revert data directly from the error type
    /// This handles reth's EthApiError which may contain output data
    fn try_extract_revert_from_error(error: &Eth::Error) -> Option<Bytes> {
        // Use Debug format to get the full error structure
        // The error structure looks like:
        // Eth(InvalidTransaction(Revert(RevertError { output: Some(0x...) })))
        let error_debug = format!("{:?}", error);
        
        // Check for "output: Some(0x..." pattern from RevertError
        if let Some(idx) = error_debug.find("output: Some(0x") {
            let hex_start = idx + 13; // Skip "output: Some("
            return Self::extract_hex_at_position(&error_debug[hex_start..]);
        }
        
        // Fallback: Check for "output: 0x..." pattern
        if let Some(idx) = error_debug.find("output: 0x") {
            let hex_start = idx + 8; // Skip "output: "
            return Self::extract_hex_at_position(&error_debug[hex_start..]);
        }
        
        // Check for "data: 0x..." pattern
        if let Some(idx) = error_debug.find("data: 0x") {
            let hex_start = idx + 6; // Skip "data: "
            return Self::extract_hex_at_position(&error_debug[hex_start..]);
        }
        
        // Check for 'data: "0x..."' pattern (with quotes, common in JSON-RPC responses)
        if let Some(idx) = error_debug.find("data: \"0x") {
            let hex_start = idx + 7; // Skip 'data: "'
            return Self::extract_hex_at_position(&error_debug[hex_start..]);
        }
        
        None
    }

    /// Extract hex bytes starting at the given position in the string
    fn extract_hex_at_position(s: &str) -> Option<Bytes> {
        if !s.starts_with("0x") {
            return None;
        }
        
        let end = s[2..]
            .find(|c: char| !c.is_ascii_hexdigit())
            .map(|i| i + 2)
            .unwrap_or(s.len());
        let hex_str = &s[..end];
        
        if let Ok(bytes) = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str)) {
            if !bytes.is_empty() {
                return Some(Bytes::from(bytes));
            }
        }
        None
    }

    /// Extract revert data from an error string
    /// Common patterns: "execution reverted: 0x...", "revert: 0x...", or just hex data
    fn extract_revert_data(error_str: &str) -> Option<Bytes> {
        // Try to find hex data in the error string
        if let Some(idx) = error_str.find("0x") {
            let hex_part = &error_str[idx..];
            // Find end of hex string (stops at non-hex char or end)
            let end = hex_part[2..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .map(|i| i + 2)
                .unwrap_or(hex_part.len());
            let hex_str = &hex_part[..end];

            if let Ok(bytes) = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str)) {
                if !bytes.is_empty() {
                    return Some(Bytes::from(bytes));
                }
            }
        }
        None
    }

    /// Try to get debug trace when eth_call fails without revert data
    /// This helps diagnose simulation failures by showing the call trace
    async fn try_debug_trace_for_error(
        &self,
        to: Address,
        data: Bytes,
        block_id: BlockId,
        state_override: Option<StateOverride>,
        original_error: &str,
    ) -> Result<SimulationCallResult, String> {
        use alloy_rpc_types_trace::geth::{GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingOptions};

        info!(
            target: "aa-simulation",
            to = %to,
            "Attempting debug_traceCall to diagnose simulation failure"
        );

        // Build trace request with callTracer
        let tx_request = OpTransactionRequest::default()
            .to(to)
            .input(data.into())
            .gas_limit(DEFAULT_SIMULATION_GAS_LIMIT);

        let tracing_opts = GethDebugTracingCallOptions {
            tracing_options: GethDebugTracingOptions {
                tracer: Some(GethDebugTracerType::BuiltInTracer(
                    GethDebugBuiltInTracerType::CallTracer,
                )),
                ..Default::default()
            },
            state_overrides: state_override,
            block_overrides: None,
            tx_index: None,
        };

        match self.debug_api.debug_trace_call(tx_request, Some(block_id), tracing_opts).await {
            Ok(trace) => {
                // Log the trace for debugging
                Self::log_geth_trace(&trace);
                
                // Return the original error with enhanced context
                Err(format!(
                    "Simulation reverted: {}",
                    original_error
                ))
            }
            Err(trace_err) => {
                warn!(
                    target: "aa-simulation",
                    original_error = %original_error,
                    trace_error = %trace_err,
                    "debug_traceCall also failed"
                );
                Err(format!(
                    "{}. Additionally, debug_traceCall failed: {}",
                    original_error, trace_err
                ))
            }
        }
    }

    /// Log a GethTrace (callTracer) for debugging purposes
    fn log_geth_trace(trace: &GethTrace) {
        if let GethTrace::CallTracer(call_frame) = trace {
            info!(
                target: "aa-simulation",
                "=== DEBUG TRACE (callTracer) ==="
            );
            Self::log_call_frame(call_frame, 0);
            info!(
                target: "aa-simulation",
                "=== END DEBUG TRACE ==="
            );
        }
    }

    /// Log a call frame recursively
    fn log_call_frame(frame: &alloy_rpc_types_trace::geth::CallFrame, depth: usize) {
        let indent = "  ".repeat(depth);
        
        let status = if frame.revert_reason.is_some() {
            "REVERT"
        } else if frame.error.is_some() {
            "ERROR"
        } else {
            "OK"
        };

        info!(
            target: "aa-simulation",
            "{}{} {} -> {} [{}]",
            indent,
            frame.typ,
            frame.from,
            frame.to.map(|a| format!("{}", a)).unwrap_or_else(|| "CREATE".to_string()),
            status
        );

        if let Some(ref error) = frame.error {
            warn!(
                target: "aa-simulation",
                "{}  error: {}",
                indent,
                error
            );
        }

        if let Some(ref revert_reason) = frame.revert_reason {
            warn!(
                target: "aa-simulation",
                "{}  revert_reason: {}",
                indent,
                revert_reason
            );
        }

        if let Some(ref output) = frame.output {
            if !output.is_empty() && output.len() <= 256 {
                debug!(
                    target: "aa-simulation",
                    "{}  output: 0x{}",
                    indent,
                    hex::encode(output)
                );
            }
        }

        // Log nested calls
        for call in &frame.calls {
            Self::log_call_frame(call, depth + 1);
        }
    }
}

