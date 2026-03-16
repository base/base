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
    /// The txpool rejected the transaction because the nonce slot is already
    /// reserved by a stuck transaction. Frames are requeued and no new
    /// submissions are attempted until the blockage is cleared.
    TxpoolBlocked,
}
