//! Transaction execution types and errors.
//!
//! Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)

use core::fmt::Debug;

use ExecutionMeteringLimitExceeded::TransactionExecutionTime;
use alloy_consensus::{TxReceipt, proofs};
use alloy_primitives::{Address, B256, Bloom, U256, logs_bloom};
use base_common_chains::Upgrades;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned};
use base_common_evm::BaseTransactionError;
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;
use derive_more::Display;
use thiserror::Error;

use crate::{InclusionTracker, PredicateLoadTracker};

/// Resource limits configuration for transaction and block constraints.
///
/// This struct encapsulates all the resource limit parameters used to determine
/// whether a transaction can be included in a block without exceeding various
/// resource budgets (gas, DA, and per-transaction execution time).
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
    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes (optional).
    pub block_uncompressed_size_limit: Option<u64>,
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
    /// EIP-8130 payer authentication gas, metered on top of `gas_limit` (`0` for
    /// non-8130 / self-pay transactions). Reserved against the block gas budget
    /// in addition to `gas_limit`, since the payer reimburses its own
    /// authentication beyond the declared limit.
    pub payer_auth: u64,
    /// Predicted execution time in microseconds (from metering data, if available).
    pub execution_time_us: Option<u128>,
    /// Raw EIP-2718 encoded transaction size in bytes.
    pub uncompressed_size: u64,
}

/// Execution metering limits that depend on metering service predictions.
/// These can operate in dry-run or enforcement mode via the execution metering mode setting.
#[derive(Debug, Error, Clone)]
pub enum ExecutionMeteringLimitExceeded {
    /// A single transaction's predicted execution time exceeded its per-tx limit.
    #[error("transaction execution time exceeded: tx_time_us={0} limit_us={1}")]
    TransactionExecutionTime(u128, u128),
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

    /// Block uncompressed size limit exceeded.
    #[error(
        "block uncompressed size exceeded: total_uncompressed={total_uncompressed} tx_uncompressed_size={tx_uncompressed_size} block_limit={block_limit}"
    )]
    BlockUncompressedSizeExceeded {
        /// Total uncompressed bytes before this transaction.
        total_uncompressed: u64,
        /// Uncompressed size of this transaction.
        tx_uncompressed_size: u64,
        /// Block uncompressed size limit.
        block_limit: u64,
    },

    // Execution metering limits (optionally enforced, depend on metering service predictions)
    /// Execution metering limit exceeded.
    #[error("{0}")]
    ExecutionMeteringLimitExceeded(ExecutionMeteringLimitExceeded),

    // Transaction status
    /// Transaction is a sequencer transaction (skipped).
    #[error("sequencer transaction")]
    SequencerTransaction,

    /// Transaction nonce is too low.
    #[error("nonce too low")]
    NonceTooLow,

    /// Internal EVM error during transaction execution.
    #[error("internal error: {0}")]
    InternalError(BaseTransactionError),

    /// EVM execution error.
    #[error("EVM error")]
    EvmError,

    /// Transaction gas usage exceeds configured maximum.
    #[error("max gas usage exceeded")]
    MaxGasUsageExceeded,

    /// Metering data has not yet arrived for this transaction.
    #[error("metering data pending")]
    MeteringDataPending,
}

impl TxnExecutionError {
    /// Returns `true` if this rejection is permanent — the transaction will never be includable
    /// regardless of block/flashblock cumulative state. Permanent rejections are intrinsic to
    /// the transaction itself (e.g. its size or predicted execution time exceeds the per-tx limit).
    ///
    /// Transient rejections depend on cumulative block state (gas used, DA used, etc.) and may
    /// succeed in a future block or flashblock with different cumulative values.
    pub const fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::TransactionDASizeExceeded(_, _)
                | Self::ExecutionMeteringLimitExceeded(
                    ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _),
                )
                | Self::MaxGasUsageExceeded
        )
    }
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

/// Header commitments to the append-only executed transaction and receipt lists.
#[derive(Debug, Clone, Copy)]
pub struct BlockRoots {
    /// Number of transactions covered by the transaction root.
    pub transaction_count: usize,
    /// Number of receipts covered by the receipt root and bloom.
    pub receipt_count: usize,
    /// Historical Regolith receipt encoding omits the deposit nonce until Canyon.
    pub strip_deposit_nonce: bool,
    /// Ordered transaction trie root.
    pub transactions_root: B256,
    /// Ordered receipt trie root.
    pub receipts_root: B256,
    /// Combined receipt log bloom.
    pub logs_bloom: Bloom,
}

