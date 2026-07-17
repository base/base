//! Outcome reporting for the flashblock build loop.
//!
//! [`OutcomeReporter`] is the single layer that turns build-loop decisions into their observable
//! side effects: builder transaction events, rejection diagnostics, metrics, and the rejected-tx
//! audit trail. Gates return a [`GateRejection`](super::gates::GateRejection) and the build walk
//! delegates every emission/recording to this reporter, so no other layer emits events. A future
//! closed build walk supplies its own reporter with its own event vocabulary while reusing the
//! same gates.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::TxHash;
use base_bundles::{MeterBundleResponse, RejectedTransaction, RejectionReason};
use base_observability_events::TransactionEventType;
use serde::Serialize;
use tracing::{debug, trace};

use super::{
    context::{BasePayloadBuilderCtx, FlashblockDiagnostics},
    gates::GateRejection,
};
use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources,
    TxnExecutionError, TxnOutcome,
    transaction_events::{
        BuilderAcceptedEventData, BuilderBudgetFields, BuilderRejectedEventData,
        BuilderTransactionEventContext, emit_builder_transaction_event, rejection_reason_code,
    },
};

/// A candidate transaction as seen by outcome reporting.
///
/// Bundles the per-candidate fields shared across a candidate's considered/rejected/accepted
/// reports: its hash, one-based scan position, estimated resources (absent before the candidate is
/// materialized), and priority fee (used as the value for rejection metrics).
#[derive(Debug, Clone, Copy)]
pub struct ReportedTx<'a> {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// One-based ordering position within this scan.
    pub ordering_position: u64,
    /// Estimated resources for the candidate, if known.
    pub resources: Option<&'a TxResources>,
    /// Effective priority fee (tip per gas), used as the value for rejection metrics.
    pub priority_fee: f64,
}

/// Records the observable outcomes of build-loop decisions: transaction events, rejection
/// diagnostics, metrics, and the rejected-tx audit trail.
///
/// Holds the payload builder context (for configuration, block identity, and the audit channel)
/// and the payload id used as the event join key.
#[derive(Debug, Clone, Copy)]
pub struct OutcomeReporter<'a> {
    ctx: &'a BasePayloadBuilderCtx,
    payload_id: &'a str,
}

impl<'a> OutcomeReporter<'a> {
    /// Creates a reporter for the given context and payload id.
    pub const fn new(ctx: &'a BasePayloadBuilderCtx, payload_id: &'a str) -> Self {
        Self { ctx, payload_id }
    }

    /// Emits a builder transaction event through the global event writer.
    fn emit<D, F>(
        &self,
        event_type: TransactionEventType,
        tx_hash: TxHash,
        ordering_position: u64,
        data: F,
    ) where
        D: Serialize,
        F: FnOnce() -> D,
    {
        emit_builder_transaction_event(
            BuilderTransactionEventContext {
                payload_id: self.payload_id.to_string(),
                block_number: self.ctx.block_number(),
                block_hash: None,
                parent_hash: self.ctx.parent_hash(),
                flashblock_index: Some(self.ctx.flashblock_index()),
                target_flashblock_count: self.ctx.target_flashblock_count(),
                ordering_position: Some(ordering_position),
                builder_mode: "flashblocks",
                source_queue: "txpool_best",
            },
            event_type,
            tx_hash,
            data,
        );
    }

    /// Reports that the builder has begun evaluating a candidate, emitting the considered event.
    ///
    /// Emitted up front, before the gate chain runs, so its timestamp marks the start of
    /// evaluation rather than a successful admission.
    pub fn considered(&self, tx: ReportedTx<'_>, info: &ExecutionInfo, limits: &ResourceLimits) {
        self.emit(
            TransactionEventType::BuilderConsidered,
            tx.tx_hash,
            tx.ordering_position,
            || BuilderBudgetFields::new(info, limits, tx.resources),
        );
    }

