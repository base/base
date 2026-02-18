/// Transaction execution types and errors.
///
/// Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)
use core::fmt::Debug;

use alloy_primitives::{Address, U256};
use derive_more::Display;
use op_revm::OpTransactionError;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use thiserror::Error;

use crate::FlashblocksExecutionInfo;

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
    ///
    /// This is a "use it or lose it" budget - unused time does not carry over between
    /// flashblocks. The budget resets at the start of each flashblock.
    pub flashblock_execution_time_limit_us: Option<u128>,
    /// Maximum state root calculation time per transaction in microseconds (optional).
    pub tx_state_root_time_limit_us: Option<u128>,
    /// Maximum cumulative state root calculation time for the block in microseconds (optional).
    ///
    /// Unlike execution time, state root time is cumulative across the entire block because
    /// state root is calculated once at the end of the block, not per-flashblock.
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

/// Execution metering limits that depend on metering service predictions.
/// These can operate in dry-run or enforcement mode via the execution metering mode setting.
#[derive(Debug, Error, Clone)]
pub enum ExecutionMeteringLimitExceeded {
    /// Transaction execution time exceeded the per-transaction limit.
    #[error("transaction execution time exceeded: tx_time_us={0} limit_us={1}")]
    TransactionExecutionTime(u128, u128),
    /// Flashblock execution time exceeded the flashblock-level limit.
    #[error(
        "flashblock execution time exceeded: flashblock_used_us={0} tx_time_us={1} limit_us={2}"
    )]
    FlashblockExecutionTime(u128, u128, u128),
    /// Transaction state root time exceeded the per-transaction limit.
    #[error("transaction state root time exceeded: tx_time_us={0} limit_us={1}")]
    TransactionStateRootTime(u128, u128),
    /// Block state root time exceeded the block-level limit.
    #[error("block state root time exceeded: cumulative_us={0} tx_time_us={1} block_limit_us={2}")]
    BlockStateRootTime(u128, u128, u128),
}

/// Error returned when a transaction fails execution or exceeds block limits.
#[derive(Debug, Error, Clone)]
pub enum TxnExecutionError {
    // DA size limits (always enforced, operator-configured)
    /// Transaction DA size exceeds the per-transaction limit.
    #[error("transaction DA size exceeded: tx_da_size={0} limit={1}")]
    TransactionDASizeExceeded(u64, u64),

    /// Block DA size limit exceeded.
    #[error(
        "block DA size exceeded: total_da_used={total_da_used} tx_da_size={tx_da_size} block_da_limit={block_da_limit}"
    )]
    BlockDASizeExceeded {
        /// Total DA bytes used before this transaction.
        total_da_used: u64,
        /// DA size of this transaction.
        tx_da_size: u64,
        /// Block DA limit.
        block_da_limit: u64,
    },

    // Protocol-enforced limits (always rejected)
    /// DA footprint limit exceeded (post-Jovian, protocol-enforced).
    #[error(
        "DA footprint limit exceeded: total_da_used={total_da_used} tx_da_size={tx_da_size} da_footprint={da_footprint}"
    )]
    DAFootprintLimitExceeded {
        /// Total DA bytes used before this transaction.
        total_da_used: u64,
        /// DA size of this transaction.
        tx_da_size: u64,
        /// Computed DA footprint that exceeded the limit.
        da_footprint: u64,
    },

    /// Transaction gas limit exceeds remaining block gas.
    #[error(
        "transaction gas limit exceeded: cumulative_gas_used={cumulative_gas_used} tx_gas_limit={tx_gas_limit} block_gas_limit={block_gas_limit}"
    )]
    TransactionGasLimitExceeded {
        /// Cumulative gas used before this transaction.
        cumulative_gas_used: u64,
        /// Gas limit of this transaction.
        tx_gas_limit: u64,
        /// Block gas limit.
        block_gas_limit: u64,
    },

    // Execution metering limits (optionally enforced, depend on metering service predictions)
    /// Execution metering limit exceeded (execution time or state root time).
    #[error("{0}")]
    ExecutionMeteringLimitExceeded(ExecutionMeteringLimitExceeded),

    // Transaction status
    /// Transaction is a sequencer transaction (skipped).
    #[error("sequencer transaction")]
    SequencerTransaction,

    /// Transaction nonce is too low.
    #[error("nonce too low")]
    NonceTooLow,

    /// Interop validation failed.
    #[error("interop failed")]
    InteropFailed,

    /// Internal EVM error during transaction execution.
    #[error("internal error: {0}")]
    InternalError(OpTransactionError),

    /// EVM execution error.
    #[error("EVM error")]
    EvmError,

    /// Transaction gas usage exceeds configured maximum.
    #[error("max gas usage exceeded")]
    MaxGasUsageExceeded,
}

