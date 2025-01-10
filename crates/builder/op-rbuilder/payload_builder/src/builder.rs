//! Optimism payload builder implementation with Flashbots bundle support.

use alloy_consensus::{BlockHeader, Header, Transaction, TxEip1559, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::merge::BEACON_NONCE;
use alloy_rpc_types_beacon::events::{PayloadAttributesData, PayloadAttributesEvent};
use alloy_rpc_types_engine::payload::PayloadAttributes;
use op_alloy_consensus::DepositTransaction;
use rbuilder::utils::Signer;
use reth_basic_payload_builder::*;
use reth_chain_state::ExecutedBlock;
use reth_chainspec::ChainSpecProvider;
use reth_evm::{system_calls::SystemCaller, ConfigureEvm, ConfigureEvmEnv, NextBlockEnvAttributes};
use reth_execution_types::ExecutionOutcome;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::calculate_receipt_root_no_memo_optimism;
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_optimism_payload_builder::error::OpPayloadBuilderError;
use reth_payload_builder::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::{
    proofs, Block, BlockBody, Receipt, Transaction as RethTransaction, TransactionSigned, TxType,
};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use reth_trie::HashedPostState;
use revm::{
    db::states::bundle_state::BundleRetention,
    primitives::{
        Address, BlockEnv, CfgEnvWithHandlerCfg, EVMError, EnvWithHandlerCfg, InvalidTransaction,
        ResultAndState, TxKind, U256,
    },
    DatabaseCommit, State,
};
use std::sync::Arc;
use tracing::{debug, error, trace, warn};
use transaction_pool_bundle_ext::{BundlePoolOperations, TransactionPoolBundleExt};

/// Payload builder for OP Stack, which includes bundle support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpRbuilderPayloadBuilder<EvmConfig> {
    /// The rollup's compute pending block configuration option.
    compute_pending_block: bool,
    /// The builder's signer key to use for an end of block tx
    builder_signer: Option<Signer>,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
}

impl<EvmConfig> OpRbuilderPayloadBuilder<EvmConfig> {
    pub const fn new(evm_config: EvmConfig) -> Self {
        Self {
            compute_pending_block: true,
            builder_signer: None,
            evm_config,
        }
    }

    /// Sets the rollup's compute pending block configuration option.
    pub const fn set_compute_pending_block(mut self, compute_pending_block: bool) -> Self {
        self.compute_pending_block = compute_pending_block;
        self
    }

    /// Sets the builder's signer key to use for an end of block tx
    pub const fn set_builder_signer(mut self, builder_signer: Option<Signer>) -> Self {
        self.builder_signer = builder_signer;
        self
    }

    /// Enables the rollup's compute pending block configuration option.
    pub const fn compute_pending_block(self) -> Self {
        self.set_compute_pending_block(true)
    }

    /// Returns the rollup's compute pending block configuration option.
    pub const fn is_compute_pending_block(&self) -> bool {
        self.compute_pending_block
    }

    /// Returns the builder's signer key to use for an end of block tx
    pub fn builder_signer(&self) -> Option<Signer> {
        self.builder_signer.clone()
    }
}

impl<EvmConfig> OpRbuilderPayloadBuilder<EvmConfig>
where
    EvmConfig: ConfigureEvmEnv<Header = Header>,
{
    /// Returns the configured [`CfgEnvWithHandlerCfg`] and [`BlockEnv`] for the targeted payload
    /// (that has the `parent` as its parent).
    pub fn cfg_and_block_env(
        &self,
        config: &PayloadConfig<OpPayloadBuilderAttributes>,
        parent: &Header,
    ) -> Result<(CfgEnvWithHandlerCfg, BlockEnv), EvmConfig::Error> {
        let next_attributes = NextBlockEnvAttributes {
            timestamp: config.attributes.timestamp(),
            suggested_fee_recipient: config.attributes.suggested_fee_recipient(),
            prev_randao: config.attributes.prev_randao(),
        };
        self.evm_config
            .next_cfg_and_block_env(parent, next_attributes)
    }
}

