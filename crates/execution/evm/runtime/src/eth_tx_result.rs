//! Shared transaction execution result.
use base_execution_evm_runtime::{ResultAndState, block::TxResult};

/// The result of executing an Ethereum transaction.
#[derive(Debug)]
pub struct EthTxResult<H, T> {
    /// Result of the transaction execution.
    pub result: ResultAndState<H>,
    /// Blob gas used by the transaction.
    pub blob_gas_used: u64,
    /// Type of the transaction.
    pub tx_type: T,
}

impl<H, T> TxResult for EthTxResult<H, T>
where
    H: Send + 'static,
    T: Send + 'static,
{
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.result
    }
}
