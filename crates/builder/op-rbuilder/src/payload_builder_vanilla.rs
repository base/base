use alloy_rpc_types_eth::Withdrawals;
use reth_transaction_pool::PoolTransaction;
use std::{fmt::Display, sync::Arc};

use crate::{
    generator::{BlockCell, PayloadBuilder},
    tx_signer::Signer,
};
use alloy_consensus::{
    Eip658Value, Header, Transaction, TxEip1559, Typed2718, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::merge::BEACON_NONCE;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use reth_basic_payload_builder::*;
use reth_chain_state::ExecutedBlock;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_evm::{env::EvmEnv, system_calls::SystemCaller, ConfigureEvm, NextBlockEnvAttributes};
use reth_execution_types::ExecutionOutcome;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::calculate_receipt_root_no_memo_optimism;
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::{OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::PayloadTransactions;
use reth_primitives::{
    proofs, transaction::SignedTransactionIntoRecoveredExt, Block, BlockBody, BlockExt,
    SealedHeader, TxType,
};
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateProviderFactory, StateRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::{BestTransactionsAttributes, TransactionPool};
use revm::{
    db::{states::bundle_state::BundleRetention, State},
    primitives::{
        BlockEnv, CfgEnvWithHandlerCfg, EVMError, EnvWithHandlerCfg, InvalidTransaction,
        ResultAndState, TxEnv,
    },
    Database, DatabaseCommit,
};
use tracing::{debug, trace, warn};

use op_alloy_consensus::{OpDepositReceipt, OpTxType, OpTypedTransaction};
use reth_optimism_payload_builder::{
    error::OpPayloadBuilderError,
    payload::{OpBuiltPayload, OpPayloadBuilderAttributes},
};
use reth_transaction_pool::pool::BestPayloadTransactions;

/// Optimism's payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpPayloadBuilderVanilla<EvmConfig, Txs = ()> {
    /// The rollup's compute pending block configuration option.
    // TODO(clabby): Implement this feature.
    pub compute_pending_block: bool,
    /// The type responsible for creating the evm.
    pub evm_config: EvmConfig,
    /// The builder's signer key to use for an end of block tx
    pub builder_signer: Option<Signer>,
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
}

impl<EvmConfig> OpPayloadBuilderVanilla<EvmConfig> {
    /// `OpPayloadBuilder` constructor.
    pub const fn new(evm_config: EvmConfig, builder_signer: Option<Signer>) -> Self {
        Self {
            compute_pending_block: true,
            evm_config,
            builder_signer,
            best_transactions: (),
        }
    }
}

impl<EvmConfig, Pool, Client> PayloadBuilder<Pool, Client> for OpPayloadBuilderVanilla<EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = EvmConfig::Transaction>>,
    EvmConfig: ConfigureEvm<Header = Header, Transaction = OpTransactionSigned>,
{
    type Attributes = OpPayloadBuilderAttributes;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        let pool = args.pool.clone();

        match self.build_payload(args, |attrs| ().best_transactions(pool, attrs))? {
            BuildOutcome::Better { payload, .. } => {
                best_payload.set(payload);
                Ok(())
            }
            BuildOutcome::Freeze(payload) => {
                best_payload.set(payload);
                Ok(())
            }
            BuildOutcome::Cancelled => {
                tracing::warn!("Payload build cancelled");
                Err(PayloadBuilderError::MissingPayload)
            }
            _ => {
                tracing::warn!("No better payload found");
                Err(PayloadBuilderError::MissingPayload)
            }
        }
    }
}

