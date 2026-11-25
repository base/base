/// Custom errors that can occur during Base block execution.
#[derive(Debug, thiserror::Error)]
pub enum BaseBlockExecutionError {
    /// Transaction DA footprint exceeds available block DA footprint.
    #[error(
        "transaction DA footprint exceeds available block DA footprint. transaction_da_footprint: {transaction_da_footprint}, available_block_da_footprint: {available_block_da_footprint}"
    )]
    TransactionDaFootprintAboveGasLimit {
        /// The DA footprint of the transaction to execute.
        transaction_da_footprint: u64,
        /// The available block DA footprint.
        available_block_da_footprint: u64,
    },
    /// Block DA footprint exceeds the block gas limit.
    #[error(
        "block DA footprint exceeds block gas limit. da_footprint_used: {da_footprint_used}, gas_limit: {gas_limit}"
    )]
    BlockDaFootprintAboveGasLimit {
        /// The DA footprint of the block.
        da_footprint_used: u64,
        /// The block gas limit, which is also its DA footprint limit.
        gas_limit: u64,
    },
}
