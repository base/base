use alloy_primitives::Address;
use base_alloy_consensus::OpTxType;
use revm::context::result::ResultAndState;

/// Combined execution result for an OP transaction, used for caching pre-executed results.
#[derive(Debug, Clone)]
pub struct OpTxResult<H> {
    /// The raw EVM execution output including state changes.
    pub result: ResultAndState<H>,
    /// Whether the transaction is a deposit.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
    /// Blob gas used by the transaction.
    pub blob_gas_used: u64,
    /// The transaction type.
    pub tx_type: OpTxType,
}

impl<H: Clone> OpTxResult<H> {
    /// Returns a reference to the [`ResultAndState`].
    pub fn result(&self) -> &ResultAndState<H> {
        &self.result
    }
}