impl<EvmConfig> OpPayloadBuilderVanilla<EvmConfig>
where
    EvmConfig: ConfigureEvm<Header = Header, Transaction = OpTransactionSigned>,
{
    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    fn build_payload<'a, Client, Pool, Txs>(
        &self,
        args: BuildArguments<Pool, Client, OpPayloadBuilderAttributes, OpBuiltPayload>,
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
    ) -> Result<BuildOutcome<OpBuiltPayload>, PayloadBuilderError>
    where
        Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
        Pool: TransactionPool,
        Txs: PayloadTransactions<Transaction = OpTransactionSigned>,
    {
        let evm_env = self
            .cfg_and_block_env(&args.config.attributes, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;
        let EvmEnv {
            cfg_env_with_handler_cfg,
            block_env,
        } = evm_env;

        let BuildArguments {
            client,
            pool: _,
            mut cached_reads,
            config,
            cancel,
            best_payload,
        } = args;

        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            chain_spec: client.chain_spec(),
            config,
            initialized_cfg: cfg_env_with_handler_cfg,
            initialized_block_env: block_env,
            cancel,
            best_payload,
            builder_signer: self.builder_signer,
        };

        let builder = OpBuilder::new(best);

        let state_provider = client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(state_provider);

        if ctx.attributes().no_tx_pool {
            let db = State::builder()
                .with_database(state)
                .with_bundle_update()
                .build();
            builder.build(db, ctx)
        } else {
            // sequencer mode we can reuse cachedreads from previous runs
            let db = State::builder()
                .with_database(cached_reads.as_db_mut(state))
                .with_bundle_update()
                .build();
            builder.build(db, ctx)
        }
        .map(|out| out.with_cached_reads(cached_reads))
    }
}

impl<EvmConfig, Txs> OpPayloadBuilderVanilla<EvmConfig, Txs>
where
    EvmConfig: ConfigureEvm<Header = Header>,
{
    /// Returns the configured [`EvmEnv`] for the targeted payload
    /// (that has the `parent` as its parent).
    pub fn cfg_and_block_env(
        &self,
        attributes: &OpPayloadBuilderAttributes,
        parent: &Header,
    ) -> Result<EvmEnv, EvmConfig::Error> {
        let next_attributes = NextBlockEnvAttributes {
            timestamp: attributes.timestamp(),
            suggested_fee_recipient: attributes.suggested_fee_recipient(),
            prev_randao: attributes.prev_randao(),
            gas_limit: attributes.gas_limit.unwrap_or(parent.gas_limit),
        };
        self.evm_config
            .next_cfg_and_block_env(parent, next_attributes)
    }
}

/// The type that builds the payload.
///
/// Payload building for optimism is composed of several steps.
/// The first steps are mandatory and defined by the protocol.
///
/// 1. first all System calls are applied.
/// 2. After canyon the forced deployed `create2deployer` must be loaded
/// 3. all sequencer transactions are executed (part of the payload attributes)
///
/// Depending on whether the node acts as a sequencer and is allowed to include additional
/// transactions (`no_tx_pool == false`):
/// 4. include additional transactions
///
/// And finally
/// 5. build the block: compute all roots (txs, state)
#[derive(derive_more::Debug)]
pub struct OpBuilder<'a, Txs> {
    /// Yields the best transaction to include if transactions from the mempool are allowed.
    best: Box<dyn FnOnce(BestTransactionsAttributes) -> Txs + 'a>,
}

impl<'a, Txs> OpBuilder<'a, Txs> {
    fn new(best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a) -> Self {
        Self {
            best: Box::new(best),
        }
    }
}