/// Execution information specific to flashblocks.
///
/// Tracks the last consumed flashblock index for progressive block construction.
#[derive(Debug, Default, Clone)]
pub struct FlashblocksExecutionInfo {
    /// Index of the last consumed flashblock
    pub(crate) last_flashblock_index: usize,
    /// Cached commitments for the current payload; invalidated when either list grows.
    pub block_roots: Option<BlockRoots>,
}

/// Accumulated execution state for the current block being built.
#[derive(Default, Debug)]
pub struct ExecutionInfo {
    /// All executed transactions (unrecovered).
    pub executed_transactions: Vec<BaseTransactionSigned>,
    /// The recovered senders for the executed transactions.
    pub executed_senders: Vec<Address>,
    /// The transaction receipts
    pub receipts: Vec<BaseReceipt>,
    /// All gas used so far
    pub cumulative_gas_used: u64,
    /// Estimated DA size
    pub cumulative_da_bytes_used: u64,
    /// Cumulative uncompressed (EIP-2718 encoded) bytes used in the block
    pub cumulative_uncompressed_bytes: u64,
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
    /// Extra execution information for the Flashblocks builder
    pub extra: FlashblocksExecutionInfo,
    /// DA Footprint Scalar for Jovian
    pub da_footprint_scalar: Option<u16>,
    /// Validity-predicate state loads accumulated across the block's flashblock builds.
    pub predicate_loads: PredicateLoadTracker,
    /// Validity inclusion and EIP-1559 fee revenue accumulated across the block.
    pub inclusion: InclusionTracker,
}

impl ExecutionInfo {
    /// Reuse header commitments while no transactions or receipts have been appended.
    ///
    /// Execution lists are append-only during a payload job. Callers replacing existing entries
    /// must clear `extra.block_roots`. Fork-dependent deposit encoding is part of the cache key.
    pub fn block_roots(&mut self, chain_spec: &BaseChainSpec, timestamp: u64) -> BlockRoots {
        let strip_deposit_nonce = chain_spec.is_regolith_active_at_timestamp(timestamp)
            && !chain_spec.is_canyon_active_at_timestamp(timestamp);
        if let Some(roots) = self.extra.block_roots
            && roots.transaction_count == self.executed_transactions.len()
            && roots.receipt_count == self.receipts.len()
            && roots.strip_deposit_nonce == strip_deposit_nonce
        {
            return roots;
        }
        let roots = BlockRoots {
            transaction_count: self.executed_transactions.len(),
            receipt_count: self.receipts.len(),
            strip_deposit_nonce,
            transactions_root: proofs::calculate_transaction_root(&self.executed_transactions),
            receipts_root: calculate_receipt_root_no_memo(&self.receipts, chain_spec, timestamp),
            logs_bloom: logs_bloom(self.receipts.iter().flat_map(|receipt| receipt.logs())),
        };
        self.extra.block_roots = Some(roots);
        roots
    }

