/// Transaction executor for sequencer and mempool transactions.
use std::time::Instant;

use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::Database;
use alloy_primitives::U256;
use base_access_lists::FBALBuilderDb;
use reth_evm::{
    ConfigureEvm, Evm, EvmError, InvalidTxError,
    eth::receipt_builder::ReceiptBuilderCtx,
    execute::BlockBuilder,
    op_revm::L1BlockInfo,
};
use alloy_consensus::Transaction;
use reth_node_api::PayloadBuilderError;
use reth_optimism_forks::OpHardforks;
use reth_optimism_payload_builder::error::OpPayloadBuilderError;
use reth_optimism_txpool::estimated_da_size::DataAvailabilitySized;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::{InMemorySize, SignedTransaction};
use reth_provider::ProviderError;
use reth_revm::State;
use reth_transaction_pool::PoolTransaction;
use revm::{DatabaseCommit, context::result::ResultAndState};
use tracing::{debug, error, trace, warn};

use crate::{
    ExecutionInfo, PayloadTxsBounds, ResourceLimits, TxResources,
    context::OpPayloadBuilderCtx,
    execution::{TxnExecutionError, TxnOutcome},
};

/// Executes sequencer and mempool transactions against an EVM state.
///
/// Constructed from an [`OpPayloadBuilderCtx`] to access configuration,
/// limits, metering, and metrics. All execution is stateless with respect
/// to `TxExecutor` itself — state is passed in per call.
#[derive(Debug)]
pub struct TxExecutor<'a> {
    ctx: &'a OpPayloadBuilderCtx,
}

impl<'a> TxExecutor<'a> {
    /// Creates a new [`TxExecutor`] for the given build context.
    pub fn new(ctx: &'a OpPayloadBuilderCtx) -> Self {
        Self { ctx }
    }

    /// Applies pre-execution EVM changes and executes all sequencer transactions.
    ///
    /// This is the entry point for the initial block setup: it applies the
    /// pre-execution state changes (e.g. system calls) and then runs the
    /// sequencer-provided transactions from the payload attributes.
    pub fn execute_pre_steps<DB>(
        &self,
        state: &mut State<DB>,
    ) -> Result<ExecutionInfo, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + std::fmt::Debug + revm::Database,
    {
        self.ctx
            .evm_config
            .builder_for_next_block(state, self.ctx.parent(), self.ctx.block_env_attributes.clone())
            .map_err(PayloadBuilderError::other)?
            .apply_pre_execution_changes()?;
        self.execute_sequencer_transactions(state)
    }

    /// Executes all sequencer transactions from the payload attributes.
    pub fn execute_sequencer_transactions<DB>(
        &self,
        db: &mut State<DB>,
    ) -> Result<ExecutionInfo, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + revm::Database,
    {
        let mut info = ExecutionInfo::with_capacity(self.ctx.attributes().transactions.len());

        let mut fbal_db = FBALBuilderDb::new(&mut *db);
        let min_tx_index = info.executed_transactions.iter().len() as u64;
        fbal_db.set_index(min_tx_index);
        let mut evm = self.ctx.evm_config.evm_with_env(&mut fbal_db, self.ctx.evm_env.clone());

        for sequencer_tx in &self.ctx.attributes().transactions {
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    OpPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            let sequencer_tx =
                sequencer_tx.value().try_clone_into_recovered().map_err(|_| {
                    PayloadBuilderError::other(
                        OpPayloadBuilderError::TransactionEcRecoverFailed,
                    )
                })?;

            let depositor_nonce = (self.ctx.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .db_mut()
                        .load_cache_account(sequencer_tx.signer())
                        .map(|acc| acc.account_info().unwrap_or_default().nonce)
                })
                .transpose()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
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
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            let gas_used = result.gas_used();
            info.cumulative_gas_used += gas_used;

            if !sequencer_tx.is_deposit() {
                info.cumulative_da_bytes_used += op_alloy_flz::tx_estimated_size_fjord_bytes(
                    sequencer_tx.encoded_2718().as_slice(),
                );
            }

            let ctx = ReceiptBuilderCtx {
                tx: sequencer_tx.inner(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };

            info.receipts.push(self.ctx.build_receipt(ctx, depositor_nonce));

            evm.db_mut().commit(state);

            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_inner());
            evm.db_mut().inc_index();
        }

