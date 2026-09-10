//! Contains the [`BaseTxResult`] type.

use alloy_primitives::Address;
use base_common_types_chain::OpTxType;
use base_execution_evm_runtime::{AccountInfo, BaseHaltReason, ResultAndState};

/// The result of executing a Base transaction.
#[derive(Debug)]
pub struct BaseTxResult {
    /// Execution result and state changes.
    pub result: ResultAndState<BaseHaltReason>,
    /// DA footprint accounted for by block execution.
    pub blob_gas_used: u64,
    /// Type of the executed transaction.
    pub tx_type: OpTxType,
    /// Whether the transaction is a deposit transaction.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
    /// The depositor account info, fetched during execution for post-Regolith deposit nonce.
    pub depositor: Option<AccountInfo>,
}
