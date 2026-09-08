//! Context for building receipts from execution.
use base_evm_context::ExecutionResult;
use revm::state::EvmState;

use crate::Evm;

/// Context for building a receipt.
#[derive(Debug)]
pub struct ReceiptBuilderCtx<'a, T, E: Evm> {
    /// Transaction
    pub tx_type: T,
    /// Reference to EVM. State changes should not be committed to inner database when building
    /// receipt so receipt construction can use state before transaction execution.
    pub evm: &'a E,
    /// Result of transaction execution.
    pub result: ExecutionResult<E::HaltReason>,
    /// Reference to EVM state after execution.
    pub state: &'a EvmState,
    /// Cumulative gas used.
    pub cumulative_gas_used: u64,
}