impl<Txs> OpBuilder<'_, Txs>
where
    Txs: PayloadTransactions<Transaction = OpTransactionSigned>,
{
    /// Executes the payload and returns the outcome.
    pub fn execute<EvmConfig, DB>(
        self,
        state: &mut State<DB>,
        ctx: &OpPayloadBuilderCtx<EvmConfig>,
    ) -> Result<BuildOutcomeKind<ExecutedPayload>, PayloadBuilderError>
    where
        EvmConfig: ConfigureEvm<Header = Header, Transaction = OpTransactionSigned>,
        DB: Database<Error = ProviderError>,
    {
        let Self { best } = self;
        debug!(target: "payload_builder", id=%ctx.payload_id(), parent_header = ?ctx.parent().hash(), parent_number = ctx.parent().number, "building new payload");

        // 1. apply eip-4788 pre block contract call
        ctx.apply_pre_beacon_root_contract_call(state)?;

        // 2. ensure create2deployer is force deployed
        ctx.ensure_create2_deployer(state)?;

        // 3. execute sequencer transactions
        let mut info = ctx.execute_sequencer_transactions(state)?;

        // 4. if mem pool transactions are requested we execute them

        // gas reserved for builder tx
        let message = format!("Block Number: {}", ctx.block_number())
            .as_bytes()
            .to_vec();
        let builder_tx_gas = ctx.builder_signer().map_or(0, |_| {
            OpPayloadBuilderCtx::<EvmConfig>::estimate_gas_for_builder_tx(message.clone())
        });
        let block_gas_limit = ctx.block_gas_limit() - builder_tx_gas;
        if !ctx.attributes().no_tx_pool {
            let best_txs = best(ctx.best_transaction_attributes());
            if ctx
                .execute_best_transactions(&mut info, state, best_txs, block_gas_limit)?
                .is_some()
            {
                return Ok(BuildOutcomeKind::Cancelled);
            }

            // check if the new payload is even more valuable
            if !ctx.is_better_payload(info.total_fees) {
                // can skip building the block
                return Ok(BuildOutcomeKind::Aborted {
                    fees: info.total_fees,
                });
            }
        }

        // Add builder tx to the block
        ctx.add_builder_tx(&mut info, state, builder_tx_gas, message);

        let withdrawals_root = ctx.commit_withdrawals(state)?;

        // merge all transitions into bundle state, this would apply the withdrawal balance changes
        // and 4788 contract call
        state.merge_transitions(BundleRetention::Reverts);

        Ok(BuildOutcomeKind::Better {
            payload: ExecutedPayload {
                info,
                withdrawals_root,
            },
        })
    }

    /// Builds the payload on top of the state.
    pub fn build<EvmConfig, DB, P>(
        self,
        mut state: State<DB>,
        ctx: OpPayloadBuilderCtx<EvmConfig>,
    ) -> Result<BuildOutcomeKind<OpBuiltPayload>, PayloadBuilderError>
    where
        EvmConfig: ConfigureEvm<Header = Header, Transaction = OpTransactionSigned>,
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider,
    {
        let ExecutedPayload {
            info,
            withdrawals_root,
        } = match self.execute(&mut state, &ctx)? {
            BuildOutcomeKind::Better { payload } | BuildOutcomeKind::Freeze(payload) => payload,
            BuildOutcomeKind::Cancelled => return Ok(BuildOutcomeKind::Cancelled),
            BuildOutcomeKind::Aborted { fees } => return Ok(BuildOutcomeKind::Aborted { fees }),
        };

        let block_number = ctx.block_number();
        let execution_outcome = ExecutionOutcome::new(
            state.take_bundle(),
            info.receipts.into(),
            block_number,
            Vec::new(),
        );
        let receipts_root = execution_outcome
            .generic_receipts_root_slow(block_number, |receipts| {
                calculate_receipt_root_no_memo_optimism(
                    receipts,
                    &ctx.chain_spec,
                    ctx.attributes().timestamp(),
                )
            })
            .expect("Number is in range");
        let logs_bloom = execution_outcome
            .block_logs_bloom(block_number)
            .expect("Number is in range");

        // // calculate the state root
        let state_provider = state.database.as_ref();
        let hashed_state = state_provider.hashed_post_state(execution_outcome.state());
        let (state_root, trie_output) = {
            state
                .database
                .as_ref()
                .state_root_with_updates(hashed_state.clone())
                .inspect_err(|err| {
                    warn!(target: "payload_builder",
                    parent_header=%ctx.parent().hash(),
                        %err,
                        "failed to calculate state root for payload"
                    );
                })?
        };

        // create the block header
        let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);

        // OP doesn't support blobs/EIP-4844.
        // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
        // Need [Some] or [None] based on hardfork to match block hash.
        let (excess_blob_gas, blob_gas_used) = ctx.blob_fields();
        let extra_data = ctx.extra_data()?;

        let header = Header {
            parent_hash: ctx.parent().hash(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: ctx.initialized_block_env.coinbase,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: ctx.attributes().payload_attributes.timestamp,
            mix_hash: ctx.attributes().payload_attributes.prev_randao,
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(ctx.base_fee()),
            number: ctx.parent().number + 1,
            gas_limit: ctx.block_gas_limit(),
            difficulty: U256::ZERO,
            gas_used: info.cumulative_gas_used,
            extra_data,
            parent_beacon_block_root: ctx.attributes().payload_attributes.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash: None,
        };

        // seal the block
        let block = Block {
            header,
            body: BlockBody {
                transactions: info.executed_transactions,
                ommers: vec![],
                withdrawals: ctx.withdrawals().cloned(),
            },
        };

        let sealed_block = Arc::new(block.seal_slow());
        debug!(target: "payload_builder", id=%ctx.attributes().payload_id(), sealed_block_header = ?sealed_block.header, "sealed built block");

        // create the executed block data
        let executed: ExecutedBlock<OpPrimitives> = ExecutedBlock {
            block: sealed_block.clone(),
            senders: Arc::new(info.executed_senders),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Arc::new(hashed_state),
            trie: Arc::new(trie_output),
        };

        let no_tx_pool = ctx.attributes().no_tx_pool;

        let payload = OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            ctx.chain_spec.clone(),
            ctx.config.attributes,
            Some(executed),
        );

        if no_tx_pool {
            // if `no_tx_pool` is set only transactions from the payload attributes will be included
            // in the payload. In other words, the payload is deterministic and we can
            // freeze it once we've successfully built it.
            Ok(BuildOutcomeKind::Freeze(payload))
        } else {
            Ok(BuildOutcomeKind::Better { payload })
        }
    }
}

