use core::fmt::Debug;
use std::{
    sync::Arc,
    time::{Instant, SystemTime},
};

use alloy_consensus::{Eip658Value, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::Database;
use alloy_primitives::{B256, BlockHash, Bytes, TxHash, U256};
use alloy_rpc_types_eth::Withdrawals;
use base_access_lists::FBALBuilderDb;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned, DepositReceipt, OpTxType};
use base_common_evm::{BaseReceiptBuilder, L1BlockInfo, OpSpecId};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, OpNextBlockEnvAttributes};
use base_execution_payload_builder::{OpPayloadBuilderAttributes, error::BasePayloadBuilderError};
use base_execution_txpool::{
    BundleTransaction, TimestampedTransaction, estimated_da_size::DataAvailabilitySized,
};
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    ConfigureEvm, Evm, EvmEnv, EvmError, InvalidTxError, eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_node_api::PayloadBuilderError;
use reth_payload_builder::PayloadId;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::SealedHeader;
use reth_primitives_traits::{InMemorySize, SignedTransaction};
use reth_revm::{State, context::Block};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction};
use revm::{DatabaseCommit, context::result::ResultAndState, interpreter::as_u64_saturated};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use crate::{
    BuilderConfig, BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, PayloadTxsBounds,
    ResourceLimits, TxResources, TxnExecutionError, TxnOutcome,
};

/// Records the priority fee of a rejected transaction with the given reason as a label.
fn record_rejected_tx_priority_fee(reason: &TxnExecutionError, priority_fee: f64) {
    let r = match reason {
        TxnExecutionError::TransactionDASizeExceeded(_, _) => "tx_da_size_exceeded",
        TxnExecutionError::BlockDASizeExceeded { .. } => "block_da_size_exceeded",
        TxnExecutionError::DAFootprintLimitExceeded { .. } => "da_footprint_limit_exceeded",
        TxnExecutionError::TransactionGasLimitExceeded { .. } => "transaction_gas_limit_exceeded",
        TxnExecutionError::BlockUncompressedSizeExceeded { .. } => {
            "block_uncompressed_size_exceeded"
        }
        TxnExecutionError::MeteringDataPending => "metering_data_pending",
        TxnExecutionError::ExecutionMeteringLimitExceeded(inner) => match inner {
            ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) => {
                "tx_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                "flashblock_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::BlockStateRootGas(_, _, _) => {
                "block_state_root_gas_exceeded"
            }
        },
        TxnExecutionError::SequencerTransaction => "sequencer_transaction",
        TxnExecutionError::NonceTooLow => "nonce_too_low",
        TxnExecutionError::InternalError(_) => "internal_error",
        TxnExecutionError::EvmError => "evm_error",
        TxnExecutionError::MaxGasUsageExceeded => "max_gas_usage_exceeded",
    };
    BuilderMetrics::rejected_tx_priority_fee(r).record(priority_fee);
}

/// Diagnostics captured during a single flashblock's transaction execution.
///
/// Tracks how transaction selection ended, what limits were hit, and the
/// priority fee threshold among included transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashblockSelectionOutcome {
    /// Transaction selection stopped because the flashblock build was cancelled.
    Cancelled,
    /// Transaction selection stopped because no pool transaction was considered.
    PoolEmpty,
    /// Transaction selection stopped after draining the candidate pool.
    PoolDrained,
}

impl FlashblockSelectionOutcome {
    /// Returns the label used for logs and metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::PoolEmpty => "pool_empty",
            Self::PoolDrained => "pool_drained",
        }
    }
}

/// Per-flashblock diagnostics summarizing transaction selection outcomes.
#[derive(Debug, Default)]
pub struct FlashblockDiagnostics {
    /// Whether the flashblock timer or block cancel fired during execution.
    pub cancelled: bool,
    /// Number of transactions considered from the pool.
    pub txs_considered: u64,
    /// Number of transactions included in the flashblock.
    pub txs_included: u64,
    /// Number rejected by gas limit.
    pub txs_rejected_gas: u64,
    /// Number rejected by DA size limits (tx or block).
    pub txs_rejected_da: u64,
    /// Number rejected by DA footprint limit.
    pub txs_rejected_da_footprint: u64,
    /// Number rejected by execution time limits (tx or flashblock).
    pub txs_rejected_execution_time: u64,
    /// Number rejected by state root time limits (tx or block).
    pub txs_rejected_state_root_time: u64,
    /// Number rejected by uncompressed size limit.
    pub txs_rejected_uncompressed_size: u64,
    /// Number skipped because metering data has not yet arrived.
    pub txs_rejected_metering_data_pending: u64,
    /// Number rejected or skipped for other reasons.
    pub txs_rejected_other: u64,
    /// Minimum effective priority fee (tip per gas) among included transactions.
    pub min_priority_fee: Option<u64>,
    /// Transaction hashes permanently rejected due to per-tx intrinsic limits
    /// (e.g. tx DA size exceeded, tx execution time exceeded). These will never
    /// be includable and should be evicted from the pool.
    pub permanently_rejected_txs: Vec<TxHash>,
}