        let da_footprint_gas_scalar = self
            .ctx
            .chain_spec
            .is_jovian_active_at_timestamp(self.ctx.attributes().timestamp())
            .then(|| {
                L1BlockInfo::fetch_da_footprint_gas_scalar(evm.db_mut())
                    .expect("DA footprint should always be available from the database post jovian")
            });

        info.da_footprint_scalar = da_footprint_gas_scalar;

        match fbal_db.finish() {
            Ok(fbal_builder) => info.extra.access_list_builder = fbal_builder,
            Err(err) => {
                error!("Failed to finalize FBALBuilder: {}", err);
            }
        }

        Ok(info)
    }

    /// Executes the best mempool transactions up to the per-flashblock resource limits.
    ///
    /// Limits (gas, DA, execution time, state root time) are read from the current
    /// [`FlashblocksExtraCtx`] on the build context, so the executor always reflects
    /// the limits for the current flashblock without extra arguments.
    ///
    /// Returns `Ok(Some(()))` if the job was cancelled mid-execution.
    pub fn execute_best_transactions(
        &self,
        info: &mut ExecutionInfo,
        db: &mut State<impl Database>,
        best_txs: &mut impl PayloadTxsBounds,
    ) -> Result<Option<()>, PayloadBuilderError> {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;
        let mut reverted_gas_used = 0;
        let base_fee = self.ctx.base_fee();
        let tx_da_limit = self.ctx.da_config.max_da_tx_size();

        let mut fbal_db = FBALBuilderDb::new(&mut *db);
        let min_tx_index = info.executed_transactions.len() as u64;
        fbal_db.set_index(min_tx_index);
        let mut evm = self.ctx.evm_config.evm_with_env(&mut fbal_db, self.ctx.evm_env.clone());

        let limits = ResourceLimits {
            block_gas_limit: self.ctx.extra.target_gas_for_batch.min(self.ctx.block_gas_limit()),
            tx_data_limit: tx_da_limit,
            block_data_limit: self.ctx.extra.target_da_for_batch,
            da_footprint_gas_scalar: info.da_footprint_scalar,
            block_da_footprint_limit: self.ctx.extra.target_da_footprint_for_batch,
            tx_execution_time_limit_us: self.ctx.max_execution_time_per_tx_us,
            flashblock_execution_time_limit_us: self.ctx.extra.execution_time_per_batch_us,
            tx_state_root_time_limit_us: self.ctx.max_state_root_time_per_tx_us,
            block_state_root_time_limit_us: self.ctx.extra.target_state_root_time_for_batch_us,
        };

        debug!(
            target: "payload_builder",
            message = "Executing best transactions",
            block_da_limit = ?limits.block_data_limit,
            tx_da_limit = ?limits.tx_data_limit,
            block_gas_limit = ?limits.block_gas_limit,
            flashblock_execution_time_limit_us = ?limits.flashblock_execution_time_limit_us,
            block_state_root_time_limit_us = ?limits.block_state_root_time_limit_us,
            execution_metering_mode = ?self.ctx.execution_metering_mode,
        );

        while let Some(tx) = best_txs.next(()) {
            let tx_da_size = tx.estimated_da_size();
            let tx = tx.into_consensus();
            let tx_hash = tx.tx_hash();

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

            let resource_usage = self.ctx.metering_provider.get(&tx_hash);

            let predicted_execution_time_us =
                resource_usage.as_ref().map(|m| m.total_execution_time_us);
            let predicted_state_root_time_us =
                resource_usage.as_ref().map(|m| m.state_root_time_us);

            let tx_resources = TxResources {
                da_size: tx_da_size,
                gas_limit: tx.gas_limit(),
                execution_time_us: predicted_execution_time_us,
                state_root_time_us: predicted_state_root_time_us,
            };

            if let Err(err) = info.is_tx_over_limits(&tx_resources, &limits) {
                if let TxnExecutionError::ExecutionMeteringLimitExceeded(ref limit_err) = err {
                    self.ctx.metrics.record_execution_metering_limit_exceeded(limit_err);

                    if self.ctx.execution_metering_mode.is_dry_run() {
                        warn!(
                            target: "payload_builder",
                            message = "Execution metering limit would reject transaction (dry-run mode)",
                            tx_hash = ?tx_hash,
                            limit = %limit_err,
                        );
                    } else {
                        let priority_fee =
                            tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                        self.ctx.metrics.record_rejected_tx_priority_fee(&err, priority_fee);

                        log_txn(Err(err));
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        continue;
                    }
                } else {
                    self.ctx.metrics.record_static_limit_exceeded(&err);

                    let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                    self.ctx.metrics.record_rejected_tx_priority_fee(&err, priority_fee);

                    log_txn(Err(err));
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
            }

            if let Some(predicted_us) = predicted_execution_time_us {
                self.ctx.metrics.tx_predicted_execution_time_us.record(predicted_us as f64);
            }
            if let Some(predicted_us) = predicted_state_root_time_us {
                self.ctx.metrics.tx_predicted_state_root_time_us.record(predicted_us as f64);
            }

            if tx.is_eip4844() || tx.is_deposit() {
                log_txn(Err(TxnExecutionError::SequencerTransaction));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            if self.ctx.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            let tx_simulation_start_time = Instant::now();
            let ResultAndState { result, state } = match evm.transact(&tx) {
                Ok(res) => res,
                Err(err) => {
                    if let Some(err) = err.as_invalid_tx_err() {
                        if err.is_nonce_too_low() {
                            log_txn(Err(TxnExecutionError::NonceTooLow));
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            log_txn(Err(TxnExecutionError::InternalError(err.clone())));
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    log_txn(Err(TxnExecutionError::EvmError));
                    return Err(PayloadBuilderError::evm(err));
                }
            };

            let actual_execution_time_us = tx_simulation_start_time.elapsed().as_micros();

            self.ctx.metrics.tx_simulation_duration.record(tx_simulation_start_time.elapsed());
            self.ctx.metrics.tx_byte_size.record(tx.inner().size() as f64);
            self.ctx.metrics.tx_actual_execution_time_us.record(actual_execution_time_us as f64);
            num_txs_simulated += 1;

            if let Some(predicted_us) = predicted_execution_time_us {
                let error = predicted_us as f64 - actual_execution_time_us as f64;
                self.ctx.metrics.execution_time_prediction_error_us.record(error);
            }

            let gas_used = result.gas_used();
            let is_success = result.is_success();
            if is_success {
                log_txn(Ok(TxnOutcome::Success));
                num_txs_simulated_success += 1;
                self.ctx.metrics.successful_tx_gas_used.record(gas_used as f64);
            } else {
                log_txn(Ok(TxnOutcome::Reverted));
                num_txs_simulated_fail += 1;
                reverted_gas_used += gas_used as i32;
                self.ctx.metrics.reverted_tx_gas_used.record(gas_used as f64);
            }

            if let Some(max_gas_per_txn) = self.ctx.max_gas_per_txn
                && gas_used > max_gas_per_txn
            {
                log_txn(Err(TxnExecutionError::MaxGasUsageExceeded));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            info.cumulative_gas_used += gas_used;
            info.cumulative_da_bytes_used += tx_da_size;
            info.flashblock_execution_time_us +=
                predicted_execution_time_us.unwrap_or(actual_execution_time_us);

            if let Some(state_root_time) = predicted_state_root_time_us {
                info.cumulative_state_root_time_us += state_root_time;

                if gas_used > 0 {
                    let ratio = state_root_time as f64 / gas_used as f64;
                    self.ctx.metrics.state_root_time_per_gas_ratio.record(ratio);
                }
            }

            let ctx = ReceiptBuilderCtx {
                tx: tx.inner(),
                evm: &evm,
                result,
                state: &state,
                cumulative_gas_used: info.cumulative_gas_used,
            };
            info.receipts.push(self.ctx.build_receipt(ctx, None));

            evm.db_mut().commit(state);

            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
            evm.db_mut().inc_index();
        }

        match fbal_db.finish() {
            Ok(fbal_builder) => {
                info.extra.access_list_builder = fbal_builder;
            }
            Err(err) => {
                error!("Failed to finalize FBALBuilder: {}", err);
            }
        }

        if info.cumulative_state_root_time_us > 0 {
            self.ctx
                .metrics
                .block_predicted_state_root_time_us
                .record(info.cumulative_state_root_time_us as f64);
        }

        let payload_transaction_simulation_time = execute_txs_start_time.elapsed();
        self.ctx.metrics.set_payload_builder_metrics(
            payload_transaction_simulation_time,
            num_txs_considered,
            num_txs_simulated,
            num_txs_simulated_success,
            num_txs_simulated_fail,
            reverted_gas_used,
        );

        debug!(
            target: "payload_builder",
            message = "Completed executing best transactions",
            txs_executed = num_txs_considered,
            txs_applied = num_txs_simulated_success,
            txs_rejected = num_txs_simulated_fail,
        );

        Ok(None)
    }
}