    /// Create a new instance with allocated slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            executed_transactions: Vec::with_capacity(capacity),
            executed_senders: Vec::with_capacity(capacity),
            receipts: Vec::with_capacity(capacity),
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            cumulative_uncompressed_bytes: 0,
            total_fees: U256::ZERO,
            extra: Default::default(),
            da_footprint_scalar: None,
            predicate_loads: PredicateLoadTracker::default(),
            inclusion: InclusionTracker::default(),
        }
    }

    /// Returns true if the transaction would exceed the block limits:
    /// - block gas limit: ensures the transaction still fits into the block.
    /// - tx DA limit: if configured, ensures the tx does not exceed the maximum allowed DA limit
    ///   per tx.
    /// - block DA limit: if configured, ensures the transaction's DA size does not exceed the
    ///   maximum allowed DA limit per block.
    /// - execution time limit: if configured with metering data, ensures the transaction's
    ///   predicted execution time does not exceed the per-transaction limit.
    pub fn is_tx_over_limits(
        &self,
        tx: &TxResources,
        limits: &ResourceLimits,
    ) -> Result<(), TxnExecutionError> {
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

        // Check gas limit. EIP-8130 meters payer authentication gas on top of the declared
        // gas_limit, so it is reserved against the block gas budget in addition to gas_limit.
        if self.cumulative_gas_used.saturating_add(tx.gas_limit).saturating_add(tx.payer_auth)
            > limits.block_gas_limit
        {
            return Err(TxnExecutionError::TransactionGasLimitExceeded {
                cumulative_gas_used: self.cumulative_gas_used,
                tx_gas_limit: tx.gas_limit.saturating_add(tx.payer_auth),
                block_gas_limit: limits.block_gas_limit,
            });
        }

        // Check block uncompressed size limit
        if let Some(limit) = limits.block_uncompressed_size_limit {
            let total = self.cumulative_uncompressed_bytes.saturating_add(tx.uncompressed_size);
            if total > limit {
                return Err(TxnExecutionError::BlockUncompressedSizeExceeded {
                    total_uncompressed: self.cumulative_uncompressed_bytes,
                    tx_uncompressed_size: tx.uncompressed_size,
                    block_limit: limit,
                });
            }
        }

        // Check execution time limits (if metering data is available)
        if let Some(tx_time) = tx.execution_time_us {
            // Check per-transaction execution time limit
            if let Some(tx_limit) = limits.tx_execution_time_limit_us
                && tx_time > tx_limit
            {
                return Err(TransactionExecutionTime(tx_time, tx_limit).into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Receipt, TxEip1559};
    use alloy_primitives::{Bytes, Log};
    use base_common_consensus::{BaseTypedTransaction, DepositReceipt};
    use base_common_evm::BaseUpgrade;

    use super::*;
    use crate::test_utils::{builder_signer, sign_base_tx};

    #[test]
    fn block_roots_follow_appended_transactions_and_receipt_logs() {
        let chain_spec = BaseChainSpec::sepolia();
        let timestamp = u64::MAX;
        let mut info = ExecutionInfo::default();
        let empty = info.block_roots(&chain_spec, timestamp);
        for nonce in 0..2 {
            let transaction = sign_base_tx(
                &builder_signer(),
                BaseTypedTransaction::Eip1559(TxEip1559 { nonce, ..Default::default() }),
            )
            .unwrap()
            .into_inner();
            info.executed_transactions.push(transaction);
            info.receipts.push(BaseReceipt::Eip1559(Receipt {
                status: true.into(),
                cumulative_gas_used: (nonce + 1) * 21_000,
                logs: vec![Log::new_unchecked(
                    Address::with_last_byte(nonce as u8 + 1),
                    vec![],
                    Bytes::new(),
                )],
            }));
            let roots = info.block_roots(&chain_spec, timestamp);
            assert_eq!(
                roots.transactions_root,
                proofs::calculate_transaction_root(&info.executed_transactions)
            );
            assert_eq!(
                roots.receipts_root,
                calculate_receipt_root_no_memo(&info.receipts, &chain_spec, timestamp)
            );
            assert_eq!(
                roots.logs_bloom,
                logs_bloom(info.receipts.iter().flat_map(|receipt| receipt.logs()))
            );
            assert_ne!(roots.transactions_root, empty.transactions_root);
            assert_ne!(roots.logs_bloom, empty.logs_bloom);
            let unchanged = info.block_roots(&chain_spec, timestamp);
            assert_eq!(unchanged.transactions_root, roots.transactions_root);
            assert_eq!(unchanged.receipts_root, roots.receipts_root);
            assert_eq!(unchanged.logs_bloom, roots.logs_bloom);
        }
    }

    #[test]
    fn block_roots_preserve_regolith_and_canyon_deposit_encoding() {
        let mut chain_spec = BaseChainSpec::sepolia();
        chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Regolith, 10);
        chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Canyon, 20);
        let mut info = ExecutionInfo::default();
        info.receipts.push(BaseReceipt::Deposit(DepositReceipt {
            inner: Receipt { status: true.into(), cumulative_gas_used: 21_000, logs: vec![] },
            deposit_nonce: Some(5),
            deposit_receipt_version: None,
        }));
        let bedrock = info.block_roots(&chain_spec, 9).receipts_root;
        let regolith = info.block_roots(&chain_spec, 10).receipts_root;
        let canyon = info.block_roots(&chain_spec, 20).receipts_root;
        assert_ne!(bedrock, regolith);
        assert_eq!(bedrock, canyon);
        assert_eq!(regolith, calculate_receipt_root_no_memo(&info.receipts, &chain_spec, 10));
        assert_eq!(canyon, calculate_receipt_root_no_memo(&info.receipts, &chain_spec, 20));
    }

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

    #[test]
    fn test_gas_limit_reserves_payer_auth() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_gas_used = 29_979_000;

        let limits = default_limits();
        // gas_limit alone fits exactly (29_979_000 + 21_000 = 30_000_000), but the
        // EIP-8130 payer authentication is metered on top, pushing it over.
        let tx = TxResources { gas_limit: 21_000, payer_auth: 2_100, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(result, Err(TxnExecutionError::TransactionGasLimitExceeded { .. })));
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
    fn test_execution_time_within_limits() {
        let info = ExecutionInfo::with_capacity(10);

        let limits =
            ResourceLimits { tx_execution_time_limit_us: Some(1_000_000), ..default_limits() };
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
        let limits = ResourceLimits { tx_execution_time_limit_us: Some(1_000), ..default_limits() };
        // No execution_time_us set - should skip the check
        let tx = TxResources { gas_limit: 21_000, execution_time_us: None, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    // ==================== Combined Resource Tests ====================

    #[test]
    fn test_multiple_limits_first_exceeded_wins() {
        let info = ExecutionInfo::with_capacity(10);
        let limits = ResourceLimits {
            tx_data_limit: Some(100),
            tx_execution_time_limit_us: Some(1_000_000),
            ..default_limits()
        };

        // DA limit exceeded first (checked before execution time)
        let tx = TxResources {
            da_size: 200,
            gas_limit: 21_000,
            payer_auth: 0,
            execution_time_us: Some(2_000_000),
            uncompressed_size: 0,
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
            ..Default::default()
        };

        let tx = TxResources {
            da_size: 500,
            gas_limit: 100_000,
            payer_auth: 0,
            execution_time_us: Some(100_000),
            uncompressed_size: 0,
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
    fn test_with_capacity_initializes_correctly() {
        let info = ExecutionInfo::with_capacity(100);

        assert_eq!(info.cumulative_gas_used, 0);
        assert_eq!(info.cumulative_da_bytes_used, 0);
        assert_eq!(info.cumulative_uncompressed_bytes, 0);
        assert_eq!(info.total_fees, U256::ZERO);
        assert_eq!(info.inclusion, InclusionTracker::default());
        assert!(info.executed_transactions.is_empty());
        assert!(info.executed_senders.is_empty());
        assert!(info.receipts.is_empty());
    }

    #[test]
    fn test_block_uncompressed_size_exceeded() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_uncompressed_bytes = 90_000;

        let limits =
            ResourceLimits { block_uncompressed_size_limit: Some(100_000), ..default_limits() };
        let tx = TxResources { gas_limit: 21_000, uncompressed_size: 20_000, ..Default::default() };

        let result = info.is_tx_over_limits(&tx, &limits);
        assert!(matches!(
            result,
            Err(TxnExecutionError::BlockUncompressedSizeExceeded {
                total_uncompressed: 90_000,
                tx_uncompressed_size: 20_000,
                block_limit: 100_000,
            })
        ));
    }

    #[test]
    fn test_block_uncompressed_size_within_limits() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_uncompressed_bytes = 50_000;

        let limits =
            ResourceLimits { block_uncompressed_size_limit: Some(100_000), ..default_limits() };
        let tx = TxResources { gas_limit: 21_000, uncompressed_size: 40_000, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }

    #[test]
    fn test_block_uncompressed_size_none_means_no_limit() {
        let mut info = ExecutionInfo::with_capacity(10);
        info.cumulative_uncompressed_bytes = u64::MAX - 1;

        let limits = ResourceLimits { block_uncompressed_size_limit: None, ..default_limits() };
        let tx =
            TxResources { gas_limit: 21_000, uncompressed_size: 1_000_000, ..Default::default() };

        assert!(info.is_tx_over_limits(&tx, &limits).is_ok());
    }
}