impl FlashblockDiagnostics {
    /// Returns how transaction selection ended for this flashblock.
    pub const fn selection_outcome(&self) -> FlashblockSelectionOutcome {
        if self.cancelled {
            FlashblockSelectionOutcome::Cancelled
        } else if self.txs_considered == 0 {
            FlashblockSelectionOutcome::PoolEmpty
        } else {
            FlashblockSelectionOutcome::PoolDrained
        }
    }

    /// Returns the rejection counts keyed by their metric/log reason labels.
    pub const fn rejection_counts(&self) -> [(&'static str, u64); 8] {
        [
            ("gas_limit", self.txs_rejected_gas),
            ("da_size", self.txs_rejected_da),
            ("da_footprint", self.txs_rejected_da_footprint),
            ("execution_time", self.txs_rejected_execution_time),
            ("state_root_time", self.txs_rejected_state_root_time),
            ("uncompressed_size", self.txs_rejected_uncompressed_size),
            ("metering_data_pending", self.txs_rejected_metering_data_pending),
            ("other", self.txs_rejected_other),
        ]
    }

    /// Returns the distinct rejection categories encountered while scanning the pool.
    pub fn rejection_reasons(&self) -> Vec<&'static str> {
        self.rejection_counts()
            .into_iter()
            .filter_map(|(reason, count)| (count > 0).then_some(reason))
            .collect()
    }

    /// Total number of rejected or skipped transactions across all tracked categories.
    pub const fn txs_rejected_total(&self) -> u64 {
        self.txs_rejected_gas
            + self.txs_rejected_da
            + self.txs_rejected_da_footprint
            + self.txs_rejected_execution_time
            + self.txs_rejected_state_root_time
            + self.txs_rejected_uncompressed_size
            + self.txs_rejected_metering_data_pending
            + self.txs_rejected_other
    }

    /// Records a rejected transaction into the appropriate rejection bucket.
    pub const fn record_rejection(&mut self, err: &TxnExecutionError) {
        match err {
            TxnExecutionError::TransactionGasLimitExceeded { .. } => {
                self.txs_rejected_gas += 1;
            }
            TxnExecutionError::TransactionDASizeExceeded(_, _)
            | TxnExecutionError::BlockDASizeExceeded { .. } => {
                self.txs_rejected_da += 1;
            }
            TxnExecutionError::DAFootprintLimitExceeded { .. } => {
                self.txs_rejected_da_footprint += 1;
            }
            TxnExecutionError::BlockUncompressedSizeExceeded { .. } => {
                self.txs_rejected_uncompressed_size += 1;
            }
            TxnExecutionError::ExecutionMeteringLimitExceeded(inner) => match inner {
                ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _)
                | ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                    self.txs_rejected_execution_time += 1;
                }
                ExecutionMeteringLimitExceeded::BlockStateRootGas(_, _, _) => {
                    self.txs_rejected_state_root_time += 1;
                }
            },
            TxnExecutionError::MeteringDataPending => {
                self.txs_rejected_metering_data_pending += 1;
            }
            TxnExecutionError::SequencerTransaction
            | TxnExecutionError::NonceTooLow
            | TxnExecutionError::InternalError(_)
            | TxnExecutionError::EvmError
            | TxnExecutionError::MaxGasUsageExceeded => {
                self.txs_rejected_other += 1;
            }
        }
    }
}

/// Extra context for flashblock payload building.
///
/// Contains flashblock-specific configuration and state for tracking
/// gas and data availability limits across flashblock batches.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlashblocksExtraCtx {
    /// Current flashblock index
    pub flashblock_index: u64,
    /// Target flashblock count per block
    pub target_flashblock_count: u64,
    /// Total gas left for the current flashblock
    pub target_gas_for_batch: u64,
    /// Total DA bytes left for the current flashblock
    pub target_da_for_batch: Option<u64>,
    /// Total DA footprint left for the current flashblock
    pub target_da_footprint_for_batch: Option<u64>,
    /// Target execution time for the current flashblock in microseconds
    pub target_execution_time_for_batch_us: Option<u128>,
    /// Target state root gas for the current flashblock
    pub target_state_root_gas_for_batch: Option<u64>,
    /// Gas limit per flashblock
    pub gas_per_batch: u64,
    /// DA bytes limit per flashblock
    pub da_per_batch: Option<u64>,
    /// DA footprint limit per flashblock
    pub da_footprint_per_batch: Option<u64>,
    /// Execution time limit per flashblock in microseconds
    pub execution_time_per_batch_us: Option<u128>,
    /// State root gas limit per flashblock
    pub state_root_gas_per_batch: Option<u64>,
}

impl FlashblocksExtraCtx {
    /// Creates the next flashblock context with updated gas and DA targets.
    ///
    /// Increments the flashblock index and sets new target limits for the
    /// next flashblock batch iteration.
    pub const fn next(
        self,
        target_gas_for_batch: u64,
        target_da_for_batch: Option<u64>,
        target_da_footprint_for_batch: Option<u64>,
        target_execution_time_for_batch_us: Option<u128>,
        target_state_root_gas_for_batch: Option<u64>,
    ) -> Self {
        Self {
            flashblock_index: self.flashblock_index + 1,
            target_gas_for_batch,
            target_da_for_batch,
            target_da_footprint_for_batch,
            target_execution_time_for_batch_us,
            target_state_root_gas_for_batch,
            ..self
        }
    }
}