/// Implementation of the [`PayloadBuilder`] trait for [`OpRbuilderPayloadBuilder`].
impl<Pool, Client, EvmConfig> PayloadBuilder<Pool, Client> for OpRbuilderPayloadBuilder<EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
    EvmConfig: ConfigureEvm<Header = Header>,
    Pool: TransactionPoolBundleExt + BundlePoolOperations<Transaction = TransactionSigned>,
{
    type Attributes = OpPayloadBuilderAttributes;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, OpPayloadBuilderAttributes, OpBuiltPayload>,
    ) -> Result<BuildOutcome<OpBuiltPayload>, PayloadBuilderError> {
        let parent_header = args.config.parent_header.clone();

        // Notify our BundleOperations of the new payload attributes event.
        let eth_payload_attributes = args.config.attributes.payload_attributes.clone();
        let payload_attributes = PayloadAttributes {
            timestamp: eth_payload_attributes.timestamp,
            prev_randao: eth_payload_attributes.prev_randao,
            withdrawals: Some(eth_payload_attributes.withdrawals.into_inner()),
            parent_beacon_block_root: eth_payload_attributes.parent_beacon_block_root,
            suggested_fee_recipient: eth_payload_attributes.suggested_fee_recipient,
        };
        let payload_attributes_data = PayloadAttributesData {
            parent_block_number: parent_header.number,
            parent_block_root: parent_header.state_root(),
            parent_block_hash: parent_header.hash(),
            proposal_slot: parent_header.number + 1,
            proposer_index: 0, // Shouldn't be required for core building logic
            payload_attributes,
        };
        let payload_attributes_event = PayloadAttributesEvent {
            version: "placeholder".to_string(),
            data: payload_attributes_data,
        };

        let (cfg_env, block_env) = self
            .cfg_and_block_env(&args.config, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;

        // gas reserved for builder tx
        let builder_tx_gas = self.builder_signer().map_or(0, |_| {
            let message = format!("Block Number: {}", block_env.number);
            estimate_gas_for_builder_tx(message.as_bytes().to_vec())
        });

        if let Err(e) = args.pool.notify_payload_attributes_event(
            payload_attributes_event,
            args.config
                .attributes
                .gas_limit
                .map(|gas_limit| gas_limit - builder_tx_gas),
        ) {
            error!(?e, "Failed to notify payload attributes event!");
        };

        try_build_inner(
            &self.evm_config,
            args,
            cfg_env,
            block_env,
            self.builder_signer(),
            self.compute_pending_block,
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Pool, Client, OpPayloadBuilderAttributes, OpBuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        // we want to await the job that's already in progress because that should be returned as
        // is, there's no benefit in racing another job
        MissingPayloadBehaviour::AwaitInProgress
    }

    // NOTE: this should only be used for testing purposes because this doesn't have access to L1
    // system txs, hence on_missing_payload we return [MissingPayloadBehaviour::AwaitInProgress].
    fn build_empty_payload(
        &self,
        _client: &Client,
        _config: PayloadConfig<Self::Attributes>,
    ) -> Result<OpBuiltPayload, PayloadBuilderError> {
        Err(PayloadBuilderError::MissingPayload)
    }
}

/// Constructs an Optimism payload from the transactions sent through the
/// Payload attributes by the sequencer, and bundles returned by the BundlePool.
///
/// Given build arguments including an Ethereum client, bundle-enabled transaction pool,
/// and configuration, this function creates a transaction payload. Returns
/// a result indicating success with the payload or an error in case of failure.
#[inline]
pub(crate) fn try_build_inner<EvmConfig, Pool, Client>(
    evm_config: &EvmConfig,
    args: BuildArguments<Pool, Client, OpPayloadBuilderAttributes, OpBuiltPayload>,
    initialized_cfg: CfgEnvWithHandlerCfg,
    initialized_block_env: BlockEnv,
    builder_signer: Option<Signer>,
    _compute_pending_block: bool,
) -> Result<BuildOutcome<OpBuiltPayload>, PayloadBuilderError>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
    EvmConfig: ConfigureEvm<Header = Header>,
    Pool: TransactionPoolBundleExt + BundlePoolOperations<Transaction = TransactionSigned>,
{
    let BuildArguments {
        client,
        pool,
        mut cached_reads,
        config,
        cancel,
        best_payload,
    } = args;

    let chain_spec = client.chain_spec();
    let state_provider = client.state_by_block_hash(config.parent_header.hash())?;
    let state = StateProviderDatabase::new(state_provider);
    let mut db = State::builder()
        .with_database(cached_reads.as_db_mut(state))
        .with_bundle_update()
        .build();
    let PayloadConfig {
        parent_header,
        attributes,
        mut extra_data,
    } = config;

    debug!(target: "payload_builder", id=%attributes.payload_attributes.payload_id(), parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");

    let mut cumulative_gas_used = 0;
    let block_gas_limit: u64 = attributes.gas_limit.unwrap_or_else(|| {
        initialized_block_env
            .gas_limit
            .try_into()
            .unwrap_or(chain_spec.max_gas_limit)
    });
    let base_fee = initialized_block_env.basefee.to::<u64>();

    let mut executed_txs = Vec::with_capacity(attributes.transactions.len());
    let mut executed_senders = Vec::with_capacity(attributes.transactions.len());

    let mut total_fees = U256::ZERO;

    let block_number = initialized_block_env.number.to::<u64>();

    let is_regolith =
        chain_spec.is_regolith_active_at_timestamp(attributes.payload_attributes.timestamp);

    // apply eip-4788 pre block contract call
    let mut system_caller = SystemCaller::new(evm_config.clone(), chain_spec.clone());

    system_caller
        .pre_block_beacon_root_contract_call(
            &mut db,
            &initialized_cfg,
            &initialized_block_env,
            attributes.payload_attributes.parent_beacon_block_root,
        )
        .map_err(|err| {
            warn!(target: "payload_builder",
                parent_header=%parent_header.hash(),
                %err,
                "failed to apply beacon root contract call for payload"
            );
            PayloadBuilderError::Internal(err.into())
        })?;

    // Ensure that the create2deployer is force-deployed at the canyon transition. Optimism
    // blocks will always have at least a single transaction in them (the L1 info transaction),
    // so we can safely assume that this will always be triggered upon the transition and that
    // the above check for empty blocks will never be hit on OP chains.
    reth_optimism_evm::ensure_create2_deployer(
        chain_spec.clone(),
        attributes.payload_attributes.timestamp,
        &mut db,
    )
    .map_err(|err| {
        warn!(target: "payload_builder", %err, "missing create2 deployer, skipping block.");
        PayloadBuilderError::other(OpPayloadBuilderError::ForceCreate2DeployerFail)
    })?;

    let mut receipts = Vec::with_capacity(attributes.transactions.len());
    for sequencer_tx in &attributes.transactions {
        // Check if the job was cancelled, if so we can exit early.
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // A sequencer's block should never contain blob transactions.
        if sequencer_tx.value().is_eip4844() {
            return Err(PayloadBuilderError::other(
                OpPayloadBuilderError::BlobTransactionRejected,
            ));
        }

        // Convert the transaction to a [TransactionSignedEcRecovered]. This is
        // purely for the purposes of utilizing the `evm_config.tx_env`` function.
        // Deposit transactions do not have signatures, so if the tx is a deposit, this
        // will just pull in its `from` address.
        let sequencer_tx = sequencer_tx
            .value()
            .clone()
            .try_into_ecrecovered()
            .map_err(|_| {
                PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
            })?;

        // Cache the depositor account prior to the state transition for the deposit nonce.
        //
        // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
        // were not introduced in Bedrock. In addition, regular transactions don't have deposit
        // nonces, so we don't need to touch the DB for those.
        let depositor = (is_regolith && sequencer_tx.is_deposit())
            .then(|| {
                db.load_cache_account(sequencer_tx.signer())
                    .map(|acc| acc.account_info().unwrap_or_default())
            })
            .transpose()
            .map_err(|_| {
                PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
                    sequencer_tx.signer(),
                ))
            })?;

        let env = EnvWithHandlerCfg::new_with_cfg_env(
            initialized_cfg.clone(),
            initialized_block_env.clone(),
            evm_config.tx_env(sequencer_tx.as_signed(), sequencer_tx.signer()),
        );

        let mut evm = evm_config.evm_with_env(&mut db, env);

        let ResultAndState { result, state } = match evm.transact() {
            Ok(res) => res,
            Err(err) => {
                match err {
                    EVMError::Transaction(err) => {
                        trace!(target: "payload_builder", %err, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                        continue;
                    }
                    err => {
                        // this is an error that we should treat as fatal for this attempt
                        return Err(PayloadBuilderError::EvmExecutionError(err));
                    }
                }
            }
        };

        // to release the db reference drop evm.
        drop(evm);
        // commit changes
        db.commit(state);

        let gas_used = result.gas_used();

        // add gas used by the transaction to cumulative gas used, before creating the receipt
        cumulative_gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        receipts.push(Some(Receipt {
            tx_type: sequencer_tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs().into_iter().map(Into::into).collect(),
            deposit_nonce: depositor.map(|account| account.nonce),
            // The deposit receipt version was introduced in Canyon to indicate an update to how
            // receipt hashes should be computed when set. The state transition process
            // ensures this is only set for post-Canyon deposit transactions.
            deposit_receipt_version: chain_spec
                .is_canyon_active_at_timestamp(attributes.payload_attributes.timestamp)
                .then_some(1),
        }));

        // append sender and transaction to the respective lists
        executed_senders.push(sequencer_tx.signer());
        executed_txs.push(sequencer_tx.into_signed());
    }

    // reserved gas for builder tx
    let message = format!("Block Number: {}", block_number)
        .as_bytes()
        .to_vec();
    let builder_tx_gas = if builder_signer.is_some() {
        estimate_gas_for_builder_tx(message.clone())
    } else {
        0
    };

    // Apply rbuilder block
    let mut count = 0;
    let iter = pool
        .get_transactions(U256::from(parent_header.number + 1))
        .unwrap()
        .into_iter();
    for pool_tx in iter {
        // Ensure we still have capacity for this transaction
        if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit - builder_tx_gas {
            let inner =
                EVMError::Custom("rbuilder suggested transaction over gas limit!".to_string());
            return Err(PayloadBuilderError::EvmExecutionError(inner));
        }

        // A sequencer's block should never contain blob or deposit transactions from rbuilder.
        if pool_tx.is_eip4844() || pool_tx.tx_type() == TxType::Deposit as u8 {
            error!("rbuilder suggested blob or deposit transaction!");
            let inner =
                EVMError::Custom("rbuilder suggested blob or deposit transaction!".to_string());
            return Err(PayloadBuilderError::EvmExecutionError(inner));
        }

        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // convert tx to a signed transaction
        let tx = pool_tx.try_into_ecrecovered().map_err(|_| {
            PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
        })?;

        let env = EnvWithHandlerCfg::new_with_cfg_env(
            initialized_cfg.clone(),
            initialized_block_env.clone(),
            evm_config.tx_env(tx.as_signed(), tx.signer()),
        );

        // Configure the environment for the block.
        let mut evm = evm_config.evm_with_env(&mut db, env);

        let ResultAndState { result, state } = match evm.transact() {
            Ok(res) => res,
            Err(err) => {
                match err {
                    EVMError::Transaction(err) => {
                        if matches!(err, InvalidTransaction::NonceTooLow { .. }) {
                            // if the nonce is too low, we can skip this transaction
                            error!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            error!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                        }

                        let inner = EVMError::Custom("rbuilder transaction errored!".to_string());
                        return Err(PayloadBuilderError::EvmExecutionError(inner));
                    }
                    err => {
                        // this is an error that we should treat as fatal for this attempt
                        error!("rbuilder provided where an error occured!");
                        return Err(PayloadBuilderError::EvmExecutionError(err));
                    }
                }
            }
        };
        // drop evm so db is released.
        drop(evm);
        // commit changes
        db.commit(state);

        let gas_used = result.gas_used();

        // add gas used by the transaction to cumulative gas used, before creating the
        // receipt
        cumulative_gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        receipts.push(Some(Receipt {
            tx_type: tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs().into_iter().map(Into::into).collect(),
            deposit_nonce: None,
            deposit_receipt_version: None,
        }));

        // update add to total fees
        let miner_fee = tx
            .effective_tip_per_gas(base_fee)
            .expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);

        // append sender and transaction to the respective lists
        executed_senders.push(tx.signer());
        executed_txs.push(tx.clone().into_signed());
        count += 1;
    }

    trace!("Executed {} txns from rbuilder", count);

    // check if we have a better block, but only if we included transactions from the pool
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        // can skip building the block
        return Ok(BuildOutcome::Aborted {
            fees: total_fees,
            cached_reads,
        });
    }

    // Add builder tx to the block
    builder_signer
        .map(|signer| {
            // Create message with block number for the builder to sign
            let nonce = db
                .load_cache_account(signer.address)
                .map(|acc| acc.account_info().unwrap_or_default().nonce)
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
                        signer.address,
                    ))
                })?;

            // Create the EIP-1559 transaction
            let tx = RethTransaction::Eip1559(TxEip1559 {
                chain_id: chain_spec.chain.id(),
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

            let env = EnvWithHandlerCfg::new_with_cfg_env(
                initialized_cfg.clone(),
                initialized_block_env.clone(),
                evm_config.tx_env(builder_tx.as_signed(), builder_tx.signer()),
            );

            let mut evm = evm_config.evm_with_env(&mut db, env);

            let ResultAndState { result, state } = evm
                .transact()
                .map_err(PayloadBuilderError::EvmExecutionError)?;

            // Release the db reference by dropping evm
            drop(evm);
            // Commit changes
            db.commit(state);

            let gas_used = result.gas_used();

            // Add gas used by the transaction to cumulative gas used, before creating the receipt
            cumulative_gas_used += gas_used;

            // Push transaction changeset and calculate header bloom filter for receipt
            receipts.push(Some(Receipt {
                tx_type: builder_tx.tx_type(),
                success: result.is_success(),
                cumulative_gas_used,
                logs: result.into_logs().into_iter().map(Into::into).collect(),
                deposit_nonce: None,
                deposit_receipt_version: None,
            }));

            // Append sender and transaction to the respective lists
            executed_senders.push(builder_tx.signer());
            executed_txs.push(builder_tx.into_signed());
            Ok(())
        })
        .transpose()
        .unwrap_or_else(|err: PayloadBuilderError| {
            warn!(target: "payload_builder", %err, "Failed to add builder transaction");
            None
        });

    let WithdrawalsOutcome {
        withdrawals_root,
        withdrawals,
    } = commit_withdrawals(
        &mut db,
        &chain_spec,
        attributes.payload_attributes.timestamp,
        attributes.payload_attributes.withdrawals.clone(),
    )?;

    // merge all transitions into bundle state, this would apply the withdrawal balance changes
    // and 4788 contract call
    db.merge_transitions(BundleRetention::Reverts);

    let execution_outcome = ExecutionOutcome::new(
        db.take_bundle(),
        vec![receipts.clone()].into(),
        block_number,
        Vec::new(),
    );
    let receipts_root = execution_outcome
        .generic_receipts_root_slow(block_number, |receipts| {
            calculate_receipt_root_no_memo_optimism(receipts, &chain_spec, attributes.timestamp())
        })
        .expect("Number is in range");
    let logs_bloom = execution_outcome
        .block_logs_bloom(block_number)
        .expect("Number is in range");

    // calculate the state root
    let hashed_state = HashedPostState::from_bundle_state(&execution_outcome.state().state);
    let (state_root, trie_output) = {
        db.database
            .inner()
            .state_root_with_updates(hashed_state.clone())
            .inspect_err(|err| {
                warn!(target: "payload_builder",
                parent_header=%parent_header.hash(),
                    %err,
                    "failed to calculate state root for payload"
                );
            })?
    };

    // create the block header
    let transactions_root = proofs::calculate_transaction_root(&executed_txs);

    // OP doesn't support blobs/EIP-4844.
    // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
    // Need [Some] or [None] based on hardfork to match block hash.
    let (excess_blob_gas, blob_gas_used) =
        if chain_spec.is_ecotone_active_at_timestamp(attributes.payload_attributes.timestamp) {
            (Some(0), Some(0))
        } else {
            (None, None)
        };

    let is_holocene =
        chain_spec.is_holocene_active_at_timestamp(attributes.payload_attributes.timestamp);

    if is_holocene {
        extra_data = attributes
            .get_holocene_extra_data(
                chain_spec.base_fee_params_at_timestamp(attributes.payload_attributes.timestamp),
            )
            .map_err(PayloadBuilderError::other)?;
    }

    let header = Header {
        parent_hash: parent_header.hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: initialized_block_env.coinbase,
        state_root,
        transactions_root,
        receipts_root,
        withdrawals_root,
        logs_bloom,
        timestamp: attributes.payload_attributes.timestamp,
        mix_hash: attributes.payload_attributes.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(base_fee),
        number: parent_header.number + 1,
        gas_limit: block_gas_limit,
        difficulty: U256::ZERO,
        gas_used: cumulative_gas_used,
        extra_data,
        parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash: None,
    };

    // seal the block
    let block = Block {
        header,
        body: BlockBody {
            transactions: executed_txs,
            ommers: vec![],
            withdrawals,
        },
    };

    let sealed_block = block.seal_slow();
    debug!(target: "payload_builder", ?sealed_block, "sealed built block");

    // create the executed block data
    let executed = ExecutedBlock {
        block: Arc::new(sealed_block.clone()),
        senders: Arc::new(executed_senders),
        execution_output: Arc::new(execution_outcome),
        hashed_state: Arc::new(hashed_state),
        trie: Arc::new(trie_output),
    };

    let payload = OpBuiltPayload::new(
        attributes.payload_attributes.id,
        Arc::new(sealed_block),
        total_fees,
        chain_spec,
        attributes,
        Some(executed),
    );

    Ok(BuildOutcome::Better {
        payload,
        cached_reads,
    })
}

fn estimate_gas_for_builder_tx(input: Vec<u8>) -> u64 {
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

    zero_cost + nonzero_cost + 21_000
}
