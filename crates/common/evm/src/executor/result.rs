//! Contains the [`BaseTxResult`] type.

/// The result of executing an OP transaction.
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
}