/// A type that returns a the [`PayloadTransactions`] that should be included in the pool.
pub trait OpPayloadTransactions: Clone + Send + Sync + Unpin + 'static {
    /// Returns an iterator that yields the transaction in the order they should get included in the
    /// new payload.
    fn best_transactions<
        Pool: TransactionPool<Transaction: PoolTransaction<Consensus = OpTransactionSigned>>,
    >(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = OpTransactionSigned>;
}

impl OpPayloadTransactions for () {
    fn best_transactions<
        Pool: TransactionPool<Transaction: PoolTransaction<Consensus = OpTransactionSigned>>,
    >(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = OpTransactionSigned> {
        BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr))
    }
}

/// Holds the state after execution
#[derive(Debug)]
pub struct ExecutedPayload {
    /// Tracked execution info
    pub info: ExecutionInfo,
    /// Withdrawal hash.
    pub withdrawals_root: Option<B256>,
}

/// This acts as the container for executed transactions and its byproducts (receipts, gas used)
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
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
}

impl ExecutionInfo {
    /// Create a new instance with allocated slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            executed_transactions: Vec::with_capacity(capacity),
            executed_senders: Vec::with_capacity(capacity),
            receipts: Vec::with_capacity(capacity),
            cumulative_gas_used: 0,
            total_fees: U256::ZERO,
        }
    }
}

/// Container type that holds all necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx<EvmConfig> {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: EvmConfig,
    /// The chainspec
    pub chain_spec: Arc<OpChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes>,
    /// Evm Settings
    pub initialized_cfg: CfgEnvWithHandlerCfg,
    /// Block config
    pub initialized_block_env: BlockEnv,
    /// Marker to check whether the job has been cancelled.
    pub cancel: Cancelled,
    /// The currently best payload.
    pub best_payload: Option<OpBuiltPayload>,
    /// The builder signer
    pub builder_signer: Option<Signer>,
}

impl<EvmConfig> OpPayloadBuilderCtx<EvmConfig> {
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes {
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
            .unwrap_or_else(|| self.initialized_block_env.gas_limit.saturating_to())
    }

    /// Returns the block number for the block.
    pub fn block_number(&self) -> u64 {
        self.initialized_block_env.number.to()
    }

    /// Returns the current base fee
    pub fn base_fee(&self) -> u64 {
        self.initialized_block_env.basefee.to()
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.initialized_block_env
            .get_blob_gasprice()
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

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain.id()
    }

    /// Returns the builder signer
    pub fn builder_signer(&self) -> Option<Signer> {
        self.builder_signer
    }

    /// Returns true if the fees are higher than the previous payload.
    pub fn is_better_payload(&self, total_fees: U256) -> bool {
        is_better_payload(self.best_payload.as_ref(), total_fees)
    }

    /// Commits the withdrawals from the payload attributes to the state.
    pub fn commit_withdrawals<DB>(&self, db: &mut State<DB>) -> Result<Option<B256>, ProviderError>
    where
        DB: Database<Error = ProviderError>,
    {
        commit_withdrawals(
            db,
            &self.chain_spec,
            self.attributes().payload_attributes.timestamp,
            &self.attributes().payload_attributes.withdrawals,
        )
    }

