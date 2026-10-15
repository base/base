//! Contains the [`BaseTxResult`] type.

use alloc::vec::Vec;

use alloy_evm::{block::TxResult as TxResultTrait, eth::EthTxResult};
use alloy_primitives::Address;
use revm::{context::result::ResultAndState, state::AccountInfo};

/// The result of executing a Base transaction.
#[derive(Debug)]
pub struct BaseTxResult<H, T> {
    /// The inner result of the transaction execution.
    pub inner: EthTxResult<H, T>,
    /// Whether the transaction is a deposit transaction.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
    /// The depositor account info, fetched during execution for post-Regolith deposit nonce.
    pub depositor: Option<AccountInfo>,
    /// The EIP-8130 `phaseStatuses` of the transaction, one entry per call phase. Empty for
    /// every other transaction type. Carried with the result so the receipt can be built on a
    /// different thread from the one that executed the transaction.
    pub eip8130_phase_statuses: Vec<u8>,
}

impl<H: Send + 'static, T: Send + 'static> TxResultTrait for BaseTxResult<H, T> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result
    }
}
