//! Contains the [`BaseTxResult`] type.

use alloy_evm::{block::TxResult as TxResultTrait, eth::EthTxResult};
use alloy_primitives::Address;
use revm::context::result::ResultAndState;

/// The result of executing a Base transaction.
#[derive(Debug)]
pub struct BaseTxResult<H, T> {
    /// The inner result of the transaction execution.
    pub inner: EthTxResult<H, T>,
    /// Whether the transaction is a deposit transaction.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
}

impl<H, T> TxResultTrait for BaseTxResult<H, T> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result
    }
}