    /// Reports a gate rejection: emits the rejected event and records its diagnostics, metrics,
    /// audit-trail entry, and per-candidate trace.
    ///
    /// The candidate's [`considered`](Self::considered) event was already emitted up front, so this
    /// emits only the rejected event — carrying the decision-specific context (bundle window,
    /// metering wait, dry-run) that is not present on the considered event. Takes the rejection by
    /// value so it can move the error and the metering response out rather than cloning them on the
    /// per-candidate hot path.
    pub fn reject(
        &self,
        tx: ReportedTx<'_>,
        rejection: GateRejection,
        info: &mut ExecutionInfo,
        limits: &ResourceLimits,
        diag: &mut FlashblockDiagnostics,
    ) {
        match rejection {
            GateRejection::BundleWrongTarget { target, current } => {
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::new(
                            "wrong_target_block",
                            "bundle target block does not match current block",
                            false,
                            info,
                            limits,
                            tx.resources,
                        )
                        .with_bundle_target_block(target)
                        .with_current_block(current)
                    },
                );
            }
            GateRejection::BundleExpired { block_timestamp } => {
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::new(
                            "bundle_expired",
                            "bundle validity window expired",
                            false,
                            info,
                            limits,
                            tx.resources,
                        )
                        .with_block_timestamp(block_timestamp)
                    },
                );
            }
            GateRejection::BundleNotYetValid { block_timestamp } => {
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::new(
                            "bundle_not_yet_valid",
                            "bundle validity window has not started",
                            false,
                            info,
                            limits,
                            tx.resources,
                        )
                        .with_block_timestamp(block_timestamp)
                    },
                );
            }
            GateRejection::MeteringPending { tx_age_ms, wait_duration_ms } => {
                let err = TxnExecutionError::MeteringDataPending;
                diag.record_rejection(&err);
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::from_error(&err, info, limits, tx.resources)
                            .with_metering_wait(tx_age_ms, wait_duration_ms)
                    },
                );
                BuilderMetrics::metering_data_pending_skip().increment(1);
                self.log_outcome(tx, Err(&err));
            }
            GateRejection::Limit(err) => self.reject_with_error(tx, &err, info, limits, diag),
            GateRejection::Sequencer => self.reject_with_error(
                tx,
                &TxnExecutionError::SequencerTransaction,
                info,
                limits,
                diag,
            ),
            GateRejection::ManifestStale { cause } => {
                diag.txs_rejected_other += 1;
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::new(
                            "manifest_precheck_stale",
                            cause,
                            false,
                            info,
                            limits,
                            tx.resources,
                        )
                    },
                );
                trace!(
                    target: "payload_builder",
                    tx_hash = ?tx.tx_hash,
                    cause,
                    "skipping EIP-8130 transaction with stale authorization manifest"
                );
            }
            GateRejection::MeteringLimit { limit, resource_usage } => {
                let ExecutionMeteringLimitExceeded::TransactionExecutionTime(tx_time_us, limit_us) =
                    limit;
                let err = TxnExecutionError::ExecutionMeteringLimitExceeded(
                    ExecutionMeteringLimitExceeded::TransactionExecutionTime(tx_time_us, limit_us),
                );
                diag.record_rejection(&err);
                Self::record_rejected_tx_priority_fee(&err, tx.priority_fee);
                if err.is_permanent() {
                    diag.permanently_rejected_txs.push(tx.tx_hash);
                }
                // Only record per-tx execution time limits for the audit trail for now. Unbox and
                // move the metering response in rather than cloning it.
                self.record_rejected_tx(
                    info,
                    tx.tx_hash,
                    RejectionReason::ExecutionTimeExceeded { tx_time_us, limit_us },
                    resource_usage.map(|usage| *usage).unwrap_or_default(),
                );
                self.emit(
                    TransactionEventType::BuilderRejected,
                    tx.tx_hash,
                    tx.ordering_position,
                    || {
                        BuilderRejectedEventData::from_error(&err, info, limits, tx.resources)
                            .with_dry_run(false)
                    },
                );
                self.log_outcome(tx, Err(&err));
            }
        }
    }

    /// Reports a rejection discovered during execution (nonce-too-low, invalid, max-gas).
    ///
    /// The considered event was already emitted, so this emits the rejected event and records the
    /// rejection's diagnostics, metrics, and trace — the error arrives as its own
    /// [`TxnExecutionError`] rather than a gate decision.
    pub fn execution_rejected(
        &self,
        tx: ReportedTx<'_>,
        err: &TxnExecutionError,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        diag: &mut FlashblockDiagnostics,
    ) {
        self.reject_with_error(tx, err, info, limits, diag);
    }

    /// Records and emits an error-typed rejection: rejection diagnostics, the static-limit and
    /// priority-fee metrics, the permanent-rejection cache, the rejected event, and the trace.
    /// Shared by the hard-limit and sequencer gate rejections and the execution-discovered ones.
    fn reject_with_error(
        &self,
        tx: ReportedTx<'_>,
        err: &TxnExecutionError,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        diag: &mut FlashblockDiagnostics,
    ) {
        diag.record_rejection(err);
        Self::record_static_limit_exceeded(err);
        Self::record_rejected_tx_priority_fee(err, tx.priority_fee);
        if err.is_permanent() {
            diag.permanently_rejected_txs.push(tx.tx_hash);
        }
        self.emit(TransactionEventType::BuilderRejected, tx.tx_hash, tx.ordering_position, || {
            BuilderRejectedEventData::from_error(err, info, limits, tx.resources)
        });
        self.log_outcome(tx, Err(err));
    }

    /// Emits the per-candidate `"Considering transaction"` debug trace with its terminal outcome.
    fn log_outcome(&self, tx: ReportedTx<'_>, result: Result<TxnOutcome, &TxnExecutionError>) {
        let result_str = match result {
            Ok(outcome) => outcome.to_string(),
            Err(err) => err.to_string(),
        };
        debug!(
            target: "payload_builder",
            message = "Considering transaction",
            tx_hash = ?tx.tx_hash,
            tx_da_size = ?tx.resources.map(|r| r.da_size),
            result = %result_str,
        );
    }

    /// Reports a committed transaction, emitting the accepted event and its terminal trace.
    pub fn accepted(
        &self,
        tx: ReportedTx<'_>,
        is_success: bool,
        gas_used: u64,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
    ) {
        let (outcome, execution_outcome) = if is_success {
            (TxnOutcome::Success, "success")
        } else {
            (TxnOutcome::Reverted, "reverted")
        };
        self.emit(TransactionEventType::BuilderAccepted, tx.tx_hash, tx.ordering_position, || {
            BuilderAcceptedEventData::new(execution_outcome, gas_used, info, limits, tx.resources)
        });
        self.log_outcome(tx, Ok(outcome));
    }

    /// Records the priority fee of a rejected transaction with the given reason as a label.
    fn record_rejected_tx_priority_fee(reason: &TxnExecutionError, priority_fee: f64) {
        BuilderMetrics::rejected_tx_priority_fee(rejection_reason_code(reason))
            .record(priority_fee);
    }

    /// Records metrics for a limit that can be evaluated via static analysis (always enforced).
    fn record_static_limit_exceeded(err: &TxnExecutionError) {
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

    /// Pushes a rejected transaction onto the block's audit trail, to be flushed after finalization.
    fn record_rejected_tx(
        &self,
        info: &mut ExecutionInfo,
        tx_hash: TxHash,
        reason: RejectionReason,
        metering: MeterBundleResponse,
    ) {
        if self.ctx.rejected_tx_sender.is_none() {
            return;
        }

        if info.rejected_txs.len() >= self.ctx.builder_config.max_rejected_txs_per_block {
            BuilderMetrics::rejected_tx_per_block_drops().increment(1);
            return;
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        info.rejected_txs.push(RejectedTransaction {
            tx_hash,
            block_number: self.ctx.block_number(),
            reason,
            timestamp: now,
            metering,
        });
    }
}
