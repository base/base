//! Contains the [`BaseTxResult`] type.

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
    /// The EIP-8130 `committedPhases` of the transaction: how many leading call phases
    /// committed. Zero for every other transaction type. Carried with the result so the receipt
    /// can be built on a different thread from the one that executed the transaction.
    pub eip8130_committed_phases: u64,
    /// Gas charged to the block for this transaction when gas-limit fee billing (Zenith) is
    /// active: the gas a non-deposit transaction reserved against the block (its declared
    /// `gas_limit`, plus payer authentication for EIP-8130), regardless of how much it
    /// consumed. `None` keeps the gas the result reports as used, which is the pre-billing
    /// behavior and also the deposit case.
    pub billed_gas: Option<u64>,
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
