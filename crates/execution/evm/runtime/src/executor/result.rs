//! Contains the [`BaseTxResult`] type.

use alloy_primitives::Address;
use base_execution_evm_runtime::{
    AccountInfo, EthTxResult, ResultAndState, TxResult as TxResultTrait,
};

/// The result of executing a Base transaction.
#[derive(Debug)]
pub struct BaseTxResult {
    /// The inner result of the transaction execution.
    pub inner: EthTxResult<crate::BaseHaltReason, base_common_types_chain::OpTxType>,
    /// Whether the transaction is a deposit transaction.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
    /// The depositor account info, fetched during execution for post-Regolith deposit nonce.
    pub depositor: Option<AccountInfo>,
}

impl TxResultTrait for BaseTxResult {
    type HaltReason = crate::BaseHaltReason;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result
    }
}
