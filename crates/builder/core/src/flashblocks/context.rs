use core::fmt::Debug;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{Eip658Value, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::Database;
#[cfg(any(test, feature = "test-utils"))]
use alloy_primitives::B256;
use alloy_primitives::{Address, BlockHash, Bytes, TxHash, U256};
use alloy_rpc_types_eth::Withdrawals;
use base_bundles::{MeterBundleResponse, RejectedTransaction, RejectionReason};
use base_common_chains::Upgrades;
use base_common_consensus::{
    BaseReceipt, BaseTransactionSigned, CoinbaseTip, DepositReceipt, OpTxType,
};
use base_common_evm::{BaseReceiptBuilder, BaseSpecId, L1BlockInfo};
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130::IntrinsicGas;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, BuilderMetrics as SharedBuilderMetrics, CoinbaseTipAffordability,
    ValidityMetrics, error::BasePayloadBuilderError,
};
use base_execution_txpool::{
    BasePooledTx, GuardMetrics, PredicateContext, TimestampedTransaction,
    estimated_da_size::DataAvailabilitySized,
};
use base_observability_events::TransactionEventType;
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    ConfigureEvm, Evm, EvmEnv, EvmError, InvalidTxError, eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_node_api::PayloadBuilderError;
use reth_payload_builder::PayloadId;
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{InMemorySize, SealedHeader, SignedTransaction};
use reth_revm::{State, context::Block};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction};
use revm::{DatabaseCommit, context::result::ResultAndState, interpreter::as_u64_saturated};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Level, debug, span, trace, warn};

use crate::{
    BuilderConfig, BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded,
    ParkedPredicateIndex, PayloadTxsBounds, PredicateReadRecorder, ResourceLimits,
    StateChangeEffects, TxResources, TxnExecutionError, TxnOutcome, ValidityPredicateEvaluation,
    transaction_events::{
        BuilderAcceptedEventData, BuilderConsideredEventData, BuilderDeferredEventData,
        BuilderExpiredEventData, BuilderRejectedEventData, BuilderTransactionEventContext,
        emit_builder_transaction_event, rejection_reason_code,
    },
};