/// Container type that holds all the necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: BaseEvmConfig,
    /// The chainspec
    pub chain_spec: Arc<BaseChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<BaseTransactionSigned>>,
    /// Evm Settings
    pub evm_env: EvmEnv<OpSpecId>,
    /// Block env attributes for the current block.
    pub block_env_attributes: OpNextBlockEnvAttributes,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// Extra context for the payload builder
    pub extra: FlashblocksExtraCtx,
    /// Builder configuration containing limits and metering settings.
    pub builder_config: BuilderConfig,
}

impl OpPayloadBuilderCtx {
    pub(super) fn with_cancel(self, cancel: CancellationToken) -> Self {
        Self { cancel, ..self }
    }

    pub(super) fn with_extra_ctx(self, extra: FlashblocksExtraCtx) -> Self {
        Self { extra, ..self }
    }

    pub(crate) const fn flashblock_index(&self) -> u64 {
        self.extra.flashblock_index
    }

    pub(crate) const fn target_flashblock_count(&self) -> u64 {
        self.extra.target_flashblock_count
    }

    /// Returns the parent block the payload will be built on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the parent hash
    pub fn parent_hash(&self) -> BlockHash {
        self.parent().hash()
    }

    /// Returns the timestamp
    pub fn timestamp(&self) -> u64 {
        self.attributes().timestamp()
    }

    /// Returns the builder attributes.
    pub(super) const fn attributes(&self) -> &OpPayloadBuilderAttributes<BaseTransactionSigned> {
        &self.config.attributes
    }

    /// Returns the withdrawals if shanghai is active.
    pub fn withdrawals(&self) -> Option<&Withdrawals> {
        self.chain_spec
            .is_shanghai_active_at_timestamp(self.attributes().timestamp())
            .then(|| &self.attributes().payload_attributes.withdrawals)
    }

    /// Returns the block gas limit to target.
    pub fn block_gas_limit(&self) -> u64 {
        self.builder_config.gas_limit_config.gas_limit().unwrap_or_else(|| {
            self.attributes().gas_limit.unwrap_or(self.evm_env.block_env.gas_limit)
        })
    }

    /// Returns the block number for the block.
    pub const fn block_number(&self) -> u64 {
        as_u64_saturated!(self.evm_env.block_env.number)
    }

    /// Returns the current base fee
    pub const fn base_fee(&self) -> u64 {
        self.evm_env.block_env.basefee
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.evm_env.block_env.blob_gasprice().map(|gasprice| gasprice as u64)
    }

    /// Returns the blob fields for the header.
    ///
    /// This will return the cumulative DA bytes * scalar after Jovian
    /// after Ecotone, this will always return Some(0) as blobs aren't supported
    /// pre Ecotone, these fields aren't used.
    pub fn blob_fields(&self, info: &ExecutionInfo) -> (Option<u64>, Option<u64>) {
        if self.is_jovian_active() {
            let scalar =
                info.da_footprint_scalar.expect("Scalar must be defined for Jovian blocks");
            let result = info.cumulative_da_bytes_used * scalar as u64;
            (Some(0), Some(result))
        } else if self.is_ecotone_active() {
            (Some(0), Some(0))
        } else {
            (None, None)
        }
    }

