//! Transaction execution types and errors.
//!
//! Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)

use core::fmt::Debug;

use alloy_primitives::{Address, U256};
use op_revm::OpTransactionError;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use thiserror::Error;

use crate::flashblocks::FlashblocksExecutionInfo;

/// Error returned when a transaction fails execution or exceeds block limits.
#[derive(Debug, Error, Clone)]
pub enum TxnExecutionError {
    /// Transaction data availability size exceeds the per-transaction limit.
    #[error("transaction DA limit exceeded")]
    TransactionDALimitExceeded,

    /// Block data availability limit exceeded.
    #[error(
        "block DA limit exceeded: total_da_used={total_da_used} tx_da_size={tx_da_size} block_da_limit={block_da_limit}"
    )]
    BlockDALimitExceeded {
        /// Total DA bytes used before this transaction.
        total_da_used: u64,
        /// DA size of this transaction.
        tx_da_size: u64,
        /// Block DA limit.
        block_da_limit: u64,
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

/// Outcome of transaction execution for logging purposes.
#[derive(Debug, Clone, Copy)]
pub enum TxnOutcome {
    /// Transaction executed successfully.
    Success,
    /// Transaction reverted but was included.
    Reverted,
    /// Transaction reverted and was excluded from the block.
    RevertedAndExcluded,
}

impl std::fmt::Display for TxnOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "Success"),
            Self::Reverted => write!(f, "Reverted"),
            Self::RevertedAndExcluded => write!(f, "RevertedAndExcluded"),
        }
    }
}

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
            total_fees: U256::ZERO,
            extra: Default::default(),
            da_footprint_scalar: None,
        }
    }

    /// Returns true if the transaction would exceed the block limits:
    /// - block gas limit: ensures the transaction still fits into the block.
    /// - tx DA limit: if configured, ensures the tx does not exceed the maximum allowed DA limit
    ///   per tx.
    /// - block DA limit: if configured, ensures the transaction's DA size does not exceed the
    ///   maximum allowed DA limit per block.
    #[allow(clippy::too_many_arguments)]
    pub fn is_tx_over_limits(
        &self,
        tx_da_size: u64,
        block_gas_limit: u64,
        tx_data_limit: Option<u64>,
        block_data_limit: Option<u64>,
        tx_gas_limit: u64,
        da_footprint_gas_scalar: Option<u16>,
        block_da_footprint_limit: Option<u64>,
    ) -> Result<(), TxnExecutionError> {
        if tx_data_limit.is_some_and(|da_limit| tx_da_size > da_limit) {
            return Err(TxnExecutionError::TransactionDALimitExceeded);
        }
        let total_da_bytes_used = self.cumulative_da_bytes_used.saturating_add(tx_da_size);
        if block_data_limit.is_some_and(|da_limit| total_da_bytes_used > da_limit) {
            return Err(TxnExecutionError::BlockDALimitExceeded {
                total_da_used: self.cumulative_da_bytes_used,
                tx_da_size,
                block_da_limit: block_data_limit.unwrap_or_default(),
            });
        }

        // Post Jovian: the tx DA footprint must be less than the block gas limit
        if let Some(da_footprint_gas_scalar) = da_footprint_gas_scalar {
            let tx_da_footprint =
                total_da_bytes_used.saturating_mul(da_footprint_gas_scalar as u64);
            if tx_da_footprint > block_da_footprint_limit.unwrap_or(block_gas_limit) {
                return Err(TxnExecutionError::BlockDALimitExceeded {
                    total_da_used: total_da_bytes_used,
                    tx_da_size,
                    block_da_limit: tx_da_footprint,
                });
            }
        }

        if self.cumulative_gas_used + tx_gas_limit > block_gas_limit {
            return Err(TxnExecutionError::TransactionGasLimitExceeded {
                cumulative_gas_used: self.cumulative_gas_used,
                tx_gas_limit,
                block_gas_limit,
            });
        }
        Ok(())
    }
}
