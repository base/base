use alloy_consensus::{
    conditional::BlockConditionalAttributes, Eip658Value, Transaction, TxEip1559,
};
use alloy_eips::{eip7623::TOTAL_COST_FLOOR_PER_TOKEN, Encodable2718, Typed2718};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_rpc_types_eth::Withdrawals;
use core::fmt::Debug;
use op_alloy_consensus::{OpDepositReceipt, OpTypedTransaction};
use op_revm::OpSpecId;
use reth::payload::PayloadBuilderAttributes;
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    eth::receipt_builder::ReceiptBuilderCtx, ConfigureEvm, Evm, EvmEnv, EvmError, InvalidTxError,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpPayloadBuilderAttributes;
use reth_optimism_payload_builder::{config::OpDAConfig, error::OpPayloadBuilderError};
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_optimism_txpool::{
    conditional::MaybeConditionalTransaction,
    estimated_da_size::DataAvailabilitySized,
    interop::{is_valid_interop, MaybeInteropTransaction},
};
use reth_payload_builder::PayloadId;
use reth_primitives::{Recovered, SealedHeader};
use reth_primitives_traits::{InMemorySize, SignedTransaction};
use reth_provider::ProviderError;
use reth_revm::{context::Block, State};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction};
use revm::{context::result::ResultAndState, Database, DatabaseCommit};
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::{
    metrics::OpRBuilderMetrics,
    primitives::reth::{ExecutionInfo, TxnExecutionResult},
    traits::PayloadTxsBounds,
    tx::MaybeRevertingTransaction,
    tx_signer::Signer,
};

/// Container type that holds all necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: OpEvmConfig,
    /// The DA config for the payload builder
    pub da_config: OpDAConfig,
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
    /// The builder signer
    pub builder_signer: Option<Signer>,
    /// The metrics for the builder
    pub metrics: Arc<OpRBuilderMetrics>,
}

impl OpPayloadBuilderCtx {
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes<OpTransactionSigned> {
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
        self.attributes()
            .gas_limit
            .unwrap_or(self.evm_env.block_env.gas_limit)
    }

    /// Returns the block number for the block.
    pub fn block_number(&self) -> u64 {
        self.evm_env.block_env.number
    }

    /// Returns the current base fee
    pub fn base_fee(&self) -> u64 {
        self.evm_env.block_env.basefee
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.evm_env
            .block_env
            .blob_gasprice()
            .map(|gasprice| gasprice as u64)
    }

    /// Returns the blob fields for the header.
    ///
    /// This will always return `Some(0)` after ecotone.
    pub fn blob_fields(&self) -> (Option<u64>, Option<u64>) {
        // OP doesn't support blobs/EIP-4844.
        // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
        // Need [Some] or [None] based on hardfork to match block hash.
        if self.is_ecotone_active() {
            (Some(0), Some(0))
        } else {
            (None, None)
        }
    }

