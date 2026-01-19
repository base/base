//! Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)
use core::fmt::Debug;

use alloy_primitives::{Address, U256};
use derive_more::Display;
use op_revm::OpTransactionError;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};

/// Resource limits configuration for transaction and block constraints.
///
/// This struct encapsulates all the resource limit parameters used to determine
/// whether a transaction can be included in a block without exceeding various
/// resource budgets (gas, DA, execution time).
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// The block gas limit.
    pub block_gas_limit: u64,
    /// Maximum DA bytes per transaction (optional).
    pub tx_data_limit: Option<u64>,
    /// Maximum DA bytes per block (optional).
    pub block_data_limit: Option<u64>,
    /// DA footprint scalar for Jovian (optional).
    pub da_footprint_gas_scalar: Option<u16>,
    /// Maximum DA footprint for the block (optional).
    pub block_da_footprint_limit: Option<u64>,
    /// Maximum execution time per transaction in microseconds (optional).
    pub tx_execution_time_limit_us: Option<u128>,
    /// Maximum execution time budget for the current flashblock in microseconds (optional).
    /// This is a "use it or lose it" budget - unused time does not carry over.
    pub flashblock_execution_time_limit_us: Option<u128>,
    /// Maximum state root calculation time per transaction in microseconds (optional).
    pub tx_state_root_time_limit_us: Option<u128>,
    /// Maximum cumulative state root calculation time for the block in microseconds (optional).
    /// Unlike execution time, state root time is cumulative across the block since state root
    /// is calculated once at the end.
    pub block_state_root_time_limit_us: Option<u128>,
}

/// Resource usage for a single transaction.
///
/// This struct contains the resource consumption values for a transaction,
/// both predicted (from metering data) and declared (from tx fields).
#[derive(Debug, Clone, Default)]
pub struct TxResources {
    /// Estimated DA size for the transaction.
    pub da_size: u64,
    /// Declared gas limit from the transaction.
    pub gas_limit: u64,
    /// Predicted execution time in microseconds (from metering data, if available).
    pub execution_time_us: Option<u128>,
    /// Predicted state root calculation time in microseconds (from metering data, if available).
    pub state_root_time_us: Option<u128>,
}

#[derive(Debug, Display)]
pub enum TxnExecutionResult {
    TransactionDALimitExceeded,
    #[display("BlockDALimitExceeded: total_da_used={_0} tx_da_size={_1} block_da_limit={_2}")]
    BlockDALimitExceeded(u64, u64, u64),
    #[display("TransactionGasLimitExceeded: total_gas_used={_0} tx_gas_limit={_1}")]
    TransactionGasLimitExceeded(u64, u64, u64),
    #[display("TransactionExecutionTimeExceeded: tx_time_us={_0} limit_us={_1}")]
    TransactionExecutionTimeExceeded(u128, u128),
    #[display(
        "FlashblockExecutionTimeExceeded: flashblock_used_us={_0} tx_time_us={_1} limit_us={_2}"
    )]
    FlashblockExecutionTimeExceeded(u128, u128, u128),
    #[display("TransactionStateRootTimeExceeded: tx_time_us={_0} limit_us={_1}")]
    TransactionStateRootTimeExceeded(u128, u128),
    #[display("BlockStateRootTimeExceeded: cumulative_us={_0} tx_time_us={_1} block_limit_us={_2}")]
    BlockStateRootTimeExceeded(u128, u128, u128),
    SequencerTransaction,
    NonceTooLow,
    InteropFailed,
    #[display("InternalError({_0})")]
    InternalError(OpTransactionError),
    EvmError,
    Success,
    Reverted,
    RevertedAndExcluded,
    MaxGasUsageExceeded,
}

#[derive(Default, Debug)]
pub struct ExecutionInfo<Extra: Debug + Default = ()> {
    /// All executed transactions (unrecovered).
    pub executed_transactions: Vec<OpTransactionSigned>,
    /// The recovered senders for the executed transactions.
    pub executed_senders: Vec<Address>,
    /// The transaction receipts
    pub receipts: Vec<OpReceipt>,
    /// All gas used so far
    pub cumulative_gas_used: u64,
    /// Estimated DA size
    pub cumulative_da_bytes_used: u64,
    /// Execution time used in the current flashblock in microseconds.
    /// This is a "use it or lose it" resource - reset at the start of each flashblock.
    pub flashblock_execution_time_us: u128,
    /// Cumulative state root calculation time in microseconds.
    /// Unlike execution time, this is cumulative across the block since state root
    /// is calculated once at the end.
    pub cumulative_state_root_time_us: u128,
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
    /// Extra execution information that can be attached by individual builders.
    pub extra: Extra,
    /// DA Footprint Scalar for Jovian
    pub da_footprint_scalar: Option<u16>,
}