/// Records the priority fee of a rejected transaction with the given reason as a label.
fn record_rejected_tx_priority_fee(reason: &TxnExecutionError, priority_fee: f64) {
    BuilderMetrics::rejected_tx_priority_fee(rejection_reason_code(reason)).record(priority_fee);
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
    /// Number of successful park decisions in this flashblock.
    ///
    /// Incremented once per `park_current()` and never decremented. A later
    /// promote-and-reselect of the same transaction is a new consideration
    /// round and does not unwind this count.
    pub txs_deferred: u64,
    /// Number rejected by gas limit.
    pub txs_rejected_gas: u64,
    /// Number rejected by DA size limits (tx or block).
    pub txs_rejected_da: u64,
    /// Number rejected by DA footprint limit.
    pub txs_rejected_da_footprint: u64,
    /// Number rejected by the per-transaction execution time limit.
    pub txs_rejected_execution_time: u64,
    /// Number rejected by uncompressed size limit.
    pub txs_rejected_uncompressed_size: u64,
    /// Number skipped because metering data has not yet arrived.
    pub txs_rejected_metering_data_pending: u64,
    /// Number rejected or skipped for other reasons.
    pub txs_rejected_other: u64,
    /// Minimum effective priority fee (tip per gas) among included transactions.
    pub min_priority_fee: Option<u64>,
    /// Transaction hashes permanently rejected due to per-tx intrinsic limits.
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
    pub const fn rejection_counts(&self) -> [(&'static str, u64); 7] {
        [
            ("gas_limit", self.txs_rejected_gas),
            ("da_size", self.txs_rejected_da),
            ("da_footprint", self.txs_rejected_da_footprint),
            ("execution_time", self.txs_rejected_execution_time),
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
            + self.txs_rejected_uncompressed_size
            + self.txs_rejected_metering_data_pending
            + self.txs_rejected_other
    }

    /// Rejected plus deferred consideration outcomes.
    ///
    /// Completes `txs_considered == txs_included + txs_excluded_total()`
    /// when `txs_considered` is counted per selection attempt.
    pub const fn txs_excluded_total(&self) -> u64 {
        self.txs_rejected_total() + self.txs_deferred
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
            TxnExecutionError::ExecutionMeteringLimitExceeded(inner) => {
                let ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) = inner;
                self.txs_rejected_execution_time += 1;
            }
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
    /// Gas limit per flashblock
    pub gas_per_batch: u64,
    /// DA bytes limit per flashblock
    pub da_per_batch: Option<u64>,
    /// DA footprint limit per flashblock
    pub da_footprint_per_batch: Option<u64>,
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
    ) -> Self {
        Self {
            flashblock_index: self.flashblock_index + 1,
            target_gas_for_batch,
            target_da_for_batch,
            target_da_footprint_for_batch,
            ..self
        }
    }
}

/// Container type that holds all the necessities to build a new payload.
#[derive(Debug)]
pub struct BasePayloadBuilderCtx {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: BaseEvmConfig,
    /// The chainspec
    pub chain_spec: Arc<BaseChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<BasePayloadBuilderAttributes<BaseTransactionSigned>>,
    /// Evm Settings
    pub evm_env: EvmEnv<BaseSpecId>,
    /// Block env attributes for the current block.
    pub block_env_attributes: BaseNextBlockEnvAttributes,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// Extra context for the payload builder
    pub extra: FlashblocksExtraCtx,
    /// Builder configuration containing limits and metering settings.
    pub builder_config: BuilderConfig,
    /// Sender for forwarding per-block batches of rejected transactions to the audit-archiver.
    pub rejected_tx_sender: Option<mpsc::Sender<Vec<RejectedTransaction>>>,
}

impl BasePayloadBuilderCtx {
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
    pub(super) const fn attributes(&self) -> &BasePayloadBuilderAttributes<BaseTransactionSigned> {
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
    pub fn block_number(&self) -> u64 {
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
        self.attributes().payload_id(&self.parent_hash())
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

    fn record_rejected_tx(
        &self,
        info: &mut ExecutionInfo,
        tx_hash: TxHash,
        reason: RejectionReason,
        metering: MeterBundleResponse,
    ) {
        if self.rejected_tx_sender.is_none() {
            return;
        }

        if info.rejected_txs.len() >= self.builder_config.max_rejected_txs_per_block {
            BuilderMetrics::rejected_tx_per_block_drops().increment(1);
            return;
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        info.rejected_txs.push(RejectedTransaction {
            tx_hash,
            block_number: self.block_number(),
            reason,
            timestamp: now,
            metering,
        });
    }

    /// Flushes all accumulated rejected transactions to the audit-archiver channel
    /// as a single per-block batch.
    pub fn flush_rejected_txs(&self, info: &mut ExecutionInfo) {
        if info.rejected_txs.is_empty() {
            return;
        }

        if let Some(sender) = &self.rejected_tx_sender {
            let batch = std::mem::take(&mut info.rejected_txs);
            let batch_size = batch.len();
            if let Err(e) = sender.try_send(batch) {
                BuilderMetrics::rejected_tx_channel_drops().increment(batch_size as u64);
                warn!(
                    target: "payload_builder",
                    error = %e,
                    batch_size,
                    "Rejected tx channel full or closed, dropping batch"
                );
            }
        }
    }

    fn builder_transaction_event_context(
        &self,
        payload_id: &str,
        ordering_position: Option<u64>,
        block_hash: Option<BlockHash>,
    ) -> BuilderTransactionEventContext {
        BuilderTransactionEventContext {
            payload_id: payload_id.to_string(),
            block_number: self.block_number(),
            block_hash,
            parent_hash: self.parent_hash(),
            flashblock_index: Some(self.flashblock_index()),
            target_flashblock_count: self.target_flashblock_count(),
            ordering_position,
            builder_mode: "flashblocks",
            source_queue: "txpool_best",
        }
    }

    fn emit_builder_decision_event<D, F>(
        &self,
        payload_id: &str,
        event_type: TransactionEventType,
        tx_hash: TxHash,
        ordering_position: Option<u64>,
        data: F,
    ) where
        D: Serialize,
        F: FnOnce() -> D,
    {
        emit_builder_transaction_event(
            self.builder_transaction_event_context(payload_id, ordering_position, None),
            event_type,
            tx_hash,
            data,
        );
    }
}

/// The context needed to emit a defer/reject builder-decision event for one candidate.
pub(crate) struct DecisionContext<'a> {
    /// Payload identifier used to correlate emitted events.
    payload_id: &'a str,
    /// Execution info snapshot read into event budget fields.
    info: &'a ExecutionInfo,
    /// Resource limits read into event budget fields.
    limits: &'a ResourceLimits,
    /// Machine-readable reason code recorded on the emitted event.
    reason: &'static str,
    /// Human-readable detail recorded on the emitted event.
    detail: &'static str,
}

impl BasePayloadBuilderCtx {
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
    ///
    /// When `no_tx_pool` is set the attribute-supplied transaction list is the consensus input
    /// for the payload (derived from L1 batches by `base-consensus`), not a list of optional
    /// pre-include candidates. In that mode any invalid-tx error must be propagated as fatal so
    /// the EL rejects the payload, matching the strictness of the proof executor and allowing
    /// Holocene's deposit-only fallback to apply consistently across both consumers.
    ///
    /// When `no_tx_pool` is `false` the builder is composing a new block from mempool plus
    /// attribute pre-includes; pre-includes there may legitimately be skipped on invalid-tx
    /// errors, so the historical skip-and-continue behavior is preserved.
    pub(super) fn execute_sequencer_transactions(
        &self,
        db: &mut State<impl Database>,
    ) -> Result<ExecutionInfo, PayloadBuilderError> {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());
        let no_tx_pool = self.attributes().no_tx_pool;

        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

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
            // Note that this *only* needs to be done post-regolith upgrade, as deposit nonces
            // were not introduced in Bedrock. In addition, regular transactions don't have deposit
            // nonces, so we don't need to touch the DB for those.
            let depositor_nonce = (self.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
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
                    if err.is_invalid_tx_err() && !no_tx_pool {
                        trace!(target: "payload_builder", %err, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                        continue;
                    }
                    // Either a fatal execution error, or an invalid-tx error from an
                    // attribute-derived (`no_tx_pool=true`) transaction list. The latter must
                    // be fatal so the EL rejects the payload exactly like the proof executor
                    // does.
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            let gas_used = result.tx_gas_used();
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
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_inner());
        }

        let da_footprint_gas_scalar = self
            .chain_spec
            .is_jovian_active_at_timestamp(self.attributes().timestamp())
            .then(|| {
                L1BlockInfo::fetch_da_footprint_gas_scalar(evm.db_mut())
                    .expect("DA footprint should always be available from the database post jovian")
            });

        info.da_footprint_scalar = da_footprint_gas_scalar;

        Ok(info)
    }

    /// Runs `f`, adding its wall-clock duration to `total`. `total` stays `None`
    /// until the first call, so a `Some` result also records that at least one
    /// predicate was evaluated during the build; this gates the per-block
    /// histogram so blocks without validity transactions emit no observation.
    fn accumulate_elapsed<T>(total: &mut Option<Duration>, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let value = f();
        *total.get_or_insert(Duration::ZERO) += start.elapsed();
        value
    }

    /// Closes the iterator's current candidate.
    ///
    /// Nonce-free replay-ID entries are independent, so they are committed
    /// rather than invalidating the sender's nonce lane. Either path is required
    /// before the next `best_txs.next()` — leaving `current` set panics in debug.
    fn skip_current<B: PayloadTxsBounds>(
        best_txs: &mut B,
        sender: Address,
        nonce: u64,
        replay_independent: bool,
    ) {
        if replay_independent {
            best_txs.mark_current_committed();
        } else {
            best_txs.mark_invalid(sender, nonce);
        }
    }

    /// [`Self::skip_current`] using the pooled transaction's sender, nonce, and replay ID.
    fn skip_pooled_current<B: PayloadTxsBounds>(best_txs: &mut B, tx: &B::Transaction) {
        Self::skip_current(best_txs, tx.sender(), tx.nonce(), tx.identity().is_replay());
    }

    fn emit_considered(&self, cx: &DecisionContext<'_>, tx_hash: TxHash, ordering_position: u64) {
        self.emit_builder_decision_event(
            cx.payload_id,
            TransactionEventType::BuilderConsidered,
            tx_hash,
            Some(ordering_position),
            || BuilderConsideredEventData::new(cx.info, cx.limits, None),
        );
    }

    /// Emits considered + rejected, counts an "other" rejection, and closes the
    /// current iterator candidate.
    fn reject_current<B: PayloadTxsBounds>(
        &self,
        best_txs: &mut B,
        diag: &mut FlashblockDiagnostics,
        cx: &DecisionContext<'_>,
        tx: &B::Transaction,
        ordering_position: u64,
    ) {
        let tx_hash = *tx.hash();
        self.emit_considered(cx, tx_hash, ordering_position);
        self.emit_builder_decision_event(
            cx.payload_id,
            TransactionEventType::BuilderRejected,
            tx_hash,
            Some(ordering_position),
            || BuilderRejectedEventData::new(cx.reason, cx.detail, false, cx.info, cx.limits, None),
        );
        diag.txs_rejected_other += 1;
        Self::skip_pooled_current(best_txs, tx);
    }

    /// Emits considered + expired, records a permanent rejection, and closes the
    /// current iterator candidate.
    fn expire_current<B: PayloadTxsBounds>(
        &self,
        best_txs: &mut B,
        diag: &mut FlashblockDiagnostics,
        cx: &DecisionContext<'_>,
        tx: &B::Transaction,
        ordering_position: u64,
    ) {
        let tx_hash = *tx.hash();
        self.emit_considered(cx, tx_hash, ordering_position);
        self.emit_builder_decision_event(
            cx.payload_id,
            TransactionEventType::BuilderExpired,
            tx_hash,
            Some(ordering_position),
            || BuilderExpiredEventData::new(cx.reason, cx.detail, cx.info, cx.limits, None),
        );
        diag.txs_rejected_other += 1;
        diag.permanently_rejected_txs.push(tx_hash);
        // Same series as the pool-side block eviction: the builder drops the tx
        // before the pool sweep sees it.
        GuardMetrics::record_block_expiry_invalidations(1);
        Self::skip_pooled_current(best_txs, tx);
    }

    /// Defers the current validity-gated candidate by parking it for a later flashblock, or, when
    /// the iterator cannot park it, rejects it and closes the candidate. Emits the matching
    /// builder-decision event and updates `diag`. Returns `true` when the transaction was parked.
    fn defer_or_reject_current<B: PayloadTxsBounds>(
        &self,
        best_txs: &mut B,
        diag: &mut FlashblockDiagnostics,
        cx: &DecisionContext<'_>,
        tx: &B::Transaction,
        ordering_position: u64,
    ) -> bool {
        if best_txs.park_current() {
            self.emit_builder_decision_event(
                cx.payload_id,
                TransactionEventType::BuilderDeferred,
                *tx.hash(),
                Some(ordering_position),
                || BuilderDeferredEventData::new(cx.reason, cx.detail, cx.info, cx.limits, None),
            );
            diag.txs_deferred += 1;
            true
        } else {
            self.emit_builder_decision_event(
                cx.payload_id,
                TransactionEventType::BuilderRejected,
                *tx.hash(),
                Some(ordering_position),
                || {
                    BuilderRejectedEventData::new(
                        cx.reason, cx.detail, false, cx.info, cx.limits, None,
                    )
                },
            );
            diag.txs_rejected_other += 1;
            Self::skip_pooled_current(best_txs, tx);
            false
        }
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

        // Number of validity-predicate index buckets woken (their watched balance or storage
        // slot actually changed) across this flashblock build.
        let mut predicate_bucket_wakeups: u64 = 0;

        let min_tx_index = info.executed_transactions.len() as u64;
        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        debug!(
            target: "payload_builder",
            message = "Executing best transactions",
            block_data_limit = ?limits.block_data_limit,
            tx_data_limit = ?limits.tx_data_limit,
            block_gas_limit = ?limits.block_gas_limit,
            execution_metering_mode = ?self.builder_config.execution_metering_mode,
        );

        let block_number = as_u64_saturated!(self.evm_env.block_env.number);
        let block_timestamp = self.attributes().timestamp();
        let payload_id = self.payload_id().to_string();
        let mut predicate_index = ParkedPredicateIndex::default();
        let predicate_context =
            PredicateContext { block_number, flashblock_index: self.flashblock_index() };

        // Total validity-predicate evaluation time (inclusive of the state loads each
        // evaluation performs) across this flashblock build. `None` until the first
        // evaluation, so it both accumulates and records whether any validity
        // transaction was seen; emitted once when the loop finishes.
        let mut predicate_eval_total: Option<Duration> = None;
        let mut validity_candidates_evaluated = 0_u64;
        let mut validity_candidates_deferred = 0_u64;
        let mut predicate_eval_cutoff_hit = false;

        while let Some(tx) = best_txs.next(()) {
            if self.cancel.is_cancelled() {
                diag.cancelled = true;
                diag.txs_considered = num_txs_considered;
                diag.txs_included =
                    (info.executed_transactions.len() as u64).saturating_sub(min_tx_index);
                return Ok(diag);
            }

            num_txs_considered += 1;
            let ordering_position = num_txs_considered;
            let tx_hash = *tx.hash();
            let replay_independent = tx.identity().is_replay();
            let has_validity_predicates = !tx.validity_predicates().is_empty();
            let coinbase_tip =
                tx.as_eip8130().and_then(|signed| CoinbaseTip::decode(signed.tx(), tx.sender()));
            let has_coinbase_tip = coinbase_tip.is_some();

            // Defer without evaluating once this flashblock's predicate-eval time budget is
            // exhausted, rather than spending more IO on the naive per-transaction loop. The
            // deferred transaction is never rejected from the pool, so it is picked up as a
            // fresh candidate next flashblock, when the budget resets.
            if has_validity_predicates
                && predicate_eval_total
                    .is_some_and(|total| total >= self.builder_config.predicate_eval_hard_cutoff)
            {
                let cx = DecisionContext {
                    payload_id: &payload_id,
                    info,
                    limits,
                    reason: "predicate_eval_budget_exhausted",
                    detail: "validity-predicate evaluation time budget exhausted for this flashblock",
                };
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    "deferring validity-gated transaction: predicate evaluation budget exhausted for this flashblock"
                );
                self.emit_considered(&cx, tx_hash, ordering_position);
                ValidityMetrics::validity_predicate_evaluations_total("budget_exhausted")
                    .increment(1);
                validity_candidates_deferred += 1;
                predicate_eval_cutoff_hit = true;
                self.defer_or_reject_current(best_txs, &mut diag, &cx, &tx, ordering_position);
                continue;
            }

            let mut predicate_read_failed = false;
            let mut predicate_expired = false;
            let blocking_predicate = if has_validity_predicates {
                validity_candidates_evaluated += 1;
                match Self::accumulate_elapsed(&mut predicate_eval_total, || {
                    let mut recorder =
                        PredicateReadRecorder::new(&mut **evm.db_mut(), &mut info.predicate_loads);
                    ValidityPredicateEvaluation::evaluate(
                        tx.validity_predicates(),
                        &mut recorder,
                        &predicate_context,
                    )
                }) {
                    Ok(ValidityPredicateEvaluation::Matched) => None,
                    Ok(ValidityPredicateEvaluation::Unsatisfied { blocker, expired }) => {
                        predicate_expired = expired;
                        Some(blocker)
                    }
                    Err(error) => {
                        warn!(
                            target: "payload_builder",
                            tx_hash = ?tx_hash,
                            error = ?error,
                            "failed to read validity predicate state"
                        );
                        predicate_read_failed = true;
                        None
                    }
                }
            } else {
                None
            };
            if has_validity_predicates {
                let outcome = if predicate_read_failed {
                    "read_error"
                } else if blocking_predicate.is_some() {
                    "not_satisfied"
                } else {
                    "matched"
                };
                ValidityMetrics::validity_predicate_evaluations_total(outcome).increment(1);
            }
            if predicate_read_failed || blocking_predicate.is_some() {
                let (reason, detail) = if predicate_read_failed {
                    (
                        "validity_predicate_read_failed",
                        "failed to read state required by a validity predicate",
                    )
                } else if predicate_expired {
                    (
                        "validity_predicate_expired",
                        "a validity predicate can no longer be satisfied at or after the current build position",
                    )
                } else {
                    (
                        "validity_predicate_not_satisfied",
                        "a validity predicate is not satisfied by the current build state",
                    )
                };
                let cx = DecisionContext { payload_id: &payload_id, info, limits, reason, detail };
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    decision_reason = reason,
                    "skipping transaction with unsatisfied validity predicate"
                );
                // A read failure cannot be retried at a later ordering position: including the
                // transaction there could place it behind a lower-priority transaction even though
                // its predicate may have already been satisfied at its first position. An expired
                // position predicate is terminal too — no later position can satisfy it — so both
                // are dropped rather than parked; only recoverable state mismatches are parked.
                if predicate_read_failed {
                    // A read failure is only terminal for this scan, so it is not cached.
                    self.reject_current(best_txs, &mut diag, &cx, &tx, ordering_position);
                } else if predicate_expired {
                    // A passed position bound can never be satisfied in any later
                    // block, so an expired predicate is permanently terminal:
                    // record it for the rejection cache and pool eviction so it is
                    // not re-evaluated on subsequent flashblock rebuilds.
                    self.expire_current(best_txs, &mut diag, &cx, &tx, ordering_position);
                } else {
                    // Recoverable state mismatch: park under the current blocker to retry at a
                    // later position or flashblock, or reject if the iterator cannot park it.
                    self.emit_considered(&cx, tx_hash, ordering_position);
                    let blocking_predicate = blocking_predicate
                        .expect("unsatisfied, non-terminal predicate implies a blocking key");
                    if self.defer_or_reject_current(
                        best_txs,
                        &mut diag,
                        &cx,
                        &tx,
                        ordering_position,
                    ) {
                        predicate_index.park(tx_hash, tx, blocking_predicate);
                    }
                }
                continue;
            }

            if self.builder_config.manifest_precheck_enabled
                && let Some(manifest) = tx.watch_manifest()
                && let Err(stale) = manifest.revalidate(evm.db_mut(), block_timestamp)
            {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    cause = stale.cause(),
                    "skipping EIP-8130 transaction with stale authorization manifest"
                );
                GuardMetrics::record_builder_precheck_drop(&stale);
                self.reject_current(
                    best_txs,
                    &mut diag,
                    &DecisionContext {
                        payload_id: &payload_id,
                        info,
                        limits,
                        reason: "manifest_precheck_stale",
                        detail: stale.cause(),
                    },
                    &tx,
                    ordering_position,
                );
                continue;
            }

            let tx_da_size = tx.estimated_da_size();
            let tx_received_at_ms = tx.received_at();

            // EIP-8130 meters payer authentication gas on top of the declared gas limit, so it must
            // be reserved against the block gas budget in addition to `gas_limit`. Reserve a
            // conservative upper bound (worst-case payer policy gate) derived from the payer auth
            // blob (`0` for non-8130 / self-pay); see `IntrinsicGas::max_payer_auth_cost`.
            let tx_payer_auth = match tx.as_eip8130() {
                Some(signed) => match IntrinsicGas::max_payer_auth_cost(signed) {
                    Ok(payer_auth) => payer_auth,
                    Err(err) => {
                        trace!(
                            target: "payload_builder",
                            %err,
                            tx_hash = ?tx_hash,
                            "skipping EIP-8130 transaction with unschedulable payer authenticator"
                        );
                        self.reject_current(
                            best_txs,
                            &mut diag,
                            &DecisionContext {
                                payload_id: &payload_id,
                                info,
                                limits,
                                reason: "unschedulable_payer_authenticator",
                                detail: "EIP-8130 payer authenticator cannot be scheduled against the gas budget",
                            },
                            &tx,
                            ordering_position,
                        );
                        continue;
                    }
                },
                None => 0,
            };

            if CoinbaseTipAffordability::unaffordable(&tx, tx_payer_auth, evm.db_mut()) {
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx_hash,
                    "skipping transaction unable to pay gas plus declared coinbase tip"
                );
                self.reject_current(
                    best_txs,
                    &mut diag,
                    &DecisionContext {
                        payload_id: &payload_id,
                        info,
                        limits,
                        reason: "unaffordable_coinbase_tip",
                        detail: "sender and gas payer cannot cover worst-case gas plus the declared coinbase tip",
                    },
                    &tx,
                    ordering_position,
                );
                continue;
            }

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
                    let err = TxnExecutionError::MeteringDataPending;
                    let tx_resources = TxResources {
                        da_size: tx_da_size,
                        gas_limit: tx.gas_limit(),
                        payer_auth: tx_payer_auth,
                        execution_time_us: None,
                        uncompressed_size: tx_uncompressed_size,
                    };
                    self.emit_builder_decision_event(
                        &payload_id,
                        TransactionEventType::BuilderConsidered,
                        tx_hash,
                        Some(ordering_position),
                        || {
                            BuilderConsideredEventData::new(info, limits, Some(&tx_resources))
                                .with_metering_wait(tx_age_ms, wait_duration.as_millis())
                        },
                    );
                    self.emit_builder_decision_event(
                        &payload_id,
                        TransactionEventType::BuilderRejected,
                        tx_hash,
                        Some(ordering_position),
                        || {
                            BuilderRejectedEventData::from_error(
                                &err,
                                info,
                                limits,
                                Some(&tx_resources),
                            )
                            .with_metering_wait(tx_age_ms, wait_duration.as_millis())
                        },
                    );
                    log_txn(Err(err));
                    BuilderMetrics::metering_data_pending_skip().increment(1);
                    self.builder_config.metering_provider.skip(&tx_hash);
                    Self::skip_current(best_txs, tx.signer(), tx.nonce(), replay_independent);
                    continue;
                }
            }

            // Extract predicted execution time from metering data
            let predicted_execution_time_us =
                resource_usage.as_ref().map(|m| m.total_execution_time_us);

            // Build tx resources struct
            let tx_resources = TxResources {
                da_size: tx_da_size,
                gas_limit: tx.gas_limit(),
                payer_auth: tx_payer_auth,
                execution_time_us: predicted_execution_time_us,
                uncompressed_size: tx_uncompressed_size,
            };
            self.emit_builder_decision_event(
                &payload_id,
                TransactionEventType::BuilderConsidered,
                tx_hash,
                Some(ordering_position),
                || BuilderConsideredEventData::new(info, limits, Some(&tx_resources)),
            );

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

                        let ExecutionMeteringLimitExceeded::TransactionExecutionTime(
                            tx_time_us,
                            limit_us,
                        ) = limit_err;
                        // Only record per-tx execution time limits for the audit trail for now
                        self.record_rejected_tx(
                            info,
                            tx_hash,
                            RejectionReason::ExecutionTimeExceeded {
                                tx_time_us: *tx_time_us,
                                limit_us: *limit_us,
                            },
                            resource_usage.unwrap_or_default(),
                        );

                        self.emit_builder_decision_event(
                            &payload_id,
                            TransactionEventType::BuilderRejected,
                            tx_hash,
                            Some(ordering_position),
                            || {
                                BuilderRejectedEventData::from_error(
                                    &err,
                                    info,
                                    limits,
                                    Some(&tx_resources),
                                )
                                .with_dry_run(false)
                            },
                        );
                        log_txn(Err(err));
                        Self::skip_current(best_txs, tx.signer(), tx.nonce(), replay_independent);
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

                    self.emit_builder_decision_event(
                        &payload_id,
                        TransactionEventType::BuilderRejected,
                        tx_hash,
                        Some(ordering_position),
                        || {
                            BuilderRejectedEventData::from_error(
                                &err,
                                info,
                                limits,
                                Some(&tx_resources),
                            )
                        },
                    );
                    log_txn(Err(err));
                    Self::skip_current(best_txs, tx.signer(), tx.nonce(), replay_independent);
                    continue;
                }
            }

            // Record execution time prediction accuracy metrics
            if let Some(predicted_us) = predicted_execution_time_us {
                BuilderMetrics::tx_predicted_execution_time_us().record(predicted_us as f64);
            }
            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                let err = TxnExecutionError::SequencerTransaction;
                diag.record_rejection(&err);
                let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                record_rejected_tx_priority_fee(&err, priority_fee);
                self.emit_builder_decision_event(
                    &payload_id,
                    TransactionEventType::BuilderRejected,
                    tx_hash,
                    Some(ordering_position),
                    || {
                        BuilderRejectedEventData::from_error(
                            &err,
                            info,
                            limits,
                            Some(&tx_resources),
                        )
                    },
                );
                log_txn(Err(err));
                Self::skip_current(best_txs, tx.signer(), tx.nonce(), replay_independent);
                continue;
            }

            let tx_span = span!(
                Level::TRACE,
                "execute_transaction",
                tx_hash = %tx_hash,
                tx_gas_limit = tx.gas_limit(),
            );
            let _tx_span_guard = tx_span.enter();

            let execution_start_time = Instant::now();
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
                            self.emit_builder_decision_event(
                                &payload_id,
                                TransactionEventType::BuilderRejected,
                                tx_hash,
                                Some(ordering_position),
                                || {
                                    BuilderRejectedEventData::from_error(
                                        &diag_err,
                                        info,
                                        limits,
                                        Some(&tx_resources),
                                    )
                                },
                            );
                            log_txn(Err(diag_err));
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                            best_txs.mark_current_committed();
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            let diag_err = TxnExecutionError::InternalError(err.clone());
                            diag.record_rejection(&diag_err);
                            let priority_fee =
                                tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                            record_rejected_tx_priority_fee(&diag_err, priority_fee);
                            self.emit_builder_decision_event(
                                &payload_id,
                                TransactionEventType::BuilderRejected,
                                tx_hash,
                                Some(ordering_position),
                                || {
                                    BuilderRejectedEventData::from_error(
                                        &diag_err,
                                        info,
                                        limits,
                                        Some(&tx_resources),
                                    )
                                },
                            );
                            log_txn(Err(diag_err));
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            Self::skip_current(
                                best_txs,
                                tx.signer(),
                                tx.nonce(),
                                replay_independent,
                            );
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    log_txn(Err(TxnExecutionError::EvmError));
                    return Err(PayloadBuilderError::evm(err));
                }
            };

            let execution_time = execution_start_time.elapsed();

            // The "simulation" terminology comes from upstream op-rbuilder's name for
            // locally executing a candidate transaction before committing it to the payload;
            // this is not metering service simulation data from MeterBundleResponse.
            BuilderMetrics::tx_simulation_duration().record(execution_time);
            BuilderMetrics::tx_byte_size().record(tx.inner().size() as f64);
            num_txs_simulated += 1;

            // Record state modification counts (trie work proxy)
            let accounts_modified = state.len();
            let storage_slots_modified: usize = state.values().map(|a| a.storage.len()).sum();
            BuilderMetrics::tx_accounts_modified().record(accounts_modified as f64);
            BuilderMetrics::tx_storage_slots_modified().record(storage_slots_modified as f64);

            // Record execution time for unmetered transactions (race condition indicator)
            if resource_usage.is_none() {
                BuilderMetrics::unmetered_tx_actual_execution_time_us()
                    .record(execution_time.as_micros() as f64);
            }

            // Record prediction accuracy
            if let Some(predicted_us) = predicted_execution_time_us {
                let error = predicted_us as f64 - execution_time.as_micros() as f64;
                BuilderMetrics::execution_time_prediction_error_us().record(error);
            }

            let gas_used = result.tx_gas_used();
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
                if err.is_permanent() {
                    diag.permanently_rejected_txs.push(tx_hash);
                }
                self.emit_builder_decision_event(
                    &payload_id,
                    TransactionEventType::BuilderRejected,
                    tx_hash,
                    Some(ordering_position),
                    || {
                        BuilderRejectedEventData::from_error(
                            &err,
                            info,
                            limits,
                            Some(&tx_resources),
                        )
                    },
                );
                log_txn(Err(err));
                Self::skip_current(best_txs, tx.signer(), tx.nonce(), replay_independent);
                continue;
            }

            info.cumulative_gas_used += gas_used;
            // record tx da size
            info.cumulative_da_bytes_used += tx_da_size;
            // record uncompressed tx size
            info.cumulative_uncompressed_bytes += tx_uncompressed_size;

            self.emit_builder_decision_event(
                &payload_id,
                TransactionEventType::BuilderAccepted,
                tx_hash,
                Some(ordering_position),
                || {
                    BuilderAcceptedEventData::new(
                        if is_success { "success" } else { "reverted" },
                        gas_used,
                        info,
                        limits,
                        Some(&tx_resources),
                    )
                },
            );
            // Push transaction changeset and calculate header bloom filter for receipt.
            let ctx = ReceiptBuilderCtx {
                tx_type: tx.tx_type(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };
            info.receipts.push(self.build_receipt(ctx, None));

            let state_change_effects = if predicate_index.is_empty() {
                StateChangeEffects::default()
            } else {
                predicate_index.affected_by_state(&state)
            };
            predicate_bucket_wakeups += state_change_effects.woken_buckets as u64;

            // commit changes
            evm.db_mut().commit(state);

            // Release the committed transaction's lane before promoting predicate-unblocked heads.
            best_txs.mark_current_committed();
            let predicate_rescan_start = Instant::now();
            for (rescanned, parked_hash) in
                state_change_effects.affected_transactions.iter().enumerate()
            {
                // Stop rescanning once this flashblock's predicate-eval time budget is
                // exhausted. Remaining affected transactions stay parked under their current
                // blocker exactly as they are; they are picked up as fresh candidates next
                // flashblock, when the budget resets.
                if predicate_eval_total
                    .is_some_and(|total| total >= self.builder_config.predicate_eval_hard_cutoff)
                {
                    let remaining =
                        (state_change_effects.affected_transactions.len() - rescanned) as u64;
                    ValidityMetrics::validity_predicate_evaluations_total(
                        "rescan_budget_exhausted",
                    )
                    .increment(remaining);
                    predicate_eval_cutoff_hit = true;
                    break;
                }
                let mut predicate_read_failed = false;
                let Some(parked_transaction) = predicate_index.transaction(*parked_hash) else {
                    warn!(
                        target: "payload_builder",
                        tx_hash = ?parked_hash,
                        "affected transaction is no longer predicate-indexed"
                    );
                    continue;
                };
                let blocking_predicate =
                    match Self::accumulate_elapsed(&mut predicate_eval_total, || {
                        let mut recorder = PredicateReadRecorder::new(
                            &mut **evm.db_mut(),
                            &mut info.predicate_loads,
                        );
                        ValidityPredicateEvaluation::evaluate(
                            parked_transaction.validity_predicates(),
                            &mut recorder,
                            &predicate_context,
                        )
                    }) {
                        Ok(ValidityPredicateEvaluation::Matched) => None,
                        Ok(ValidityPredicateEvaluation::Unsatisfied { blocker, .. }) => {
                            Some(blocker)
                        }
                        Err(error) => {
                            warn!(
                                target: "payload_builder",
                                tx_hash = ?parked_hash,
                                error = ?error,
                                "failed to re-read validity predicate state"
                            );
                            predicate_read_failed = true;
                            None
                        }
                    };
                let outcome = if predicate_read_failed {
                    "rescan_read_error"
                } else if blocking_predicate.is_some() {
                    "rescan_not_satisfied"
                } else {
                    "rescan_matched"
                };
                ValidityMetrics::validity_predicate_evaluations_total(outcome).increment(1);
                if predicate_read_failed {
                    predicate_index.remove(*parked_hash);
                    best_txs.discard_parked(*parked_hash);
                } else if let Some(blocking_predicate) = blocking_predicate {
                    predicate_index.reindex(*parked_hash, blocking_predicate);
                } else {
                    predicate_index.remove(*parked_hash);
                    best_txs.promote(*parked_hash);
                }
            }
            if !state_change_effects.affected_transactions.is_empty() {
                ValidityMetrics::validity_predicate_rescan_duration()
                    .record(predicate_rescan_start.elapsed().as_secs_f64());
            }

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);
            info.inclusion.record(
                has_validity_predicates,
                gas_used,
                miner_fee,
                base_fee,
                coinbase_tip.unwrap_or_default(),
            );

            // Per-tx tip-per-gas distribution (builder priority score), tagged
            // by flow cohort and bid mechanism. `X` for top-X-percentile share is
            // left to Datadog percentile aggregations — do not bake it in here.
            SharedBuilderMetrics::record_tip_per_gas(
                has_validity_predicates,
                has_coinbase_tip,
                miner_fee as f64,
            );

            // track minimum priority fee for diagnostics (saturate u128 -> u64)
            let fee_u64 = miner_fee.min(u64::MAX as u128) as u64;
            diag.min_priority_fee = Some(diag.min_priority_fee.map_or(fee_u64, |m| m.min(fee_u64)));

            // Record metering hit/miss only for committed transactions so the
            // metric reflects actual payload inclusion, not speculative lookups.
            if self.builder_config.metering_provider.is_enabled() && resource_usage.is_some() {
                BuilderMetrics::metering_known_transaction().increment(1);
            } else {
                BuilderMetrics::metering_unknown_transaction().increment(1);
                if self.builder_config.metering_provider.is_enabled() {
                    self.builder_config.metering_provider.mark_included_without_metering(&tx_hash);
                }
            }

            // append sender and transaction to the respective lists
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
        }

        // Record accumulated validity-predicate evaluation time once per flashblock build.
        // `None` means no validity transactions were evaluated, so nothing is emitted and
        // the histogram is not flooded with zero observations.
        if let Some(predicate_eval_total) = predicate_eval_total {
            ValidityMetrics::record_predicate_eval_duration(predicate_eval_total);
        }
        ValidityMetrics::record_predicate_evaluation_coverage(
            validity_candidates_evaluated,
            validity_candidates_deferred,
            predicate_eval_cutoff_hit,
        );

        let payload_transaction_simulation_time = execute_txs_start_time.elapsed();
        BuilderMetrics::set_payload_builder_metrics(
            payload_transaction_simulation_time.as_secs_f64(),
            num_txs_considered as f64,
            num_txs_simulated as f64,
            num_txs_simulated_success as f64,
            num_txs_simulated_fail as f64,
            reverted_gas_used as f64,
        );
        ValidityMetrics::record_predicate_index_diagnostics(
            predicate_bucket_wakeups,
            &predicate_index,
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
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
use base_execution_payload_builder::payload::EthPayloadBuilderAttributes;

#[cfg(any(test, feature = "test-utils"))]
impl BasePayloadBuilderCtx {
    /// Creates a minimal [`BasePayloadBuilderCtx`] for unit tests.
    ///
    /// Derives the EVM environment from the given chain spec and parent header,
    /// using default builder attributes and a no-op cancellation token.
    pub fn for_test(chain_spec: Arc<BaseChainSpec>, parent: Arc<SealedHeader>) -> Self {
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let timestamp = parent.timestamp + 2;

        let attributes = BasePayloadBuilderAttributes {
            payload_attributes: EthPayloadBuilderAttributes {
                id: PayloadId::new([0; 8]),
                parent: parent.hash(),
                timestamp,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            gas_limit: Some(parent.gas_limit),
            ..Default::default()
        };

        let block_env_attributes = BaseNextBlockEnvAttributes {
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

        let payload_id = attributes.payload_id(&parent.hash());
        let config = PayloadConfig::new(parent, attributes, payload_id);

        Self {
            evm_config,
            chain_spec,
            config,
            evm_env,
            block_env_attributes,
            cancel: CancellationToken::new(),
            extra: FlashblocksExtraCtx::default(),
            builder_config: crate::BuilderConfig::default(),
            rejected_tx_sender: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, TxKind, U256};
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTransactionSigned, BaseTypedTransaction, TxDeposit};
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_txpool::BasePooledTransaction;
    use reth_chainspec::ChainSpec;
    use reth_payload_util::PayloadTransactions;
    use reth_primitives_traits::{Recovered, SealedHeader, WithEncoded};
    use reth_provider::noop::NoopProvider;
    use reth_revm::{State, database::StateProviderDatabase};

    use super::*;
    use crate::{ParkablePayloadTransactions, test_utils::sign_base_tx};

    fn test_builder_context() -> BasePayloadBuilderCtx {
        let genesis: serde_json::Value = serde_json::json!({
            "config": { "chainId": 901 },
            "gasLimit": "0x1C9C380",
            "timestamp": "0x0"
        });
        let genesis = serde_json::from_value(genesis).expect("valid genesis");
        let inner =
            ChainSpec::builder().chain(901.into()).genesis(genesis).cancun_activated().build();
        let chain_spec = Arc::new(BaseChainSpec::from(inner));
        let parent_header = Header { gas_limit: 30_000_000, timestamp: 0, ..Default::default() };
        let parent = Arc::new(SealedHeader::seal_slow(parent_header));
        BasePayloadBuilderCtx::for_test(chain_spec, parent)
    }

    fn pooled_test_transaction() -> BasePooledTransaction {
        let signer = PrivateKeySigner::random();
        let transaction = TxEip1559 {
            chain_id: 901,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(signer.address()),
            ..Default::default()
        };
        let recovered = sign_base_tx(&signer, BaseTypedTransaction::Eip1559(transaction))
            .expect("sign test transaction");
        let encoded_len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, encoded_len)
    }

    fn pooled_deposit_test_transaction() -> BasePooledTransaction {
        let sender = Address::ZERO;
        let transaction = TxDeposit {
            source_hash: B256::ZERO,
            from: sender,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 0,
            is_system_transaction: false,
            input: Default::default(),
        };
        let signed: BaseTransactionSigned = transaction.into();
        let recovered = Recovered::new_unchecked(signed, sender);
        let encoded_len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, encoded_len)
    }

    #[derive(Debug)]
    struct LimitRejectionTransactions {
        within_limit_transaction: BasePooledTransaction,
        over_limit_transaction: BasePooledTransaction,
        within_limit_remaining: usize,
        over_limit_remaining: usize,
        current_is_over_limit: bool,
        over_limit_rejections: usize,
        cancel: CancellationToken,
    }

    impl LimitRejectionTransactions {
        fn new(within_limit: usize, over_limit: usize, cancel: CancellationToken) -> Self {
            Self {
                within_limit_transaction: pooled_deposit_test_transaction(),
                over_limit_transaction: pooled_test_transaction(),
                within_limit_remaining: within_limit,
                over_limit_remaining: over_limit,
                current_is_over_limit: false,
                over_limit_rejections: 0,
                cancel,
            }
        }
    }

    impl PayloadTransactions for LimitRejectionTransactions {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            if self.within_limit_remaining > 0 {
                self.within_limit_remaining -= 1;
                self.current_is_over_limit = false;
                return Some(self.within_limit_transaction.clone());
            }
            if self.over_limit_remaining > 0 {
                self.over_limit_remaining -= 1;
                self.current_is_over_limit = true;
                return Some(self.over_limit_transaction.clone());
            }
            None
        }

        fn mark_invalid(&mut self, _sender: alloy_primitives::Address, _nonce: u64) {
            if self.current_is_over_limit {
                self.over_limit_rejections += 1;
                self.cancel.cancel();
            }
        }
    }

    impl ParkablePayloadTransactions for LimitRejectionTransactions {
        fn park_current(&mut self) -> bool {
            false
        }

        fn mark_current_committed(&mut self) {}

        fn promote(&mut self, _transaction_hash: TxHash) -> bool {
            false
        }

        fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct LifecycleRecorder {
        invalid: usize,
        committed: usize,
    }

    impl PayloadTransactions for LifecycleRecorder {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            None
        }

        fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {
            self.invalid += 1;
        }
    }

    impl ParkablePayloadTransactions for LifecycleRecorder {
        fn park_current(&mut self) -> bool {
            false
        }

        fn mark_current_committed(&mut self) {
            self.committed += 1;
        }

        fn promote(&mut self, _transaction_hash: TxHash) -> bool {
            false
        }

        fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
            false
        }
    }

    #[test]
    fn skip_current_commits_replay_independent_candidates() {
        let mut txs = LifecycleRecorder::default();
        BasePayloadBuilderCtx::skip_current(&mut txs, Address::ZERO, 0, true);
        assert_eq!((txs.committed, txs.invalid), (1, 0));

        BasePayloadBuilderCtx::skip_current(&mut txs, Address::ZERO, 0, false);
        assert_eq!((txs.committed, txs.invalid), (1, 1));
    }

    #[test]
    fn cancellation_is_checked_after_each_limit_rejection() {
        let ctx = test_builder_context();
        let mut best_txs = LimitRejectionTransactions::new(0, 2, ctx.cancel.clone());
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut info = ExecutionInfo::default();
        let limits = ResourceLimits { block_gas_limit: 0, ..Default::default() };

        let diagnostics = ctx
            .execute_best_transactions(&mut info, &mut state, &mut best_txs, &limits)
            .expect("cancelled selection should succeed");

        assert!(diagnostics.cancelled);
        assert_eq!(diagnostics.txs_considered, 1);
        assert_eq!(diagnostics.txs_rejected_gas, 1);
        assert_eq!(best_txs.over_limit_rejections, 1);
    }

    #[test]
    fn cancellation_stops_twenty_thousand_transaction_limit_rejection_tail() {
        const WITHIN_LIMIT: usize = 10_000;
        const OVER_LIMIT: usize = 20_000;

        let ctx = test_builder_context();
        let mut best_txs =
            LimitRejectionTransactions::new(WITHIN_LIMIT, OVER_LIMIT, ctx.cancel.clone());
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut info = ExecutionInfo::default();
        let limits = ResourceLimits { block_gas_limit: 0, ..Default::default() };

        let diagnostics = ctx
            .execute_best_transactions(&mut info, &mut state, &mut best_txs, &limits)
            .expect("cancelled selection should succeed");

        assert!(diagnostics.cancelled);
        assert_eq!(diagnostics.txs_considered, WITHIN_LIMIT as u64 + 1);
        assert_eq!(diagnostics.txs_rejected_gas, 1);
        assert_eq!(best_txs.over_limit_rejections, 1);
        assert!(best_txs.over_limit_remaining > OVER_LIMIT - 10);
    }

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
        let diag = FlashblockDiagnostics { txs_rejected_gas: 2, ..Default::default() };

        assert_eq!(
            diag.rejection_counts(),
            [
                ("gas_limit", 2),
                ("da_size", 0),
                ("da_footprint", 0),
                ("execution_time", 0),
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

    #[test]
    fn diagnostics_count_deferred_outside_rejected_total() {
        let diag = FlashblockDiagnostics {
            txs_considered: 5,
            txs_included: 2,
            txs_deferred: 2,
            txs_rejected_other: 1,
            ..Default::default()
        };

        assert_eq!(diag.txs_rejected_total(), 1);
        assert_eq!(diag.txs_excluded_total(), 3);
        assert_eq!(diag.txs_considered, diag.txs_included + diag.txs_excluded_total());
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
            gas_per_batch: 3_000_000,
            da_per_batch: Some(1_500),
            da_footprint_per_batch: Some(600),
        };

        let next = ctx.next(
            2_000_000, // new gas target
            Some(800), // new DA target
            Some(350), // new DA footprint target
        );

        // Index incremented
        assert_eq!(next.flashblock_index, 3);

        // Target fields updated to the supplied values
        assert_eq!(next.target_gas_for_batch, 2_000_000);
        assert_eq!(next.target_da_for_batch, Some(800));
        assert_eq!(next.target_da_footprint_for_batch, Some(350));

        // Per-batch limits and target count are preserved (..self)
        assert_eq!(next.target_flashblock_count, 10);
        assert_eq!(next.gas_per_batch, 3_000_000);
        assert_eq!(next.da_per_batch, Some(1_500));
        assert_eq!(next.da_footprint_per_batch, Some(600));
    }

    /// Regression test: when the payload attributes are derived (`no_tx_pool=true`), an
    /// invalid-tx error from a sequencer transaction must be propagated as a fatal error
    /// rather than silently skipped.
    ///
    /// In `no_tx_pool=true` mode the attribute-supplied transaction list is the consensus
    /// input for the payload (produced by `base-consensus` from L1 batches). The proof
    /// executor strictly executes the full list and fails the block on any invalid tx; the
    /// EL must do the same so Holocene's deposit-only fallback applies consistently across
    /// both consumers of the same L1 input. Skip-and-continue here would let the EL freeze a
    /// safe-head whose state cannot be reproduced by an honest proof client.
    #[test]
    fn execute_sequencer_transactions_propagates_invalid_tx_when_no_tx_pool() {
        // A randomly-generated signer with no balance in the (empty) NoopProvider state.
        // Any non-deposit transfer attempt will fail validation with
        // `InvalidTransaction::LackOfFundForMaxFee` — an `is_invalid_tx_err()` outcome.
        let signer = PrivateKeySigner::random();
        let tx = TxEip1559 {
            chain_id: 901,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(signer.address()),
            value: U256::from(1u64),
            ..Default::default()
        };
        let recovered =
            sign_base_tx(&signer, BaseTypedTransaction::Eip1559(tx)).expect("sign sequencer tx");
        let signed = recovered.into_inner();
        let encoded = signed.encoded_2718().into();
        let with_encoded = WithEncoded::new(encoded, signed);

        // Strict mode: derived attributes (`no_tx_pool=true`) — the invalid tx must be fatal.
        let mut ctx = test_builder_context();
        ctx.config.attributes.no_tx_pool = true;
        ctx.config.attributes.transactions = vec![with_encoded];

        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let err = ctx
            .execute_sequencer_transactions(&mut state)
            .expect_err("invalid sequencer tx must fail when no_tx_pool=true");
        assert!(
            matches!(err, PayloadBuilderError::EvmExecutionError(_)),
            "expected EvmExecutionError, got: {err:?}"
        );

        // Mempool mode (`no_tx_pool=false`): pre-includes are still skippable. Identical
        // input now succeeds with zero gas consumed and no receipt — this guards against
        // accidentally tightening the legacy code path along with the strict one.
        ctx.config.attributes.no_tx_pool = false;
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let info = ctx
            .execute_sequencer_transactions(&mut state)
            .expect("invalid pre-include is skipped when no_tx_pool=false");
        assert_eq!(info.cumulative_gas_used, 0, "skipped tx should not consume gas");
        assert!(info.receipts.is_empty(), "skipped tx should not produce a receipt");
    }
}