    /// Returns the extra data for the block.
    ///
    /// After holocene this extracts the extradata from the paylpad
    pub fn extra_data(&self) -> Result<Bytes, PayloadBuilderError> {
        if self.is_holocene_active() {
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
        self.chain_spec
            .is_regolith_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if ecotone is active for the payload.
    pub fn is_ecotone_active(&self) -> bool {
        self.chain_spec
            .is_ecotone_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if canyon is active for the payload.
    pub fn is_canyon_active(&self) -> bool {
        self.chain_spec
            .is_canyon_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec
            .is_holocene_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if isthmus is active for the payload.
    pub fn is_isthmus_active(&self) -> bool {
        self.chain_spec
            .is_isthmus_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain_id()
    }

    /// Returns the builder signer
    pub fn builder_signer(&self) -> Option<Signer> {
        self.builder_signer
    }
}

impl OpPayloadBuilderCtx {
    /// Constructs a receipt for the given transaction.
    fn build_receipt<E: Evm>(
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
    pub fn execute_sequencer_transactions<DB, E: Debug + Default>(
        &self,
        db: &mut State<DB>,
    ) -> Result<ExecutionInfo<E>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());

        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

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
            let sequencer_tx = sequencer_tx
                .value()
                .try_clone_into_recovered()
                .map_err(|_| {
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
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_inner());
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    pub fn execute_best_transactions<DB, E: Debug + Default>(
        &self,
        info: &mut ExecutionInfo<E>,
        db: &mut State<DB>,
        mut best_txs: impl PayloadTxsBounds,
        block_gas_limit: u64,
        block_da_limit: Option<u64>,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;
        let base_fee = self.base_fee();
        let tx_da_limit = self.da_config.max_da_tx_size();
        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        info!(target: "payload_builder", block_da_limit = ?block_da_limit, tx_da_size = ?tx_da_limit, block_gas_limit = ?block_gas_limit, "DA limits");

        // Remove once we merge Reth 1.4.4
        // Fixed in https://github.com/paradigmxyz/reth/pull/16514
        self.metrics
            .da_block_size_limit
            .set(block_da_limit.map_or(-1.0, |v| v as f64));
        self.metrics
            .da_tx_size_limit
            .set(tx_da_limit.map_or(-1.0, |v| v as f64));

        let block_attr = BlockConditionalAttributes {
            number: self.block_number(),
            timestamp: self.attributes().timestamp(),
        };

        while let Some(tx) = best_txs.next(()) {
            let interop = tx.interop_deadline();
            let reverted_hashes = tx.reverted_hashes().clone();
            let conditional = tx.conditional().cloned();

            let tx_da_size = tx.estimated_da_size();
            let tx = tx.into_consensus();
            let tx_hash = tx.tx_hash();

            // exclude reverting transaction if:
            // - the transaction comes from a bundle (is_some) and the hash **is not** in reverted hashes
            // Note that we need to use the Option to signal whether the transaction comes from a bundle,
            // otherwise, we would exclude all transactions that are not in the reverted hashes.
            let exclude_reverting_txs =
                reverted_hashes.is_some() && !reverted_hashes.unwrap().contains(&tx_hash);

            let log_txn = |result: TxnExecutionResult| {
                debug!(target: "payload_builder", tx_hash = ?tx_hash, tx_da_size = ?tx_da_size, exclude_reverting_txs = ?exclude_reverting_txs, result = %result, "Considering transaction");
            };

            if let Some(conditional) = conditional {
                // TODO: ideally we should get this from the txpool stream
                if !conditional.matches_block_attributes(&block_attr) {
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
            }

            // TODO: remove this condition and feature once we are comfortable enabling interop for everything
            if cfg!(feature = "interop") {
                // We skip invalid cross chain txs, they would be removed on the next block update in
                // the maintenance job
                if let Some(interop) = interop {
                    if !is_valid_interop(interop, self.config.attributes.timestamp()) {
                        log_txn(TxnExecutionResult::InteropFailed);
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        continue;
                    }
                }
            }

            num_txs_considered += 1;
            // ensure we still have capacity for this transaction
            if let Err(result) = info.is_tx_over_limits(
                tx_da_size,
                block_gas_limit,
                tx_da_limit,
                block_da_limit,
                tx.gas_limit(),
            ) {
                // we can't fit this transaction into the block, so we need to mark it as
                // invalid which also removes all dependent transaction from
                // the iterator before we can continue
                log_txn(result);
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                log_txn(TxnExecutionResult::SequencerTransaction);
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
                            log_txn(TxnExecutionResult::NonceTooLow);
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            log_txn(TxnExecutionResult::InternalError(err.clone()));
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    log_txn(TxnExecutionResult::EvmError);
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            self.metrics
                .tx_simulation_duration
                .record(tx_simulation_start_time.elapsed());
            self.metrics.tx_byte_size.record(tx.inner().size() as f64);
            num_txs_simulated += 1;
            if result.is_success() {
                log_txn(TxnExecutionResult::Success);
                num_txs_simulated_success += 1;
            } else {
                num_txs_simulated_fail += 1;
                if exclude_reverting_txs {
                    log_txn(TxnExecutionResult::RevertedAndExcluded);
                    info!(target: "payload_builder", tx_hash = ?tx.tx_hash(), "skipping reverted transaction");
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                } else {
                    log_txn(TxnExecutionResult::Reverted);
                }
            }

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            let gas_used = result.gas_used();
            info.cumulative_gas_used += gas_used;
            // record tx da size
            info.cumulative_da_bytes_used += tx_da_size;

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
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_inner());
        }

        self.metrics
            .payload_tx_simulation_duration
            .record(execute_txs_start_time.elapsed());
        self.metrics
            .payload_num_tx_considered
            .record(num_txs_considered as f64);
        self.metrics
            .payload_num_tx_simulated
            .record(num_txs_simulated as f64);
        self.metrics
            .payload_num_tx_simulated_success
            .record(num_txs_simulated_success as f64);
        self.metrics
            .payload_num_tx_simulated_fail
            .record(num_txs_simulated_fail as f64);

        Ok(None)
    }

    pub fn add_builder_tx<DB, Extra: Debug + Default>(
        &self,
        info: &mut ExecutionInfo<Extra>,
        db: &mut State<DB>,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<()>
    where
        DB: Database<Error = ProviderError>,
    {
        self.builder_signer()
            .map(|signer| {
                let base_fee = self.base_fee();
                let chain_id = self.chain_id();
                // Create and sign the transaction
                let builder_tx =
                    signed_builder_tx(db, builder_tx_gas, message, signer, base_fee, chain_id)?;

                let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

                let ResultAndState { result, state } = evm
                    .transact(&builder_tx)
                    .map_err(|err| PayloadBuilderError::EvmExecutionError(Box::new(err)))?;

                // Add gas used by the transaction to cumulative gas used, before creating the receipt
                let gas_used = result.gas_used();
                info.cumulative_gas_used += gas_used;

                let ctx = ReceiptBuilderCtx {
                    tx: builder_tx.inner(),
                    evm: &evm,
                    result,
                    state: &state,
                    cumulative_gas_used: info.cumulative_gas_used,
                };
                info.receipts.push(self.build_receipt(ctx, None));

                // Release the db reference by dropping evm
                drop(evm);
                // Commit changes
                db.commit(state);

                // Append sender and transaction to the respective lists
                info.executed_senders.push(builder_tx.signer());
                info.executed_transactions.push(builder_tx.into_inner());
                Ok(())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }

    /// Calculates EIP 2718 builder transaction size
    // TODO: this function could be improved, ideally we shouldn't take mut ref to db and maybe
    // it's possible to do this without db at all
    pub fn estimate_builder_tx_da_size<DB>(
        &self,
        db: &mut State<DB>,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<u64>
    where
        DB: Database<Error = ProviderError>,
    {
        self.builder_signer()
            .map(|signer| {
                let base_fee = self.base_fee();
                let chain_id = self.chain_id();
                // Create and sign the transaction
                let builder_tx =
                    signed_builder_tx(db, builder_tx_gas, message, signer, base_fee, chain_id)?;
                Ok(op_alloy_flz::tx_estimated_size_fjord_bytes(
                    builder_tx.encoded_2718().as_slice(),
                ))
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }
}

pub fn estimate_gas_for_builder_tx(input: Vec<u8>) -> u64 {
    // Count zero and non-zero bytes
    let (zero_bytes, nonzero_bytes) = input.iter().fold((0, 0), |(zeros, nonzeros), &byte| {
        if byte == 0 {
            (zeros + 1, nonzeros)
        } else {
            (zeros, nonzeros + 1)
        }
    });

    // Calculate gas cost (4 gas per zero byte, 16 gas per non-zero byte)
    let zero_cost = zero_bytes * 4;
    let nonzero_cost = nonzero_bytes * 16;

    // Tx gas should be not less than floor gas https://eips.ethereum.org/EIPS/eip-7623
    let tokens_in_calldata = zero_bytes + nonzero_bytes * 4;
    let floor_gas = 21_000 + tokens_in_calldata * TOTAL_COST_FLOOR_PER_TOKEN;

    std::cmp::max(zero_cost + nonzero_cost + 21_000, floor_gas)
}

/// Creates signed builder tx to Address::ZERO and specified message as input
pub fn signed_builder_tx<DB>(
    db: &mut State<DB>,
    builder_tx_gas: u64,
    message: Vec<u8>,
    signer: Signer,
    base_fee: u64,
    chain_id: u64,
) -> Result<Recovered<OpTransactionSigned>, PayloadBuilderError>
where
    DB: Database<Error = ProviderError>,
{
    // Create message with block number for the builder to sign
    let nonce = db
        .load_cache_account(signer.address)
        .map(|acc| acc.account_info().unwrap_or_default().nonce)
        .map_err(|_| {
            PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(signer.address))
        })?;

    // Create the EIP-1559 transaction
    let tx = OpTypedTransaction::Eip1559(TxEip1559 {
        chain_id,
        nonce,
        gas_limit: builder_tx_gas,
        max_fee_per_gas: base_fee.into(),
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(Address::ZERO),
        // Include the message as part of the transaction data
        input: message.into(),
        ..Default::default()
    });
    // Sign the transaction
    let builder_tx = signer.sign_tx(tx).map_err(PayloadBuilderError::other)?;

    Ok(builder_tx)
}
