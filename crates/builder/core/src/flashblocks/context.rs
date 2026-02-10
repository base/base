use core::fmt::Debug;
use std::{sync::Arc, time::Instant};

use alloy_consensus::{Eip658Value, Transaction};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::Database;
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::{BlockHash, Bytes, U256};
use alloy_rpc_types_eth::Withdrawals;
use base_access_lists::FBALBuilderDb;
use op_alloy_consensus::OpDepositReceipt;
use op_revm::OpSpecId;
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    ConfigureEvm, Evm, EvmEnv, EvmError, InvalidTxError, eth::receipt_builder::ReceiptBuilderCtx,
    op_revm::L1BlockInfo,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpPayloadBuilderAttributes;
use reth_optimism_payload_builder::{
    config::{OpDAConfig, OpGasLimitConfig},
    error::OpPayloadBuilderError,
};
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_optimism_txpool::estimated_da_size::DataAvailabilitySized;
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
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ExecutionMeteringMode,
    PayloadTxsBounds, ResourceLimits, TxData, TxDataStore, TxResources, TxnExecutionError,
    TxnOutcome,
};

/// Records the priority fee of a rejected transaction with the given reason as a label.
fn record_rejected_tx_priority_fee(reason: &TxnExecutionError, priority_fee: f64) {
    let r = match reason {
        TxnExecutionError::TransactionDASizeExceeded(_, _) => "tx_da_size_exceeded",
        TxnExecutionError::BlockDASizeExceeded { .. } => "block_da_size_exceeded",
        TxnExecutionError::DAFootprintLimitExceeded { .. } => "da_footprint_limit_exceeded",
        TxnExecutionError::TransactionGasLimitExceeded { .. } => "transaction_gas_limit_exceeded",
        TxnExecutionError::ExecutionMeteringLimitExceeded(inner) => match inner {
            ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) => {
                "tx_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                "flashblock_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::TransactionStateRootTime(_, _) => {
                "tx_state_root_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::BlockStateRootTime(_, _, _) => {
                "block_state_root_time_exceeded"
            }
        },
        _ => "unknown",
    };
    reth_metrics::metrics::histogram!("base_builder_rejected_tx_priority_fee", "reason" => r)
        .record(priority_fee);
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
    /// Target state root time for the current flashblock in microseconds
    pub target_state_root_time_for_batch_us: Option<u128>,
    /// Gas limit per flashblock
    pub gas_per_batch: u64,
    /// DA bytes limit per flashblock
    pub da_per_batch: Option<u64>,
    /// DA footprint limit per flashblock
    pub da_footprint_per_batch: Option<u64>,
    /// Execution time limit per flashblock in microseconds
    pub execution_time_per_batch_us: Option<u128>,
    /// State root time limit per flashblock in microseconds
    pub state_root_time_per_batch_us: Option<u128>,
    /// Whether to disable state root calculation for each flashblock
    pub disable_state_root: bool,
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
        target_state_root_time_for_batch_us: Option<u128>,
    ) -> Self {
        Self {
            flashblock_index: self.flashblock_index + 1,
            target_gas_for_batch,
            target_da_for_batch,
            target_da_footprint_for_batch,
            target_execution_time_for_batch_us,
            target_state_root_time_for_batch_us,
            ..self
        }
    }
}

