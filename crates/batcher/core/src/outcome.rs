//! Transaction outcome types for the batch driver.

/// The outcome of a submitted batch transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome {
    /// Transaction confirmed at the given L1 block number.
    Confirmed {
        /// The L1 block number at which the transaction was included.
        l1_block: u64,
    },
    /// Transaction failed or timed out; frames should be requeued.
    Failed,
}