impl From<ExecutionMeteringLimitExceeded> for TxnExecutionError {
    fn from(err: ExecutionMeteringLimitExceeded) -> Self {
        Self::ExecutionMeteringLimitExceeded(err)
    }
}

/// Outcome of transaction execution for logging purposes.
#[derive(Debug, Display, Clone, Copy)]
pub enum TxnOutcome {
    /// Transaction executed successfully.
    Success,
    /// Transaction reverted but was included.
    Reverted,
    /// Transaction reverted and was excluded from the block.
    RevertedAndExcluded,
}

/// Aggregated execution information for the current block.
#[derive(Default, Debug)]
pub struct ExecutionInfo {
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
    /// Reset at the start of each flashblock (see [`ResourceLimits::flashblock_execution_time_limit_us`]).
    pub flashblock_execution_time_us: u128,
    /// Cumulative state root calculation time in microseconds across the block
    /// (see [`ResourceLimits::block_state_root_time_limit_us`]).
    pub cumulative_state_root_time_us: u128,
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
    /// Extra execution information for the Flashblocks builder
    pub extra: FlashblocksExecutionInfo,
    /// DA Footprint Scalar for Jovian
    pub da_footprint_scalar: Option<u16>,
}

impl ExecutionInfo {
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
    ///   predicted state root time does not exceed per-tx or per-block limits.
    pub fn is_tx_over_limits(
        &self,
        tx: &TxResources,
        limits: &ResourceLimits,
    ) -> Result<(), TxnExecutionError> {
        use ExecutionMeteringLimitExceeded::*;

        // Check per-transaction DA size limit (always enforced, operator-configured)
        if let Some(da_limit) = limits.tx_data_limit
            && tx.da_size > da_limit
        {
            return Err(TxnExecutionError::TransactionDASizeExceeded(tx.da_size, da_limit));
        }

        // Check block DA size limit (always enforced, operator-configured)
        let total_da_bytes_used = self.cumulative_da_bytes_used.saturating_add(tx.da_size);
        if let Some(da_limit) = limits.block_data_limit
            && total_da_bytes_used > da_limit
        {
            return Err(TxnExecutionError::BlockDASizeExceeded {
                total_da_used: self.cumulative_da_bytes_used,
                tx_da_size: tx.da_size,
                block_da_limit: da_limit,
            });
        }

        // Post Jovian: the tx DA footprint must be less than the block gas limit (protocol-enforced)
        if let Some(da_footprint_gas_scalar) = limits.da_footprint_gas_scalar {
            let tx_da_footprint =
                total_da_bytes_used.saturating_mul(da_footprint_gas_scalar as u64);
            if tx_da_footprint > limits.block_da_footprint_limit.unwrap_or(limits.block_gas_limit) {
                return Err(TxnExecutionError::DAFootprintLimitExceeded {
                    total_da_used: total_da_bytes_used,
                    tx_da_size: tx.da_size,
                    da_footprint: tx_da_footprint,
                });
            }
        }

        // Check gas limit
        if self.cumulative_gas_used + tx.gas_limit > limits.block_gas_limit {
            return Err(TxnExecutionError::TransactionGasLimitExceeded {
                cumulative_gas_used: self.cumulative_gas_used,
                tx_gas_limit: tx.gas_limit,
                block_gas_limit: limits.block_gas_limit,
            });
        }

        // Check execution time limits (if metering data is available)
        if let Some(tx_time) = tx.execution_time_us {
            // Check per-transaction execution time limit
            if let Some(tx_limit) = limits.tx_execution_time_limit_us
                && tx_time > tx_limit
            {
                return Err(TransactionExecutionTime(tx_time, tx_limit).into());
            }

            // Check flashblock execution time limit
            if let Some(flashblock_limit) = limits.flashblock_execution_time_limit_us {
                let total_time = self.flashblock_execution_time_us.saturating_add(tx_time);
                if total_time > flashblock_limit {
                    return Err(FlashblockExecutionTime(
                        self.flashblock_execution_time_us,
                        tx_time,
                        flashblock_limit,
                    )
                    .into());
                }
            }
        }

        // Check state root time limits (if metering data is available)
        if let Some(tx_time) = tx.state_root_time_us {
            // Check per-transaction state root time limit
            if let Some(tx_limit) = limits.tx_state_root_time_limit_us
                && tx_time > tx_limit
            {
                return Err(TransactionStateRootTime(tx_time, tx_limit).into());
            }

            // Check block state root time limit (cumulative across the block)
            if let Some(block_limit) = limits.block_state_root_time_limit_us {
                let total_time = self.cumulative_state_root_time_us.saturating_add(tx_time);
                if total_time > block_limit {
                    return Err(BlockStateRootTime(
                        self.cumulative_state_root_time_us,
                        tx_time,
                        block_limit,
                    )
                    .into());
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create default limits with block gas limit set
    fn default_limits() -> ResourceLimits {
        ResourceLimits { block_gas_limit: 30_000_000, ..Default::default() }
    }

    // ==================== Basic Limit Tests ====================

    #[test]
    fn test_tx_within_all_limits() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = default_limits();
        let tx = TxResources { da_size: 100, gas_limit: 21_000, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    #[test]
    fn test_gas_limit_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_gas_used = 29_990_000;

        let limits = default_limits();
        let tx = TxResources { gas_limit: 21_000, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::TransactionGasLimitExceeded { .. })));
    }

    #[test]
    fn test_gas_limit_exactly_at_limit() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_gas_used = 29_979_000;

        let limits = default_limits();
        let tx = TxResources { gas_limit: 21_000, ..Default::default() };

        // 29_979_000 + 21_000 = 30_000_000, exactly at limit
        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    // ==================== DA Limit Tests ====================

    #[test]
    fn test_tx_da_limit_exceeded() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits { tx_data_limit: Some(1000), ..default_limits() };
        let tx = TxResources { da_size: 1001, gas_limit: 21_000, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::TransactionDASizeExceeded(1001, 1000))));
    }

    #[test]
    fn test_block_da_limit_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_da_bytes_used = 9500;

        let limits = ResourceLimits { block_data_limit: Some(10_000), ..default_limits() };
        let tx = TxResources { da_size: 600, gas_limit: 21_000, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::BlockDASizeExceeded { .. })));
    }

    #[test]
    fn test_da_footprint_limit_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_da_bytes_used = 1_000_000;

        let limits = ResourceLimits {
            da_footprint_gas_scalar: Some(16),
            block_da_footprint_limit: Some(20_000_000),
            ..default_limits()
        };
        // 1_000_000 + 500_000 = 1_500_000, * 16 = 24_000_000 > 20_000_000
        let tx = TxResources { da_size: 500_000, gas_limit: 21_000, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::DAFootprintLimitExceeded { .. })));
    }

    // ==================== Execution Time Tests ====================

    #[test]
    fn test_tx_execution_time_exceeded() {
        let info = ExecutionInfo::with_capacity(10);
        let limits =
            ResourceLimits { tx_execution_time_limit_us: Some(1_000_000), ..default_limits() };
        let tx = TxResources {
            gas_limit: 21_000,
            execution_time_us: Some(1_500_000), // 1.5s > 1s limit
            ..Default::default()
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::TransactionExecutionTime(1_500_000, 1_000_000)
            ))
        ));
    }

    #[test]
    fn test_flashblock_execution_time_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.flashblock_execution_time_us = 4_000_000; // 4s already used

        let limits = ResourceLimits {
            flashblock_execution_time_limit_us: Some(5_000_000), // 5s limit
            ..default_limits()
        };
        let tx = TxResources {
            gas_limit: 21_000,
            execution_time_us: Some(2_000_000), // 2s would exceed
            ..Default::default()
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::FlashblockExecutionTime(
                    4_000_000, 2_000_000, 5_000_000
                )
            ))
        ));
    }

    #[test]
    fn test_execution_time_within_limits() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.flashblock_execution_time_us = 2_000_000;

        let limits = ResourceLimits {
            tx_execution_time_limit_us: Some(1_000_000),
            flashblock_execution_time_limit_us: Some(5_000_000),
            ..default_limits()
        };
        let tx = TxResources {
            gas_limit: 21_000,
            execution_time_us: Some(500_000), // 0.5s within both limits
            ..Default::default()
        };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    #[test]
    fn test_execution_time_no_metering_data_skips_check() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            tx_execution_time_limit_us: Some(1_000),
            flashblock_execution_time_limit_us: Some(1_000),
            ..default_limits()
        };
        // No execution_time_us set - should skip the check
        let tx = TxResources { gas_limit: 21_000, execution_time_us: None, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    // ==================== State Root Time Tests ====================

    #[test]
    fn test_tx_state_root_time_exceeded() {
        let info = ExecutionInfo::with_capacity(10);
        let limits =
            ResourceLimits { tx_state_root_time_limit_us: Some(500_000), ..default_limits() };
        let tx = TxResources {
            gas_limit: 21_000,
            state_root_time_us: Some(600_000), // 0.6s > 0.5s limit
            ..Default::default()
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::TransactionStateRootTime(600_000, 500_000)
            ))
        ));
    }

    #[test]
    fn test_block_state_root_time_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_state_root_time_us = 8_000_000; // 8s already used

        let limits = ResourceLimits {
            block_state_root_time_limit_us: Some(10_000_000), // 10s block limit
            ..default_limits()
        };
        let tx = TxResources {
            gas_limit: 21_000,
            state_root_time_us: Some(3_000_000), // 3s would exceed (8 + 3 > 10)
            ..Default::default()
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::BlockStateRootTime(
                    8_000_000, 3_000_000, 10_000_000
                )
            ))
        ));
    }

    #[test]
    fn test_state_root_time_within_limits() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_state_root_time_us = 5_000_000;

        let limits = ResourceLimits {
            tx_state_root_time_limit_us: Some(1_000_000),
            block_state_root_time_limit_us: Some(10_000_000),
            ..default_limits()
        };
        let tx = TxResources {
            gas_limit: 21_000,
            state_root_time_us: Some(500_000),
            ..Default::default()
        };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    #[test]
    fn test_state_root_time_no_metering_data_skips_check() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            tx_state_root_time_limit_us: Some(1),
            block_state_root_time_limit_us: Some(1),
            ..default_limits()
        };
        let tx = TxResources { gas_limit: 21_000, state_root_time_us: None, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    // ==================== Reset Behavior Tests ====================

    #[test]
    fn test_reset_flashblock_execution_time() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.flashblock_execution_time_us = 5_000_000;
        info.cumulative_state_root_time_us = 3_000_000;
        info.cumulative_gas_used = 1_000_000;
        info.cumulative_da_bytes_used = 50_000;

        info.reset_flashblock_execution_time();

        // Only execution time should be reset
        assert_eq!(info.flashblock_execution_time_us, 0);
        // Other cumulative values should remain
        assert_eq!(info.cumulative_state_root_time_us, 3_000_000);
        assert_eq!(info.cumulative_gas_used, 1_000_000);
        assert_eq!(info.cumulative_da_bytes_used, 50_000);
    }

    #[test]
    fn test_execution_time_resets_between_flashblocks() {
        let mut info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            flashblock_execution_time_limit_us: Some(5_000_000),
            ..default_limits()
        };

        // First flashblock: use 4s
        info.flashblock_execution_time_us = 4_000_000;

        // This tx (2s) would exceed in current flashblock
        let tx = TxResources {
            gas_limit: 21_000,
            execution_time_us: Some(2_000_000),
            ..Default::default()
        };
        assert!(info.is_tx_over_limits(&tx, &limits).is_err());

        // Reset for new flashblock
        info.reset_flashblock_execution_time();

        // Same tx now fits
        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    #[test]
    fn test_state_root_time_cumulative_across_flashblocks() {
        let mut info = ExecutionInfo::with_capacity(10);
        let limits =
            ResourceLimits { block_state_root_time_limit_us: Some(10_000_000), ..default_limits() };

        // Simulate multiple flashblocks accumulating state root time
        info.cumulative_state_root_time_us = 3_000_000;
        info.reset_flashblock_execution_time();

        info.cumulative_state_root_time_us = 6_000_000;
        info.reset_flashblock_execution_time();

        // Flashblock 3: try to add 5s (would be 11s > 10s limit)
        let tx = TxResources {
            gas_limit: 21_000,
            state_root_time_us: Some(5_000_000),
            ..Default::default()
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::BlockStateRootTime(_, _, _)
            ))
        ));
    }

    // ==================== Combined Resource Tests ====================

    #[test]
    fn test_multiple_limits_first_exceeded_wins() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            tx_data_limit: Some(100),
            tx_execution_time_limit_us: Some(1_000_000),
            tx_state_root_time_limit_us: Some(500_000),
            ..default_limits()
        };

        // DA limit exceeded first (checked before execution time)
        let tx = TxResources {
            da_size: 200,
            gas_limit: 21_000,
            execution_time_us: Some(2_000_000),
            state_root_time_us: Some(1_000_000),
        };

        let result = info.is_tx_over_limits(&tx, &limits);
        // DA size limit is checked first
        assert!(matches!(result, Err(TxnExecutionError::TransactionDASizeExceeded(200, 100))));
    }

    #[test]
    fn test_all_limits_configured_tx_passes() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            block_gas_limit: 30_000_000,
            tx_data_limit: Some(10_000),
            block_data_limit: Some(1_000_000),
            tx_execution_time_limit_us: Some(1_000_000),
            flashblock_execution_time_limit_us: Some(10_000_000),
            tx_state_root_time_limit_us: Some(500_000),
            block_state_root_time_limit_us: Some(5_000_000),
            ..Default::default()
        };

        let tx = TxResources {
            da_size: 500,
            gas_limit: 100_000,
            execution_time_us: Some(100_000),
            state_root_time_us: Some(50_000),
        };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    // ==================== Edge Cases ====================

    #[test]
    fn test_zero_limits() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            block_gas_limit: 0,
            tx_execution_time_limit_us: Some(0),
            ..Default::default()
        };
        let tx = TxResources { gas_limit: 1, execution_time_us: Some(1), ..Default::default() };

        // Should fail on gas limit
        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::TransactionGasLimitExceeded { .. })));
    }

    #[test]
    fn test_saturating_add_prevents_overflow() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.flashblock_execution_time_us = u128::MAX - 100;

        let limits = ResourceLimits {
            flashblock_execution_time_limit_us: Some(u128::MAX),
            ..default_limits()
        };
        let tx = TxResources {
            gas_limit: 21_000,
            execution_time_us: Some(200), // Would overflow without saturating_add
            ..Default::default()
        };

        // Should not panic, saturating add caps at u128::MAX
        let result = info.is_tx_over_limits(&tx, &limits);
        // u128::MAX - 100 + 200 saturates to u128::MAX, which equals the limit
        assert!(result.is_ok());
    }

    #[test]
    fn test_with_capacity_initializes_correctly() {
        let info = ExecutionInfo::with_capacity(100);

        assert_eq!(info.cumulative_gas_used, 0);
        assert_eq!(info.cumulative_da_bytes_used, 0);
        assert_eq!(info.flashblock_execution_time_us, 0);
        assert_eq!(info.cumulative_state_root_time_us, 0);
        assert_eq!(info.total_fees, U256::ZERO);
        assert!(info.executed_transactions.is_empty());
        assert!(info.executed_senders.is_empty());
        assert!(info.receipts.is_empty());
    }
}