    /// Returns the extra data for the block.
    ///
    /// After holocene this extracts the extradata from the payload
    pub fn extra_data(&self) -> Result<Bytes, PayloadBuilderError> {
        if self.is_jovian_active() {
            self.attributes()
                .get_jovian_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else if self.is_holocene_active() {
            self.attributes()
                .get_holocene_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else {
            Ok(Default::default())
        }
    }

    /// Returns the current fee settings for transactions from the mempool
    pub fn best_transaction_attributes(&self) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(self.base_fee(), self.get_blob_gasprice())
    }

    /// Returns the unique id for this payload job.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if regolith is active for the payload.
    pub fn is_regolith_active(&self) -> bool {
        self.chain_spec.is_regolith_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if ecotone is active for the payload.
    pub fn is_ecotone_active(&self) -> bool {
        self.chain_spec.is_ecotone_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if canyon is active for the payload.
    pub fn is_canyon_active(&self) -> bool {
        self.chain_spec.is_canyon_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec.is_holocene_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if isthmus is active for the payload.
    pub fn is_isthmus_active(&self) -> bool {
        self.chain_spec.is_isthmus_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if jovian is active for the payload.
    pub fn is_jovian_active(&self) -> bool {
        self.chain_spec.is_jovian_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain_id()
    }
}

impl OpPayloadBuilderCtx {
    /// Constructs a receipt for the given transaction.
    pub fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, OpTxType, E>,
        deposit_nonce: Option<u64>,
    ) -> BaseReceipt {
        let receipt_builder = self.evm_config.block_executor_factory().receipt_builder();
        match receipt_builder.build_receipt(ctx) {
            Ok(receipt) => receipt,
            Err(ctx) => {
                let receipt = alloy_consensus::Receipt {
                    // Success flag was added in `EIP-658: Embedding transaction status code
                    // in receipts`.
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                receipt_builder.build_deposit_receipt(DepositReceipt {
                    inner: receipt,
                    deposit_nonce,
                    // The deposit receipt version was introduced in Canyon to indicate an
                    // update to how receipt hashes should be computed
                    // when set. The state transition process ensures
                    // this is only set for post-Canyon deposit
                    // transactions.
                    deposit_receipt_version: self.is_canyon_active().then_some(1),
                })
            }
        }
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    pub(super) fn execute_sequencer_transactions(
        &self,
        db: &mut State<impl Database>,
    ) -> Result<ExecutionInfo, PayloadBuilderError> {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());

        let mut fbal_db = FBALBuilderDb::new(&mut *db);
        let min_tx_index = info.executed_transactions.iter().len() as u64;
        fbal_db.set_index(min_tx_index);
        let mut evm = self.evm_config.evm_with_env(&mut fbal_db, self.evm_env.clone());

        for sequencer_tx in &self.attributes().transactions {
            // A sequencer's block should never contain blob transactions.
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    BasePayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Convert the transaction to a [Recovered<TransactionSigned>]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx.value().try_clone_into_recovered().map_err(|_| {
                PayloadBuilderError::other(BasePayloadBuilderError::TransactionEcRecoverFailed)
            })?;

            // Cache the depositor account prior to the state transition for the deposit nonce.
            //
            // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
            // were not introduced in Bedrock. In addition, regular transactions don't have deposit
            // nonces, so we don't need to touch the DB for those.
            let depositor_nonce = (self.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .db_mut()
                        .load_cache_account(sequencer_tx.signer())
                        .map(|acc| acc.account_info().unwrap_or_default().nonce)
                })
                .transpose()
                .map_err(|_| {
                    PayloadBuilderError::other(BasePayloadBuilderError::AccountLoadFailed(
                        sequencer_tx.signer(),
                    ))
                })?;

            let ResultAndState { result, state } = match evm.transact(&sequencer_tx) {
                Ok(res) => res,
                Err(err) => {
                    if err.is_invalid_tx_err() {
                        trace!(target: "payload_builder", %err, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            let gas_used = result.gas_used();
            info.cumulative_gas_used += gas_used;

            if !sequencer_tx.is_deposit() {
                info.cumulative_da_bytes_used += base_common_flz::tx_estimated_size_fjord_bytes(
                    sequencer_tx.encoded_2718().as_slice(),
                );
                info.cumulative_uncompressed_bytes += sequencer_tx.encode_2718_len() as u64;
            }

            let ctx = ReceiptBuilderCtx {
                tx_type: sequencer_tx.tx_type(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };

            info.receipts.push(self.build_receipt(ctx, depositor_nonce));

            // commit changes
            evm.db_mut().commit(state);

            // append sender and transaction to the respective lists
            // and increment the next txn index for the access list
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_inner());
            evm.db_mut().inc_index();
        }

        let da_footprint_gas_scalar = self
            .chain_spec
            .is_jovian_active_at_timestamp(self.attributes().timestamp())
            .then(|| {
                L1BlockInfo::fetch_da_footprint_gas_scalar(evm.db_mut())
                    .expect("DA footprint should always be available from the database post jovian")
            });

        info.da_footprint_scalar = da_footprint_gas_scalar;

        match fbal_db.finish() {
            Ok(fbal_builder) => info.extra.access_list_builder = fbal_builder,
            Err(err) => {
                error!(error = %err, "Failed to finalize FBALBuilder");
            }
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns diagnostics summarizing transaction selection for the flashblock.
    pub(super) fn execute_best_transactions(
        &self,
        info: &mut ExecutionInfo,
        db: &mut State<impl Database>,
        best_txs: &mut impl PayloadTxsBounds,
        limits: &ResourceLimits,
    ) -> Result<FlashblockDiagnostics, PayloadBuilderError> {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;
        let mut reverted_gas_used: u64 = 0;
        let base_fee = self.base_fee();
        let mut diag = FlashblockDiagnostics::default();

        let mut fbal_db = FBALBuilderDb::new(&mut *db);
        let min_tx_index = info.executed_transactions.len() as u64;
        fbal_db.set_index(min_tx_index);
        let mut evm = self.evm_config.evm_with_env(&mut fbal_db, self.evm_env.clone());

        debug!(
            target: "payload_builder",
            message = "Executing best transactions",
            block_data_limit = ?limits.block_data_limit,
            tx_data_limit = ?limits.tx_data_limit,
            block_gas_limit = ?limits.block_gas_limit,
            flashblock_execution_time_limit_us = ?limits.flashblock_execution_time_limit_us,
            block_state_root_gas_limit = ?limits.block_state_root_gas_limit,
            execution_metering_mode = ?self.builder_config.execution_metering_mode,
        );

        let block_number = as_u64_saturated!(self.evm_env.block_env.number);
        let block_timestamp = self.attributes().timestamp();

        while let Some(tx) = best_txs.next(()) {
            if let Some(target) = tx.target_block_number()
                && target != block_number
            {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.hash(),
                    target_block = target,
                    current_block = block_number,
                    "skipping bundle tx: wrong target block"
                );
                best_txs.mark_invalid(tx.sender(), tx.nonce());
                continue;
            }

            if tx.is_bundle_expired(block_number, block_timestamp) {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.hash(),
                    block = block_number,
                    timestamp = block_timestamp,
                    "skipping bundle tx: expired"
                );
                best_txs.mark_invalid(tx.sender(), tx.nonce());
                continue;
            }

            if tx.is_bundle_not_yet_valid(block_timestamp) {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.hash(),
                    block = block_number,
                    timestamp = block_timestamp,
                    "skipping bundle tx: not yet valid"
                );
                best_txs.mark_invalid(tx.sender(), tx.nonce());
                continue;
            }

            let tx_da_size = tx.estimated_da_size();
            let tx_received_at_ms = tx.received_at();
            let tx = tx.into_consensus();
            let tx_hash = tx.tx_hash();
            let tx_uncompressed_size = tx.encode_2718_len() as u64;

            let log_txn = |result: Result<TxnOutcome, TxnExecutionError>| {
                let result_str = match &result {
                    Ok(outcome) => outcome.to_string(),
                    Err(err) => err.to_string(),
                };
                debug!(
                    target: "payload_builder",
                    message = "Considering transaction",
                    tx_hash = ?tx_hash,
                    tx_da_size = ?tx_da_size,
                    result = %result_str,
                );
            };

            num_txs_considered += 1;

            let resource_usage = self.builder_config.metering_provider.get(&tx_hash);

            // Skip transactions that are too young and don't have metering data yet
            if self.builder_config.metering_provider.is_enabled()
                && resource_usage.is_none()
                && let Some(wait_duration) = self.builder_config.metering_wait_duration
            {
                let now_ms = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let tx_age_ms = now_ms.saturating_sub(tx_received_at_ms);
                if tx_age_ms < wait_duration.as_millis() {
                    log_txn(Err(TxnExecutionError::MeteringDataPending));
                    BuilderMetrics::metering_data_pending_skip().increment(1);
                    self.builder_config.metering_provider.skip(&tx_hash);
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
            }

            // Extract predicted execution time from metering data
            let predicted_execution_time_us =
                resource_usage.as_ref().map(|m| m.total_execution_time_us);
            let predicted_state_root_time_us =
                resource_usage.as_ref().map(|m| m.state_root_time_us);

            // Compute state root gas from metering data:
            // sr_gas = gas_used × (1 + K × max(0, SR_ms - anchor_ms))
            let state_root_gas = resource_usage.as_ref().map(|m| {
                let gas_used = m.total_gas_used;
                let sr_us = m.state_root_time_us;
                let anchor_us = self.builder_config.state_root_gas_anchor_us;
                let k = self.builder_config.state_root_gas_coefficient;
                let excess_us = sr_us.saturating_sub(anchor_us);
                let excess_ms = excess_us as f64 / 1000.0;
                let multiplier = 1.0 + k * excess_ms;
                (gas_used as f64 * multiplier) as u64
            });

            // Build tx resources struct
            let tx_resources = TxResources {
                da_size: tx_da_size,
                gas_limit: tx.gas_limit(),
                execution_time_us: predicted_execution_time_us,
                state_root_gas,
                uncompressed_size: tx_uncompressed_size,
            };

            // ensure we still have capacity for this transaction
            if let Err(err) = info.is_tx_over_limits(&tx_resources, limits) {
                // Check if this is an execution metering limit that should be handled
                // according to the metering mode (dry-run vs enforce)
                if let TxnExecutionError::ExecutionMeteringLimitExceeded(ref limit_err) = err {
                    // Record metrics for the exceeded limit
                    self.record_execution_metering_limit_exceeded(limit_err);

                    let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                    let dry_run = self.builder_config.execution_metering_mode.is_dry_run();

                    warn!(
                        target: "payload_builder",
                        message = if dry_run {
                            "Metering throttle: transaction would be rejected (dry-run)"
                        } else {
                            "Metering throttle: transaction rejected"
                        },
                        tx_hash = ?tx_hash,
                        limit = %limit_err,
                        priority_fee,
                        dry_run,
                    );

                    if !dry_run {
                        diag.record_rejection(&err);
                        record_rejected_tx_priority_fee(&err, priority_fee);
                        if err.is_permanent() {
                            diag.permanently_rejected_txs.push(tx_hash);
                        }
                        log_txn(Err(err));
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        continue;
                    }
                } else {
                    // DA size limits, DA footprint, and gas limits are always enforced
                    diag.record_rejection(&err);
                    self.record_static_limit_exceeded(&err);

                    let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                    record_rejected_tx_priority_fee(&err, priority_fee);

                    if err.is_permanent() {
                        diag.permanently_rejected_txs.push(tx_hash);
                    }
                    log_txn(Err(err));
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
            }

            // Record execution time prediction accuracy metrics
            if let Some(predicted_us) = predicted_execution_time_us {
                BuilderMetrics::tx_predicted_execution_time_us().record(predicted_us as f64);
            }
            if let Some(predicted_us) = predicted_state_root_time_us {
                BuilderMetrics::tx_predicted_state_root_time_us().record(predicted_us as f64);
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                let err = TxnExecutionError::SequencerTransaction;
                diag.record_rejection(&err);
                let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                record_rejected_tx_priority_fee(&err, priority_fee);
                log_txn(Err(err));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                diag.cancelled = true;
                diag.txs_considered = num_txs_considered;
                diag.txs_included =
                    (info.executed_transactions.len() as u64).saturating_sub(min_tx_index);
                return Ok(diag);
            }

            let tx_simulation_start_time = Instant::now();
            let ResultAndState { result, state } = match evm.transact(&tx) {
                Ok(res) => res,
                Err(err) => {
                    if let Some(err) = err.as_invalid_tx_err() {
                        if err.is_nonce_too_low() {
                            // if the nonce is too low, we can skip this transaction
                            let diag_err = TxnExecutionError::NonceTooLow;
                            diag.record_rejection(&diag_err);
                            let priority_fee =
                                tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                            record_rejected_tx_priority_fee(&diag_err, priority_fee);
                            log_txn(Err(diag_err));
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            let diag_err = TxnExecutionError::InternalError(err.clone());
                            diag.record_rejection(&diag_err);
                            let priority_fee =
                                tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                            record_rejected_tx_priority_fee(&diag_err, priority_fee);
                            log_txn(Err(diag_err));
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    log_txn(Err(TxnExecutionError::EvmError));
                    return Err(PayloadBuilderError::evm(err));
                }
            };

            let actual_execution_time_us = tx_simulation_start_time.elapsed().as_micros();

            BuilderMetrics::tx_simulation_duration().record(tx_simulation_start_time.elapsed());
            BuilderMetrics::tx_byte_size().record(tx.inner().size() as f64);
            BuilderMetrics::tx_actual_execution_time_us().record(actual_execution_time_us as f64);
            num_txs_simulated += 1;

            // Record state modification counts (trie work proxy)
            let accounts_modified = state.len();
            let storage_slots_modified: usize = state.values().map(|a| a.storage.len()).sum();
            BuilderMetrics::tx_accounts_modified().record(accounts_modified as f64);
            BuilderMetrics::tx_storage_slots_modified().record(storage_slots_modified as f64);

            // Record execution time for unmetered transactions (race condition indicator)
            if resource_usage.is_none() {
                BuilderMetrics::unmetered_tx_actual_execution_time_us()
                    .record(actual_execution_time_us as f64);
            }

            // Record prediction accuracy
            if let Some(predicted_us) = predicted_execution_time_us {
                let error = predicted_us as f64 - actual_execution_time_us as f64;
                BuilderMetrics::execution_time_prediction_error_us().record(error);
            }

            let gas_used = result.gas_used();
            let is_success = result.is_success();
            if is_success {
                log_txn(Ok(TxnOutcome::Success));
                num_txs_simulated_success += 1;
                BuilderMetrics::successful_tx_gas_used().record(gas_used as f64);
            } else {
                log_txn(Ok(TxnOutcome::Reverted));
                num_txs_simulated_fail += 1;
                reverted_gas_used += gas_used;
                BuilderMetrics::reverted_tx_gas_used().record(gas_used as f64);
            }

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            if let Some(max_gas_per_txn) = self.builder_config.max_gas_per_txn
                && gas_used > max_gas_per_txn
            {
                let err = TxnExecutionError::MaxGasUsageExceeded;
                diag.record_rejection(&err);
                let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                record_rejected_tx_priority_fee(&err, priority_fee);
                log_txn(Err(err));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            info.cumulative_gas_used += gas_used;
            // record tx da size
            info.cumulative_da_bytes_used += tx_da_size;
            // record uncompressed tx size
            info.cumulative_uncompressed_bytes += tx_uncompressed_size;
            // record execution time (only from predictions; unmetered txs count as zero)
            if let Some(execution_time) = predicted_execution_time_us {
                info.flashblock_execution_time_us += execution_time;
            }
            // record state root gas (only from predictions)
            if let Some(sr_gas) = state_root_gas {
                info.cumulative_state_root_gas += sr_gas;
                BuilderMetrics::tx_state_root_gas().record(sr_gas as f64);
            }
            // record state root time / gas ratio for anomaly detection
            if let Some(state_root_time) = predicted_state_root_time_us
                && gas_used > 0
            {
                let ratio = state_root_time as f64 / gas_used as f64;
                BuilderMetrics::state_root_time_per_gas_ratio().record(ratio);
            }

            // Push transaction changeset and calculate header bloom filter for receipt.
            let ctx = ReceiptBuilderCtx {
                tx_type: tx.tx_type(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };
            info.receipts.push(self.build_receipt(ctx, None));

            // commit changes
            evm.db_mut().commit(state);

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            // track minimum priority fee for diagnostics (saturate u128 -> u64)
            let fee_u64 = miner_fee.min(u64::MAX as u128) as u64;
            diag.min_priority_fee = Some(diag.min_priority_fee.map_or(fee_u64, |m| m.min(fee_u64)));

            // Record metering hit/miss only for committed transactions so the
            // metric reflects actual payload inclusion, not speculative lookups.
            if self.builder_config.metering_provider.is_enabled() && resource_usage.is_some() {
                BuilderMetrics::metering_known_transaction().increment(1);
            } else {
                BuilderMetrics::metering_unknown_transaction().increment(1);
            }

            // append sender and transaction to the respective lists
            // and increment the next txn index for the access list
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
            evm.db_mut().inc_index();
        }

        match fbal_db.finish() {
            Ok(fbal_builder) => {
                info.extra.access_list_builder.merge(fbal_builder);
            }
            Err(err) => {
                error!(error = %err, "Failed to finalize FBALBuilder");
            }
        }

        // Record cumulative state root gas for the block
        if info.cumulative_state_root_gas > 0 {
            BuilderMetrics::block_state_root_gas().record(info.cumulative_state_root_gas as f64);
        }

        let payload_transaction_simulation_time = execute_txs_start_time.elapsed();
        BuilderMetrics::set_payload_builder_metrics(
            payload_transaction_simulation_time.as_secs_f64(),
            num_txs_considered as f64,
            num_txs_simulated as f64,
            num_txs_simulated_success as f64,
            num_txs_simulated_fail as f64,
            reverted_gas_used as f64,
        );

        diag.txs_considered = num_txs_considered;
        diag.txs_included = (info.executed_transactions.len() as u64).saturating_sub(min_tx_index);

        debug!(
            target: "payload_builder",
            message = "Completed executing best transactions",
            txs_executed = num_txs_considered,
            txs_applied = num_txs_simulated_success,
            txs_rejected = num_txs_simulated_fail,
        );
        Ok(diag)
    }

    /// Record metrics for a limit that can be evaluated via static analysis (always enforced).
    fn record_static_limit_exceeded(&self, err: &TxnExecutionError) {
        match err {
            TxnExecutionError::TransactionDASizeExceeded(_, _) => {
                BuilderMetrics::tx_da_size_exceeded_total().increment(1);
            }
            TxnExecutionError::BlockDASizeExceeded { .. } => {
                BuilderMetrics::block_da_size_exceeded_total().increment(1);
            }
            TxnExecutionError::DAFootprintLimitExceeded { .. } => {
                BuilderMetrics::da_footprint_exceeded_total().increment(1);
            }
            TxnExecutionError::TransactionGasLimitExceeded { .. } => {
                BuilderMetrics::gas_limit_exceeded_total().increment(1);
            }
            TxnExecutionError::BlockUncompressedSizeExceeded { .. } => {
                BuilderMetrics::block_uncompressed_size_exceeded_total().increment(1);
            }
            _ => {}
        }
    }

    /// Record metrics for a limit that requires execution data (enforcement is configurable).
    fn record_execution_metering_limit_exceeded(&self, limit: &ExecutionMeteringLimitExceeded) {
        BuilderMetrics::resource_limit_would_reject_total().increment(1);
        match limit {
            ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) => {
                BuilderMetrics::tx_execution_time_exceeded_total().increment(1);
            }
            ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                BuilderMetrics::flashblock_execution_time_exceeded_total().increment(1);
            }
            ExecutionMeteringLimitExceeded::BlockStateRootGas(_, _, _) => {
                BuilderMetrics::block_state_root_gas_exceeded_total().increment(1);
            }
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl OpPayloadBuilderCtx {
    /// Creates a minimal [`OpPayloadBuilderCtx`] for unit tests.
    ///
    /// Derives the EVM environment from the given chain spec and parent header,
    /// using default builder attributes and a no-op cancellation token.
    pub fn for_test(chain_spec: Arc<BaseChainSpec>, parent: Arc<SealedHeader>) -> Self {
        let evm_config = BaseEvmConfig::optimism(Arc::clone(&chain_spec));
        let timestamp = parent.timestamp + 2;

        let attributes = OpPayloadBuilderAttributes {
            payload_attributes: reth_payload_builder::EthPayloadBuilderAttributes {
                id: PayloadId::new([0; 8]),
                parent: parent.hash(),
                timestamp,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            gas_limit: Some(parent.gas_limit),
            ..Default::default()
        };

        let block_env_attributes = OpNextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: Default::default(),
            prev_randao: Default::default(),
            gas_limit: parent.gas_limit,
            parent_beacon_block_root: Some(B256::ZERO),
            extra_data: Default::default(),
        };

        let evm_env = evm_config
            .next_evm_env(&parent, &block_env_attributes)
            .expect("failed to create test evm env");

        let config = PayloadConfig::new(parent, attributes);

        Self {
            evm_config,
            chain_spec,
            config,
            evm_env,
            block_env_attributes,
            cancel: CancellationToken::new(),
            extra: FlashblocksExtraCtx::default(),
            builder_config: crate::BuilderConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_report_selection_outcome() {
        let diag = FlashblockDiagnostics::default();
        assert_eq!(diag.selection_outcome(), FlashblockSelectionOutcome::PoolEmpty);
        assert_eq!(diag.selection_outcome().as_str(), "pool_empty");

        let diag =
            FlashblockDiagnostics { txs_considered: 3, txs_included: 1, ..Default::default() };
        assert_eq!(diag.selection_outcome(), FlashblockSelectionOutcome::PoolDrained);
        assert_eq!(diag.selection_outcome().as_str(), "pool_drained");

        let diag = FlashblockDiagnostics { cancelled: true, ..Default::default() };
        assert_eq!(diag.selection_outcome(), FlashblockSelectionOutcome::Cancelled);
        assert_eq!(diag.selection_outcome().as_str(), "cancelled");
    }

    #[test]
    fn diagnostics_report_distinct_rejection_reasons() {
        let mut diag = FlashblockDiagnostics::default();
        diag.txs_rejected_gas += 1;
        diag.txs_rejected_da += 2;

        assert_eq!(diag.rejection_reasons(), vec!["gas_limit", "da_size"]);
        assert_eq!(diag.txs_rejected_total(), 3);
    }

    #[test]
    fn diagnostics_report_rejection_counts() {
        let diag = FlashblockDiagnostics {
            txs_rejected_gas: 2,
            txs_rejected_state_root_time: 1,
            ..Default::default()
        };

        assert_eq!(
            diag.rejection_counts(),
            [
                ("gas_limit", 2),
                ("da_size", 0),
                ("da_footprint", 0),
                ("execution_time", 0),
                ("state_root_time", 1),
                ("uncompressed_size", 0),
                ("metering_data_pending", 0),
                ("other", 0),
            ]
        );
    }

    #[test]
    fn diagnostics_bucket_other_rejections() {
        let mut diag = FlashblockDiagnostics::default();
        diag.record_rejection(&TxnExecutionError::SequencerTransaction);
        diag.record_rejection(&TxnExecutionError::NonceTooLow);
        diag.record_rejection(&TxnExecutionError::MaxGasUsageExceeded);
        diag.record_rejection(&TxnExecutionError::MeteringDataPending);

        assert_eq!(diag.txs_rejected_metering_data_pending, 1);
        assert_eq!(diag.txs_rejected_other, 3);
        assert_eq!(diag.txs_rejected_total(), 4);
    }

    #[test]
    fn diagnostics_count_included_transactions_from_appended_txs() {
        let diag =
            FlashblockDiagnostics { txs_considered: 5, txs_included: 2, ..Default::default() };

        assert_eq!(diag.txs_considered, 5);
        assert_eq!(diag.txs_included, 2);
    }

    /// [`FlashblocksExtraCtx::next`] must increment the flashblock index,
    /// update all per-batch target fields to the new values, and preserve
    /// the per-batch *limit* fields and the target flashblock count.
    #[test]
    fn extra_ctx_next_advances_index_and_updates_targets() {
        let ctx = FlashblocksExtraCtx {
            flashblock_index: 2,
            target_flashblock_count: 10,
            target_gas_for_batch: 1_000_000,
            target_da_for_batch: Some(500),
            target_da_footprint_for_batch: Some(200),
            target_execution_time_for_batch_us: Some(100_000),
            target_state_root_gas_for_batch: Some(50_000),
            gas_per_batch: 3_000_000,
            da_per_batch: Some(1_500),
            da_footprint_per_batch: Some(600),
            execution_time_per_batch_us: Some(300_000),
            state_root_gas_per_batch: Some(150_000),
        };

        let next = ctx.next(
            2_000_000,     // new gas target
            Some(800),     // new DA target
            Some(350),     // new DA footprint target
            Some(200_000), // new execution time target
            Some(80_000),  // new state root gas target
        );

        // Index incremented
        assert_eq!(next.flashblock_index, 3);

        // Target fields updated to the supplied values
        assert_eq!(next.target_gas_for_batch, 2_000_000);
        assert_eq!(next.target_da_for_batch, Some(800));
        assert_eq!(next.target_da_footprint_for_batch, Some(350));
        assert_eq!(next.target_execution_time_for_batch_us, Some(200_000));
        assert_eq!(next.target_state_root_gas_for_batch, Some(80_000));

        // Per-batch limits and target count are preserved (..self)
        assert_eq!(next.target_flashblock_count, 10);
        assert_eq!(next.gas_per_batch, 3_000_000);
        assert_eq!(next.da_per_batch, Some(1_500));
        assert_eq!(next.da_footprint_per_batch, Some(600));
        assert_eq!(next.execution_time_per_batch_us, Some(300_000));
        assert_eq!(next.state_root_gas_per_batch, Some(150_000));
    }
}