impl<T: Debug + Default> ExecutionInfo<T> {
    /// Create a new instance with allocated slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            executed_transactions: Vec::with_capacity(capacity),
            executed_senders: Vec::with_capacity(capacity),
            receipts: Vec::with_capacity(capacity),
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            flashblock_execution_time_us: 0,
            cumulative_state_root_time_us: 0,
            total_fees: U256::ZERO,
            extra: Default::default(),
            da_footprint_scalar: None,
        }
    }

    /// Reset the flashblock-scoped execution time budget for a new flashblock.
    /// Called at the start of each flashblock to reset the "use it or lose it" resource.
    /// Note: State root time is NOT reset here since it's cumulative across the block.
    pub const fn reset_flashblock_execution_time(&mut self) {
        self.flashblock_execution_time_us = 0;
    }

    /// Returns true if the transaction would exceed the block limits:
    /// - block gas limit: ensures the transaction still fits into the block.
    /// - tx DA limit: if configured, ensures the tx does not exceed the maximum allowed DA limit
    ///   per tx.
    /// - block DA limit: if configured, ensures the transaction's DA size does not exceed the
    ///   maximum allowed DA limit per block.
    /// - execution time limits: if configured with metering data, ensures the transaction's
    ///   predicted execution time does not exceed per-tx or per-flashblock limits.
    /// - state root time limits: if configured with metering data, ensures the transaction's
    ///   predicted state root time does not exceed per-tx or per-flashblock limits.
    pub fn is_tx_over_limits(
        &self,
        tx: &TxResources,
        limits: &ResourceLimits,
    ) -> Result<(), TxnExecutionResult> {
        // Check per-transaction DA limit
        if limits.tx_data_limit.is_some_and(|da_limit| tx.da_size > da_limit) {
            return Err(TxnExecutionResult::TransactionDALimitExceeded);
        }

        // Check block DA limit
        let total_da_bytes_used = self.cumulative_da_bytes_used.saturating_add(tx.da_size);
        if limits.block_data_limit.is_some_and(|da_limit| total_da_bytes_used > da_limit) {
            return Err(TxnExecutionResult::BlockDALimitExceeded(
                self.cumulative_da_bytes_used,
                tx.da_size,
                limits.block_data_limit.unwrap_or_default(),
            ));
        }

        // Post Jovian: the tx DA footprint must be less than the block gas limit
        if let Some(da_footprint_gas_scalar) = limits.da_footprint_gas_scalar {
            let tx_da_footprint =
                total_da_bytes_used.saturating_mul(da_footprint_gas_scalar as u64);
            if tx_da_footprint > limits.block_da_footprint_limit.unwrap_or(limits.block_gas_limit) {
                return Err(TxnExecutionResult::BlockDALimitExceeded(
                    total_da_bytes_used,
                    tx.da_size,
                    tx_da_footprint,
                ));
            }
        }

        // Check gas limit
        if self.cumulative_gas_used + tx.gas_limit > limits.block_gas_limit {
            return Err(TxnExecutionResult::TransactionGasLimitExceeded(
                self.cumulative_gas_used,
                tx.gas_limit,
                limits.block_gas_limit,
            ));
        }

        // Check execution time limits (if metering data is available)
        // Execution time is a "use it or lose it" resource per flashblock
        if let Some(tx_time) = tx.execution_time_us {
            // Check per-transaction execution time limit
            if let Some(tx_limit) = limits.tx_execution_time_limit_us
                && tx_time > tx_limit
            {
                return Err(TxnExecutionResult::TransactionExecutionTimeExceeded(
                    tx_time, tx_limit,
                ));
            }

            // Check flashblock execution time limit
            if let Some(flashblock_limit) = limits.flashblock_execution_time_limit_us {
                let total_time = self.flashblock_execution_time_us.saturating_add(tx_time);
                if total_time > flashblock_limit {
                    return Err(TxnExecutionResult::FlashblockExecutionTimeExceeded(
                        self.flashblock_execution_time_us,
                        tx_time,
                        flashblock_limit,
                    ));
                }
            }
        }

        // Check state root time limits (if metering data is available)
        // State root time is a "use it or lose it" resource per flashblock
        if let Some(tx_time) = tx.state_root_time_us {
            // Check per-transaction state root time limit
            if let Some(tx_limit) = limits.tx_state_root_time_limit_us
                && tx_time > tx_limit
            {
                return Err(TxnExecutionResult::TransactionStateRootTimeExceeded(
                    tx_time, tx_limit,
                ));
            }

            // Check block state root time limit (cumulative across the block)
            if let Some(block_limit) = limits.block_state_root_time_limit_us {
                let total_time = self.cumulative_state_root_time_us.saturating_add(tx_time);
                if total_time > block_limit {
                    return Err(TxnExecutionResult::BlockStateRootTimeExceeded(
                        self.cumulative_state_root_time_us,
                        tx_time,
                        block_limit,
                    ));
                }
            }
        }

        Ok(())
    }
}