    /// Ensure that the create2deployer is force-deployed at the canyon transition. Optimism
    /// blocks will always have at least a single transaction in them (the L1 info transaction),
    /// so we can safely assume that this will always be triggered upon the transition and that
    /// the above check for empty blocks will never be hit on OP chains.
    pub fn ensure_create2_deployer<DB>(&self, db: &mut State<DB>) -> Result<(), PayloadBuilderError>
    where
        DB: Database,
        DB::Error: Display,
    {
        reth_optimism_evm::ensure_create2_deployer(
            self.chain_spec.clone(),
            self.attributes().payload_attributes.timestamp,
            db,
        )
        .map_err(|err| {
            warn!(target: "payload_builder", %err, "missing create2 deployer, skipping block.");
            PayloadBuilderError::other(OpPayloadBuilderError::ForceCreate2DeployerFail)
        })
    }
}

impl<EvmConfig> OpPayloadBuilderCtx<EvmConfig>
where
    EvmConfig: ConfigureEvm<Header = Header, Transaction = OpTransactionSigned>,
{
    /// apply eip-4788 pre block contract call
    pub fn apply_pre_beacon_root_contract_call<DB>(
        &self,
        db: &mut DB,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database + DatabaseCommit,
        DB::Error: Display,
    {
        SystemCaller::new(self.evm_config.clone(), self.chain_spec.clone())
            .pre_block_beacon_root_contract_call(
                db,
                &self.initialized_cfg,
                &self.initialized_block_env,
                self.attributes()
                    .payload_attributes
                    .parent_beacon_block_root,
            )
            .map_err(|err| {
                warn!(target: "payload_builder",
                    parent_header=%self.parent().hash(),
                    %err,
                    "failed to apply beacon root contract call for payload"
                );
                PayloadBuilderError::Internal(err.into())
            })?;

        Ok(())
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    pub fn execute_sequencer_transactions<DB>(
        &self,
        db: &mut State<DB>,
    ) -> Result<ExecutionInfo, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());

        let env = EnvWithHandlerCfg::new_with_cfg_env(
            self.initialized_cfg.clone(),
            self.initialized_block_env.clone(),
            TxEnv::default(),
        );
        let mut evm = self.evm_config.evm_with_env(&mut *db, env);

        for sequencer_tx in &self.attributes().transactions {
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
            let depositor = (self.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .load_cache_account(sequencer_tx.signer())
                        .map(|acc| acc.account_info().unwrap_or_default())
                })
                .transpose()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
                        sequencer_tx.signer(),
                    ))
                })?;

            *evm.tx_mut() = self
                .evm_config
                .tx_env(sequencer_tx.tx(), sequencer_tx.signer());

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

            // commit changes
            evm.db_mut().commit(state);

            let gas_used = result.gas_used();

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            info.cumulative_gas_used += gas_used;

            let receipt = alloy_consensus::Receipt {
                status: Eip658Value::Eip658(result.is_success()),
                cumulative_gas_used: info.cumulative_gas_used,
                logs: result.into_logs().into_iter().collect(),
            };

            // Push transaction changeset and calculate header bloom filter for receipt.
            info.receipts.push(match sequencer_tx.tx_type() {
                OpTxType::Legacy => OpReceipt::Legacy(receipt),
                OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
                OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
                OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
                OpTxType::Deposit => OpReceipt::Deposit(OpDepositReceipt {
                    inner: receipt,
                    deposit_nonce: depositor.map(|account| account.nonce),
                    // The deposit receipt version was introduced in Canyon to indicate an update to
                    // how receipt hashes should be computed when set. The state
                    // transition process ensures this is only set for
                    // post-Canyon deposit transactions.
                    deposit_receipt_version: self.is_canyon_active().then_some(1),
                }),
            });

            // append sender and transaction to the respective lists
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_tx());
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    pub fn execute_best_transactions<DB>(
        &self,
        info: &mut ExecutionInfo,
        db: &mut State<DB>,
        mut best_txs: impl PayloadTransactions<Transaction = EvmConfig::Transaction>,
        block_gas_limit: u64,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let base_fee = self.base_fee();

        let env = EnvWithHandlerCfg::new_with_cfg_env(
            self.initialized_cfg.clone(),
            self.initialized_block_env.clone(),
            TxEnv::default(),
        );
        let mut evm = self.evm_config.evm_with_env(&mut *db, env);

        while let Some(tx) = best_txs.next(()) {
            // ensure we still have capacity for this transaction
            if info.cumulative_gas_used + tx.gas_limit() > block_gas_limit {
                // we can't fit this transaction into the block, so we need to mark it as
                // invalid which also removes all dependent transaction from
                // the iterator before we can continue
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.tx_type() == TxType::Deposit as u8 {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            // Configure the environment for the tx.
            *evm.tx_mut() = self.evm_config.tx_env(tx.tx(), tx.signer());

            let ResultAndState { result, state } = match evm.transact() {
                Ok(res) => res,
                Err(err) => {
                    match err {
                        EVMError::Transaction(err) => {
                            if matches!(err, InvalidTransaction::NonceTooLow { .. }) {
                                // if the nonce is too low, we can skip this transaction
                                trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                            } else {
                                // if the transaction is invalid, we can skip it and all of its
                                // descendants
                                trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                                best_txs.mark_invalid(tx.signer(), tx.nonce());
                            }

                            continue;
                        }
                        err => {
                            // this is an error that we should treat as fatal for this attempt
                            return Err(PayloadBuilderError::EvmExecutionError(err));
                        }
                    }
                }
            };

            // commit changes
            evm.db_mut().commit(state);

            let gas_used = result.gas_used();

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            info.cumulative_gas_used += gas_used;

            let receipt = alloy_consensus::Receipt {
                status: Eip658Value::Eip658(result.is_success()),
                cumulative_gas_used: info.cumulative_gas_used,
                logs: result.into_logs().into_iter().collect(),
            };

            // Push transaction changeset and calculate header bloom filter for receipt.
            info.receipts.push(match tx.tx_type() {
                OpTxType::Legacy => OpReceipt::Legacy(receipt),
                OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
                OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
                OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
                OpTxType::Deposit => OpReceipt::Deposit(OpDepositReceipt {
                    inner: receipt,
                    deposit_nonce: None,
                    deposit_receipt_version: None,
                }),
            });

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            // append sender and transaction to the respective lists
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_tx());
        }

        Ok(None)
    }

    pub fn add_builder_tx<DB>(
        &self,
        info: &mut ExecutionInfo,
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
                let eip1559 = OpTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: self.chain_id(),
                    nonce,
                    gas_limit: builder_tx_gas,
                    max_fee_per_gas: base_fee.into(),
                    max_priority_fee_per_gas: 0,
                    to: TxKind::Call(Address::ZERO),
                    // Include the message as part of the transaction data
                    input: message.into(),
                    ..Default::default()
                });
                let tx = eip1559;

                // Sign the transaction
                let builder_tx = signer.sign_tx(tx).map_err(PayloadBuilderError::other)?;

                let env = EnvWithHandlerCfg::new_with_cfg_env(
                    self.initialized_cfg.clone(),
                    self.initialized_block_env.clone(),
                    TxEnv::default(),
                );
                let mut evm = self.evm_config.evm_with_env(&mut *db, env);
                *evm.tx_mut() = self.evm_config.tx_env(builder_tx.tx(), builder_tx.signer());

                let ResultAndState { result, state } = evm
                    .transact()
                    .map_err(PayloadBuilderError::EvmExecutionError)?;

                // Release the db reference by dropping evm
                drop(evm);
                // Commit changes
                db.commit(state);

                let gas_used = result.gas_used();

                // Add gas used by the transaction to cumulative gas used, before creating the receipt
                info.cumulative_gas_used += gas_used;

                let receipt = alloy_consensus::Receipt {
                    status: Eip658Value::Eip658(result.is_success()),
                    cumulative_gas_used: info.cumulative_gas_used,
                    logs: result.into_logs().into_iter().collect(),
                };

                // Push transaction changeset and calculate header bloom filter for receipt
                info.receipts.push(OpReceipt::Eip1559(receipt));

                // Append sender and transaction to the respective lists
                info.executed_senders.push(builder_tx.signer());
                info.executed_transactions.push(builder_tx.into_tx());
                Ok(())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
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
}