/// Container type that holds all the necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: OpEvmConfig,
    /// The DA config for the payload builder
    pub da_config: OpDAConfig,
    // Gas limit configuration for the payload builder
    pub gas_limit_config: OpGasLimitConfig,
    /// The chainspec
    pub chain_spec: Arc<OpChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<OpTransactionSigned>>,
    /// Evm Settings
    pub evm_env: EvmEnv<OpSpecId>,
    /// Block env attributes for the current block.
    pub block_env_attributes: OpNextBlockEnvAttributes,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// The metrics for the builder
    pub metrics: Arc<BuilderMetrics>,
    /// Extra context for the payload builder
    pub extra: FlashblocksExtraCtx,
    /// Max gas that can be used by a transaction.
    pub max_gas_per_txn: Option<u64>,
    /// Max execution time per transaction in microseconds.
    pub max_execution_time_per_tx_us: Option<u128>,
    /// Max state root calculation time per transaction in microseconds.
    pub max_state_root_time_per_tx_us: Option<u128>,
    /// Flashblock-level execution time budget in microseconds.
    pub flashblock_execution_time_budget_us: Option<u128>,
    /// Block-level state root calculation time budget in microseconds.
    pub block_state_root_time_budget_us: Option<u128>,
    /// Execution metering mode: off, dry-run, or enforce.
    pub execution_metering_mode: ExecutionMeteringMode,
    /// Transaction data store for resource metering
    pub tx_data_store: TxDataStore,
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
    pub(super) const fn attributes(&self) -> &OpPayloadBuilderAttributes<OpTransactionSigned> {
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
        self.gas_limit_config.gas_limit().unwrap_or_else(|| {
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
        ctx: ReceiptBuilderCtx<'_, OpTransactionSigned, E>,
        deposit_nonce: Option<u64>,
    ) -> OpReceipt {
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

                receipt_builder.build_deposit_receipt(OpDepositReceipt {
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
                    OpPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Convert the transaction to a [Recovered<TransactionSigned>]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx.value().try_clone_into_recovered().map_err(|_| {
                PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
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
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the receipt
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
                error!("Failed to finalize FBALBuilder: {}", err);
            }
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_best_transactions(
        &self,
        info: &mut ExecutionInfo,
        db: &mut State<impl Database>,
        best_txs: &mut impl PayloadTxsBounds,
        block_gas_limit: u64,
        block_da_limit: Option<u64>,
        block_da_footprint_limit: Option<u64>,
        flashblock_execution_time_limit_us: Option<u128>,
        block_state_root_time_limit_us: Option<u128>,
    ) -> Result<Option<()>, PayloadBuilderError> {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;
        let mut reverted_gas_used = 0;
        let base_fee = self.base_fee();
        let tx_da_limit = self.da_config.max_da_tx_size();

        let mut fbal_db = FBALBuilderDb::new(&mut *db);
        let min_tx_index = info.executed_transactions.len() as u64;
        fbal_db.set_index(min_tx_index);
        let mut evm = self.evm_config.evm_with_env(&mut fbal_db, self.evm_env.clone());

        // Build resource limits struct for limit checking
        let limits = ResourceLimits {
            block_gas_limit,
            tx_data_limit: tx_da_limit,
            block_data_limit: block_da_limit,
            da_footprint_gas_scalar: info.da_footprint_scalar,
            block_da_footprint_limit,
            tx_execution_time_limit_us: self.max_execution_time_per_tx_us,
            flashblock_execution_time_limit_us,
            tx_state_root_time_limit_us: self.max_state_root_time_per_tx_us,
            block_state_root_time_limit_us,
        };

        debug!(
            target: "payload_builder",
            message = "Executing best transactions",
            block_da_limit = ?block_da_limit,
            tx_da_limit = ?tx_da_limit,
            block_gas_limit = ?block_gas_limit,
            flashblock_execution_time_limit_us = ?flashblock_execution_time_limit_us,
            block_state_root_time_limit_us = ?block_state_root_time_limit_us,
            execution_metering_mode = ?self.execution_metering_mode,
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

            let TxData { metering: resource_usage, .. } = self.tx_data_store.get(&tx_hash);

            // Extract predicted execution and state root times from metering data
            let predicted_execution_time_us =
                resource_usage.as_ref().map(|m| m.total_execution_time_us);
            let predicted_state_root_time_us =
                resource_usage.as_ref().map(|m| m.state_root_time_us);

            // Build tx resources struct
            let tx_resources = TxResources {
                da_size: tx_da_size,
                gas_limit: tx.gas_limit(),
                execution_time_us: predicted_execution_time_us,
                state_root_time_us: predicted_state_root_time_us,
            };

            // ensure we still have capacity for this transaction
            if let Err(err) = info.is_tx_over_limits(&tx_resources, &limits) {
                // Check if this is an execution metering limit that should be handled
                // according to the metering mode (dry-run vs enforce)
                if let TxnExecutionError::ExecutionMeteringLimitExceeded(ref limit_err) = err {
                    // Record metrics for the exceeded limit
                    self.record_execution_metering_limit_exceeded(limit_err);

                    if self.execution_metering_mode.is_dry_run() {
                        // In dry-run mode, log but don't reject
                        warn!(
                            target: "payload_builder",
                            message = "Execution metering limit would reject transaction (dry-run mode)",
                            tx_hash = ?tx_hash,
                            limit = %limit_err,
                        );
                        // Don't skip - continue to simulate the transaction
                    } else {
                        // Enforce mode: reject the transaction
                        let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                        record_rejected_tx_priority_fee(&err, priority_fee);

                        log_txn(Err(err));
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        continue;
                    }
                } else {
                    // DA size limits, DA footprint, and gas limits are always enforced
                    self.record_static_limit_exceeded(&err);

                    let priority_fee = tx.effective_tip_per_gas(base_fee).unwrap_or(0) as f64;
                    record_rejected_tx_priority_fee(&err, priority_fee);

                    log_txn(Err(err));
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
            }

            // Record execution time prediction accuracy metrics
            if let Some(predicted_us) = predicted_execution_time_us {
                self.metrics.tx_predicted_execution_time_us.record(predicted_us as f64);
            }
            if let Some(predicted_us) = predicted_state_root_time_us {
                self.metrics.tx_predicted_state_root_time_us.record(predicted_us as f64);
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                log_txn(Err(TxnExecutionError::SequencerTransaction));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            let tx_simulation_start_time = Instant::now();
            let ResultAndState { result, state } = match evm.transact(&tx) {
                Ok(res) => res,
                Err(err) => {
                    if let Some(err) = err.as_invalid_tx_err() {
                        if err.is_nonce_too_low() {
                            // if the nonce is too low, we can skip this transaction
                            log_txn(Err(TxnExecutionError::NonceTooLow));
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            log_txn(Err(TxnExecutionError::InternalError(err.clone())));
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

            self.metrics.tx_simulation_duration.record(tx_simulation_start_time.elapsed());
            self.metrics.tx_byte_size.record(tx.inner().size() as f64);
            self.metrics.tx_actual_execution_time_us.record(actual_execution_time_us as f64);
            num_txs_simulated += 1;

            // Record prediction accuracy
            if let Some(predicted_us) = predicted_execution_time_us {
                let error = predicted_us as f64 - actual_execution_time_us as f64;
                self.metrics.execution_time_prediction_error_us.record(error);
            }

            let gas_used = result.gas_used();
            let is_success = result.is_success();
            if is_success {
                log_txn(Ok(TxnOutcome::Success));
                num_txs_simulated_success += 1;
                self.metrics.successful_tx_gas_used.record(gas_used as f64);
            } else {
                log_txn(Ok(TxnOutcome::Reverted));
                num_txs_simulated_fail += 1;
                reverted_gas_used += gas_used as i32;
                self.metrics.reverted_tx_gas_used.record(gas_used as f64);
            }

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            if let Some(max_gas_per_txn) = self.max_gas_per_txn
                && gas_used > max_gas_per_txn
            {
                log_txn(Err(TxnExecutionError::MaxGasUsageExceeded));
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            info.cumulative_gas_used += gas_used;
            // record tx da size
            info.cumulative_da_bytes_used += tx_da_size;
            // record execution time (use predicted time if available, fall back to actual)
            info.flashblock_execution_time_us +=
                predicted_execution_time_us.unwrap_or(actual_execution_time_us);
            // record state root time (only from predictions)
            if let Some(state_root_time) = predicted_state_root_time_us {
                info.cumulative_state_root_time_us += state_root_time;

                // Record state root time / gas ratio for anomaly detection
                if gas_used > 0 {
                    let ratio = state_root_time as f64 / gas_used as f64;
                    self.metrics.state_root_time_per_gas_ratio.record(ratio);
                }
            }

            // Push transaction changeset and calculate header bloom filter for receipt.
            let ctx = ReceiptBuilderCtx {
                tx: tx.inner(),
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

            // append sender and transaction to the respective lists
            // and increment the next txn index for the access list
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

        // Record cumulative predicted state root time for the block
        if info.cumulative_state_root_time_us > 0 {
            self.metrics
                .block_predicted_state_root_time_us
                .record(info.cumulative_state_root_time_us as f64);
        }

        let payload_transaction_simulation_time = execute_txs_start_time.elapsed();
        self.metrics.set_payload_builder_metrics(
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

    /// Record metrics for a limit that can be evaluated via static analysis (always enforced).
    fn record_static_limit_exceeded(&self, err: &TxnExecutionError) {
        match err {
            TxnExecutionError::TransactionDASizeExceeded(_, _) => {
                self.metrics.tx_da_size_exceeded_total.increment(1);
            }
            TxnExecutionError::BlockDASizeExceeded { .. } => {
                self.metrics.block_da_size_exceeded_total.increment(1);
            }
            TxnExecutionError::DAFootprintLimitExceeded { .. } => {
                self.metrics.da_footprint_exceeded_total.increment(1);
            }
            TxnExecutionError::TransactionGasLimitExceeded { .. } => {
                self.metrics.gas_limit_exceeded_total.increment(1);
            }
            _ => {}
        }
    }

    /// Record metrics for a limit that requires execution data (enforcement is configurable).
    fn record_execution_metering_limit_exceeded(&self, limit: &ExecutionMeteringLimitExceeded) {
        self.metrics.resource_limit_would_reject_total.increment(1);
        match limit {
            ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) => {
                self.metrics.tx_execution_time_exceeded_total.increment(1);
            }
            ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                self.metrics.flashblock_execution_time_exceeded_total.increment(1);
            }
            ExecutionMeteringLimitExceeded::TransactionStateRootTime(_, _) => {
                self.metrics.tx_state_root_time_exceeded_total.increment(1);
            }
            ExecutionMeteringLimitExceeded::BlockStateRootTime(_, _, _) => {
                self.metrics.block_state_root_time_exceeded_total.increment(1);
            }
        }
    }
}
