use core::fmt::Debug;
use std::{sync::Arc, time::Instant};

use alloy_consensus::{Eip658Value, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::Database;
#[cfg(any(test, feature = "test-utils"))]
use alloy_primitives::B256;
use alloy_primitives::{BlockHash, Bytes, TxHash, U256};
use alloy_rpc_types_eth::Withdrawals;
use base_bundles::RejectedTransaction;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned, DepositReceipt, OpTxType};
use base_common_evm::{BaseReceiptBuilder, BaseSpecId, L1BlockInfo};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, error::BasePayloadBuilderError,
};
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
use reth_transaction_pool::BestTransactionsAttributes;
use revm::{DatabaseCommit, context::result::ResultAndState, interpreter::as_u64_saturated};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Level, debug, span, trace, warn};

use crate::{
    BuilderConfig, BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, PayloadTxsBounds,
    ResourceLimits, TxnExecutionError,
    flashblocks::{
        candidate_source::{Candidate, PoolCandidateSource},
        gates::{
            BundleGate, Gate, GateRejection, GateVerdict, ManifestGate, ResourceLimitsGate,
            SequencerGate,
        },
        reporter::{OutcomeReporter, ReportedTx},
    },
};

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
        let reporter = OutcomeReporter::new(self, &payload_id);
        let gates = BundleGate::new(block_number, block_timestamp)
            .then(ManifestGate::new(self.builder_config.manifest_precheck_enabled, block_timestamp))
            .then(ResourceLimitsGate::new(
                &self.builder_config.metering_provider,
                self.builder_config.metering_wait_duration,
                self.builder_config.execution_metering_mode,
            ))
            .then(SequencerGate);
        let mut source = PoolCandidateSource::new(best_txs, base_fee);

        while let Some(mut candidate) = source.next_candidate() {
            num_txs_considered += 1;
            let ordering_position = num_txs_considered;

            // Emit the considered event up front, before any gate, so its timestamp marks when the
            // builder began evaluating the candidate. Decision-specific context (bundle window,
            // metering wait, dry-run) and the metering estimate land on the terminal event.
            reporter.considered(
                ReportedTx {
                    tx_hash: candidate.tx_hash,
                    ordering_position,
                    resources: Some(&candidate.resources),
                    priority_fee: candidate.priority_fee,
                },
                info,
                limits,
            );

            // Run the compound admission gate; it stops at the first rejection. The resource-limits
            // gate enriches the candidate with its metering estimate along the way.
            let rejection = match gates.evaluate(&mut candidate, info, limits, evm.db_mut()) {
                GateVerdict::Admit => None,
                GateVerdict::Reject(r) => Some(r),
            };

            let Candidate {
                tx,
                tx_hash,
                resources: tx_resources,
                resource_usage,
                priority_fee,
                eip8130_replay_id,
                ..
            } = candidate;
            let tx_da_size = tx_resources.da_size;
            let tx_uncompressed_size = tx_resources.uncompressed_size;
            let predicted_execution_time_us = tx_resources.execution_time_us;

            let reported = ReportedTx {
                tx_hash,
                ordering_position,
                resources: Some(&tx_resources),
                priority_fee,
            };

            if let Some(rejection) = rejection {
                // Nonce-free EIP-8130 replay-ID entries are independent, so a stale-manifest drop
                // must not mark the sender's unrelated entries invalid.
                let skip_mark_invalid = matches!(rejection, GateRejection::ManifestStale { .. })
                    && eip8130_replay_id.is_some();
                reporter.reject(reported, rejection, info, limits, &mut diag);
                if !skip_mark_invalid {
                    source.mark_invalid(tx.signer(), tx.nonce());
                }
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
                            reporter.execution_rejected(
                                reported,
                                &TxnExecutionError::NonceTooLow,
                                info,
                                limits,
                                &mut diag,
                            );
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            reporter.execution_rejected(
                                reported,
                                &TxnExecutionError::InternalError(err.clone()),
                                info,
                                limits,
                                &mut diag,
                            );
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            source.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
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
                num_txs_simulated_success += 1;
                BuilderMetrics::successful_tx_gas_used().record(gas_used as f64);
            } else {
                num_txs_simulated_fail += 1;
                reverted_gas_used += gas_used;
                BuilderMetrics::reverted_tx_gas_used().record(gas_used as f64);
            }

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            if let Some(max_gas_per_txn) = self.builder_config.max_gas_per_txn
                && gas_used > max_gas_per_txn
            {
                reporter.execution_rejected(
                    reported,
                    &TxnExecutionError::MaxGasUsageExceeded,
                    info,
                    limits,
                    &mut diag,
                );
                source.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            info.cumulative_gas_used += gas_used;
            // record tx da size
            info.cumulative_da_bytes_used += tx_da_size;
            // record uncompressed tx size
            info.cumulative_uncompressed_bytes += tx_uncompressed_size;

            reporter.accepted(reported, is_success, gas_used, info, limits);
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
                if self.builder_config.metering_provider.is_enabled() {
                    self.builder_config.metering_provider.mark_included_without_metering(&tx_hash);
                }
            }

            // append sender and transaction to the respective lists
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
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
    use alloy_primitives::{TxKind, U256};
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::BaseTypedTransaction;
    use base_execution_chainspec::BaseChainSpec;
    use reth_chainspec::ChainSpec;
    use reth_primitives_traits::{SealedHeader, WithEncoded};
    use reth_provider::noop::NoopProvider;
    use reth_revm::{State, database::StateProviderDatabase};

    use super::*;
    use crate::test_utils::sign_base_tx;

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
        // Minimal Base chainspec: chain id 901 with all L1 forks through Cancun active at
        // genesis. No inherited rollup forks, so block construction stays on the simplest
        // path. (Mirrors the helper used by the `build_block` tests in `payload.rs`.)
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
        let mut ctx = BasePayloadBuilderCtx::for_test(chain_spec, parent);
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
