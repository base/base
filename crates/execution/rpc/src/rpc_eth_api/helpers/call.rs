//! Loads a pending block from database. Helper trait for `eth_` transaction, call and trace RPC
//! methods.

use std::collections::BTreeMap;

use alloy_eips::eip2930::AccessListResult;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, Bytes, U256};
use base_common_chain_config::ChainSpecProvider;
use base_common_network::TransactionBuilder;
use base_common_types_chain::{BlockHeader, transaction::TxHashRef};
use base_common_types_rpc::{
    BaseBlockResponse, BaseTransactionRequest, BlockId, Bundle, EthCallResponse, StateContext,
    TransactionInfo,
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    state::{EvmOverrides, StateOverride},
};
use base_execution_evm_blocks::{
    BlockBuilder, BlockEnvironment, BlockExecutor, CancelOnDrop, Evm, EvmEnvFor, EvmFor,
    HaltReasonFor, InspectorFor, TransactionEnvMut, TxEnvFor,
};
use base_execution_evm_inspectors::{
    access_list::AccessListInspector, transfer::TransferInspector,
};
use base_execution_evm_machine::{Block, Cfg, ResultAndState, Transaction};
use base_execution_evm_runtime::{
    Database, DatabaseCommit,
    database::{EvmDatabaseError, State},
};
use base_execution_evm_runtime::{
    OverrideBlockHashes, apply_block_overrides, apply_state_overrides,
};
use futures::Future;
use reth_primitives_traits::Recovered;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_eth_types::{
    BaseEthApiError, EthApiError, StateCacheDb,
    simulate::{self, EthSimulateError},
};
use reth_storage_api::{BlockIdReader, ProviderTx};
use reth_storage_errors::provider::ProviderError;
use tracing::{trace, warn};

use crate::BaseEthApi;

/// Result type for `eth_simulateV1` RPC method.
pub type SimulatedBlocksResult<E> = Result<Vec<SimulatedBlock<BaseBlockResponse>>, E>;

/// Execution related functions for the [`EthApiServer`](crate::EthApiServer) trait in
/// the `eth_` namespace.
impl BaseEthApi {
    /// `eth_simulateV1` executes an arbitrary number of transactions on top of the requested state.
    /// The transactions are packed into individual blocks. Overrides can be provided.
    ///
    /// See also: <https://github.com/ethereum/go-ethereum/pull/27720>
    pub fn simulate_v1(
        &self,
        payload: SimulatePayload<BaseTransactionRequest>,
        block: Option<BlockId>,
    ) -> impl Future<Output = SimulatedBlocksResult<BaseEthApiError>> + Send {
        async move {
            if payload.block_state_calls.len() > self.max_simulate_blocks() as usize {
                return Err(EthApiError::other(EthSimulateError::TooManyBlocks).into());
            }

            let block = block.unwrap_or_default();

            let SimulatePayload {
                block_state_calls,
                trace_transfers,
                validation,
                return_full_transactions,
            } = payload;

            if block_state_calls.is_empty() {
                return Err(EthApiError::InvalidParams(String::from("calls are empty.")).into());
            }

            let _permit = self.acquire_owned_blocking_io().await;

            let base_block = self
                .recovered_block(block)
                .await?
                .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
            let parent = base_block.sealed_header().clone();
            let max_simulate_blocks = self.max_simulate_blocks();

            self.spawn_with_state_at_block(block, move |this, db| {
                let state_provider = db.database;
                let mut db = State::builder()
                    .with_database(state_provider.as_ref())
                    .with_bundle_update()
                    .build();
                let mut parent = parent;

                let chain_id = this.provider().chain_spec().chain_id();

                // Validate block ordering and fill gaps with empty blocks so every entry has an
                // explicit `number` and `time` override and the chain is contiguous (see the
                // execution-apis spec note: "If the block number is increased more than 1 compared
                // to the previous block, new empty blocks are generated in between.").
                let block_state_calls = simulate::sanitize_chain(
                    block_state_calls,
                    &parent,
                    chain_id,
                    max_simulate_blocks,
                )?;

                let mut blocks: Vec<SimulatedBlock<BaseBlockResponse>> =
                    Vec::with_capacity(block_state_calls.len());

                let call_gas_limit = this.call_gas_limit();
                let mut remaining_call_gas_limit = (call_gas_limit > 0).then_some(call_gas_limit);

                for block in block_state_calls {
                    let SimBlock { block_overrides, state_overrides, calls } = block;

                    let attributes = crate::BasePendingEnv::attributes(&parent);

                    let mut evm_env = this
                        .evm_config()
                        .next_evm_env(&parent, &attributes)
                        .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
                        .map_err(BaseEthApiError::from_eth_err)?;

                    // Always disable EIP-3607
                    evm_env.cfg_env.disable_eip3607 = true;

                    // EIP-7825's transaction gas cap is only active with Amsterdam's
                    // regular/state-gas accounting.
                    if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
                    }

                    if !validation {
                        // If not explicitly required, we disable nonce check <https://github.com/paradigmxyz/reth/issues/16108>
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;
                        evm_env.block_env.inner_mut().basefee = 0;
                    }

                    // Set prevrandao to zero for simulated blocks by default,
                    // matching spec behavior where MixDigest is zero-initialized.
                    // If user provides an override, it will be applied by apply_block_overrides.
                    evm_env.block_env.inner_mut().prevrandao = Some(B256::ZERO);
                    if !this
                        .provider()
                        .chain_spec()
                        .is_paris_active_at_block(evm_env.block_env.number().saturating_to())
                    {
                        evm_env.block_env.inner_mut().difficulty = parent.difficulty();
                    }

                    if let Some(block_overrides) = block_overrides {
                        // ensure we don't allow uncapped gas limit per block
                        if let Some(gas_limit_override) = block_overrides.gas_limit &&
                            gas_limit_override > evm_env.block_env.gas_limit() &&
                            gas_limit_override > this.call_gas_limit()
                        {
                            return Err(EthApiError::other(EthSimulateError::GasLimitReached).into())
                        }
                        apply_block_overrides(
                            block_overrides,
                            &mut db,
                            evm_env.block_env.inner_mut(),
                        );
                    }
                    if let Some(ref state_overrides) = state_overrides {
                        apply_state_overrides(state_overrides.clone(), &mut db)
                            .map_err(BaseEthApiError::from_eth_err)?;
                    }

                    let chain_id = evm_env.cfg_env.chain_id;

                    let ctx = this
                        .evm_config()
                        .context_for_next_block(&parent, attributes)
                        .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
                        .map_err(BaseEthApiError::from_eth_err)?;
                    let map_err = |e: EthApiError| -> BaseEthApiError {
                        match e.as_simulate_error() {
                            Some(sim_err) => BaseEthApiError::from_eth_err(EthApiError::other(sim_err)),
                            None => BaseEthApiError::from_eth_err(e),
                        }
                    };

                    let (result, results) = if trace_transfers {
                        // prepare inspector to capture transfer inside the evm so they are recorded
                        // and included in logs
                        let inspector = TransferInspector::new(false).with_logs(true);
                        let evm = this
                            .evm_config()
                            .evm_with_env_and_inspector(&mut db, evm_env, inspector);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| BaseEthApiError::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            &state_provider,
                            calls,
                            &mut remaining_call_gas_limit,
                            chain_id,
                            this.compute_state_root_for_eth_simulate(),
                            this.converter(),
                        )
                        .map_err(map_err)?
                    } else {
                        let evm = this.evm_config().evm_with_env(&mut db, evm_env);
                        let mut builder = this.evm_config().create_block_builder(evm, &parent, ctx);

                        if let Some(ref state_overrides) = state_overrides {
                            simulate::apply_precompile_overrides(
                                state_overrides,
                                builder.evm_mut().precompiles_mut(),
                            )
                            .map_err(|e| BaseEthApiError::from_eth_err(EthApiError::other(e)))?;
                        }

                        simulate::execute_transactions(
                            builder,
                            &state_provider,
                            calls,
                            &mut remaining_call_gas_limit,
                            chain_id,
                            this.compute_state_root_for_eth_simulate(),
                            this.converter(),
                        )
                        .map_err(map_err)?
                    };

                    let simulated_header = result.block.clone_sealed_header();
                    db.override_block_hashes(BTreeMap::from([(
                        simulated_header.number(),
                        simulated_header.hash(),
                    )]));
                    parent = simulated_header;

                    let block = simulate::build_simulated_block(
                        result.block,
                        results,
                        return_full_transactions.into(),
                        this.converter(),
                    )?;

                    blocks.push(block);
                }

                Ok(blocks)
            })
            .await
        }
    }

    /// Executes the call request (`eth_call`) and returns the output
    pub fn call(
        &self,
        request: BaseTransactionRequest,
        block_number: Option<BlockId>,
        overrides: EvmOverrides,
    ) -> impl Future<Output = Result<Bytes, BaseEthApiError>> + Send {
        async move {
            let _permit = self.acquire_owned_blocking_io().await;
            let res =
                self.transact_call_at(request, block_number.unwrap_or_default(), overrides).await?;

            BaseEthApiError::ensure_success(res.result)
        }
    }

    /// Simulate arbitrary number of transactions at an arbitrary blockchain index, with the
    /// optionality of state overrides
    pub fn call_many(
        &self,
        bundles: Vec<Bundle<BaseTransactionRequest>>,
        state_context: Option<StateContext>,
        mut state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<Vec<Vec<EthCallResponse>>, BaseEthApiError>> + Send {
        async move {
            // Check if the vector of bundles is empty
            if bundles.is_empty() {
                return Err(EthApiError::InvalidParams(String::from("bundles are empty.")).into());
            }

            let _permit = self.acquire_owned_blocking_io().await;

            let StateContext { transaction_index, block_number } =
                state_context.unwrap_or_default();
            let transaction_index = transaction_index.unwrap_or_default();

            let mut target_block = block_number.unwrap_or_default();
            let is_block_target_pending = target_block.is_pending();

            // if it's not pending, we should always use block_hash over block_number to ensure that
            // different provider calls query data related to the same block.
            if !is_block_target_pending {
                let Some(block_hash) = self
                    .provider()
                    .block_hash_for_id(target_block)
                    .map_err(BaseEthApiError::from_eth_err::<ProviderError>)?
                else {
                    return Err(EthApiError::HeaderNotFound(target_block).into());
                };
                target_block = block_hash.into();
            }

            let block = self
                .recovered_block(target_block)
                .await?
                .ok_or(EthApiError::HeaderNotFound(target_block))?;
            let evm_env = self.evm_env_for_header(block.sealed_block().sealed_header())?;

            // we're essentially replaying the transactions in the block here, hence we need the
            // state that points to the beginning of the block, which is the state at
            // the parent block
            let mut at = block.parent_hash();
            let mut replay_block_txs = true;

            let num_txs =
                transaction_index.index().unwrap_or_else(|| block.body().transactions.len());
            // but if all transactions are to be replayed, we can use the state at the block itself,
            // however only if we're not targeting the pending block, because for pending we can't
            // rely on the block's state being available
            if !is_block_target_pending && num_txs == block.body().transactions.len() {
                at = block.hash();
                replay_block_txs = false;
            }

            self.spawn_with_state_at_block(at, move |this, mut db| {
                let mut all_results = Vec::with_capacity(bundles.len());

                if replay_block_txs {
                    this.replay_block_until(&mut db, &block, num_txs)?;
                }

                // transact all bundles
                for (bundle_index, bundle) in bundles.into_iter().enumerate() {
                    let Bundle { transactions, block_override } = bundle;
                    if transactions.is_empty() {
                        // Skip empty bundles
                        continue;
                    }

                    let mut bundle_results = Vec::with_capacity(transactions.len());
                    let block_overrides = block_override.map(Box::new);

                    // transact all transactions in the bundle
                    for (tx_index, tx) in transactions.into_iter().enumerate() {
                        // Apply overrides, state overrides are only applied for the first tx in the
                        // request
                        let overrides =
                            EvmOverrides::new(state_override.take(), block_overrides.clone());

                        let (current_evm_env, prepared_tx) = this
                            .prepare_call_env(evm_env.clone(), tx, &mut db, overrides)
                            .map_err(|err| {
                                BaseEthApiError::from_eth_err(EthApiError::call_many_error(
                                    bundle_index,
                                    tx_index,
                                    err.into(),
                                ))
                            })?;
                        let res = this.transact(&mut db, current_evm_env, prepared_tx).map_err(
                            |err| {
                                BaseEthApiError::from_eth_err(EthApiError::call_many_error(
                                    bundle_index,
                                    tx_index,
                                    err.into(),
                                ))
                            },
                        )?;

                        match BaseEthApiError::ensure_success(res.result) {
                            Ok(output) => {
                                bundle_results
                                    .push(EthCallResponse { value: Some(output), error: None });
                            }
                            Err(err) => {
                                bundle_results.push(EthCallResponse {
                                    value: None,
                                    error: Some(err.to_string()),
                                });
                            }
                        }

                        // Commit state changes after each transaction to allow subsequent calls to
                        // see the updates
                        db.commit(res.state);
                    }

                    all_results.push(bundle_results);
                }

                Ok(all_results)
            })
            .await
        }
    }

    /// Creates [`AccessListResult`] for the [`RpcTxReq`] at the given
    /// [`BlockId`], or latest block.
    pub fn create_access_list_at(
        &self,
        request: BaseTransactionRequest,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<AccessListResult, BaseEthApiError>> + Send {
        async move {
            let block_id = block_number.unwrap_or_default();
            let (evm_env, at) = self.evm_env_at(block_id).await?;

            self.spawn_blocking_io_fut(async move |this| {
                this.create_access_list_with(evm_env, at, request, state_override).await
            })
            .await
        }
    }

    /// Creates [`AccessListResult`] for the [`RpcTxReq`] at the given
    /// [`BlockId`].
    pub fn create_access_list_with(
        &self,
        evm_env: EvmEnvFor,
        at: BlockId,
        request: BaseTransactionRequest,
        state_override: Option<StateOverride>,
    ) -> impl Future<Output = Result<AccessListResult, BaseEthApiError>> + Send {
        self.spawn_with_state_at_block(at, |this, mut db| {
            let initial = request.as_ref().access_list().cloned().unwrap_or_default();
            let (evm_env, mut tx_env) = this.prepare_call_env(
                evm_env,
                request,
                &mut db,
                EvmOverrides::state(state_override),
            )?;

            let mut evm = this.evm_config().evm_with_env_and_inspector(
                &mut db,
                evm_env,
                AccessListInspector::new(initial),
            );

            let result = evm.transact(tx_env.clone())?;
            let access_list = core::mem::take(evm.inspector_mut()).into_access_list();
            let gas_used = result.result.tx_gas_used();
            tx_env.set_access_list(access_list.clone());
            if let Err(err) = BaseEthApiError::ensure_success(result.result) {
                return Ok(AccessListResult {
                    access_list,
                    gas_used: U256::from(gas_used),
                    error: Some(err.to_string()),
                });
            }

            // transact again to get the exact gas used
            evm.disable_inspector();
            let result = evm.transact(tx_env)?;
            let gas_used = result.result.tx_gas_used();
            let error = BaseEthApiError::ensure_success(result.result).err().map(|e| e.to_string());

            Ok(AccessListResult { access_list, gas_used: U256::from(gas_used), error })
        })
    }
}

/// Executes code on state.
impl BaseEthApi {
    /// Returns the max gas limit that the caller can afford given a transaction environment.
    pub fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        _evm_env: &EvmEnvFor,
        tx_env: &TxEnvFor,
    ) -> Result<u64, BaseEthApiError> {
        base_execution_evm_runtime::caller_gas_allowance(&mut db, tx_env)
            .map_err(BaseEthApiError::from_eth_err)
    }

    /// Executes the `TxEnv` against the given [Database] without committing state
    /// changes.
    pub fn transact<DB>(
        &self,
        db: DB,
        evm_env: EvmEnvFor,
        tx_env: TxEnvFor,
    ) -> Result<ResultAndState<HaltReasonFor>, BaseEthApiError>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>>,
    {
        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        let res = evm.transact(tx_env).map_err(BaseEthApiError::from)?;

        Ok(res)
    }

    /// Executes the [`base_execution_evm_blocks::EvmEnv`] against the given [Database] without committing state
    /// changes.
    pub fn transact_with_inspector<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor,
        tx_env: TxEnvFor,
        inspector: I,
    ) -> Result<ResultAndState<HaltReasonFor>, BaseEthApiError>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>>,
        I: InspectorFor<DB>,
    {
        let mut evm = self.evm_config().evm_with_env_and_inspector(db, evm_env, inspector);
        let res = evm.transact(tx_env).map_err(BaseEthApiError::from)?;

        Ok(res)
    }

    /// Executes the call request at the given [`BlockId`].
    ///
    /// This spawns a new task that obtains the state for the given [`BlockId`] and then transacts
    /// the call [`Self::transact`]. If the future is dropped before the (blocking) transact
    /// call is invoked, then the task is cancelled early, (for example if the request is terminated
    /// early client-side).
    pub fn transact_call_at(
        &self,
        request: BaseTransactionRequest,
        at: BlockId,
        overrides: EvmOverrides,
    ) -> impl Future<Output = Result<ResultAndState<HaltReasonFor>, BaseEthApiError>> + Send {
        async move {
            let guard = CancelOnDrop::default();
            let cancel = guard.clone();
            let this = self.clone();

            let res = self
                .spawn_with_call_at(request, at, overrides, move |db, evm_env, tx_env| {
                    if cancel.is_cancelled() {
                        // callsite dropped the guard
                        return Err(EthApiError::InternalEthError.into());
                    }
                    this.transact(db, evm_env, tx_env)
                })
                .await;
            drop(guard);
            res
        }
    }

    /// Executes the closure with the state that corresponds to the given [`BlockId`] on a new task
    pub fn spawn_with_state_at_block<F, R>(
        &self,
        at: impl Into<BlockId>,
        f: F,
    ) -> impl Future<Output = Result<R, BaseEthApiError>> + Send
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, BaseEthApiError> + Send + 'static,
        R: Send + 'static,
    {
        let at = at.into();
        self.spawn_blocking_io_fut(async move |this| {
            let state = this.state_at_block_id(at).await?;
            let db = State::builder().with_database(state).build();
            f(this, db)
        })
    }

    /// Prepares the state and env for the given [`RpcTxReq`] at the given [`BlockId`] and
    /// executes the closure on a new task returning the result of the closure.
    ///
    /// This returns the configured [`base_execution_evm_blocks::EvmEnv`] for the given [`RpcTxReq`] at
    /// the given [`BlockId`] and with configured call settings: `prepare_call_env`.
    ///
    /// This is primarily used by `eth_call`.
    ///
    /// # Blocking behaviour
    ///
    /// This assumes executing the call is relatively more expensive on IO than CPU because it
    /// transacts a single transaction on an empty in memory database. Because `eth_call`s are
    /// usually allowed to consume a lot of gas, this also allows a lot of memory operations so
    /// we assume this is not primarily CPU bound and instead spawn the call on a regular tokio task
    /// instead, where blocking IO is less problematic.
    pub fn spawn_with_call_at<F, R>(
        &self,
        request: BaseTransactionRequest,
        at: BlockId,
        overrides: EvmOverrides,
        f: F,
    ) -> impl Future<Output = Result<R, BaseEthApiError>> + Send
    where
        F: FnOnce(&mut StateCacheDb, EvmEnvFor, TxEnvFor) -> Result<R, BaseEthApiError>
            + Send
            + 'static,
        R: Send + 'static,
    {
        async move {
            let (evm_env, at) = self.evm_env_at(at).await?;
            self.spawn_with_state_at_block(at, move |this, mut db| {
                let (evm_env, tx_env) =
                    this.prepare_call_env(evm_env, request, &mut db, overrides)?;

                f(&mut db, evm_env, tx_env)
            })
            .await
        }
    }

    /// Retrieves the transaction if it exists and executes it.
    ///
    /// Before the transaction is executed, all previous transaction in the block are applied to the
    /// state by executing them first.
    /// The callback `f` is invoked with the [`ResultAndState`] after the transaction was executed
    /// and the database that points to the beginning of the transaction.
    ///
    /// Note: Implementers should use a threadpool where blocking is allowed, such as
    /// [`BlockingTaskPool`](base_common_runtime_tasks::pool::BlockingTaskPool).
    pub fn spawn_replay_transaction<F, R>(
        &self,
        hash: B256,
        f: F,
    ) -> impl Future<Output = Result<Option<R>, BaseEthApiError>> + Send
    where
        F: FnOnce(
                TransactionInfo,
                ResultAndState<HaltReasonFor>,
                StateCacheDb,
            ) -> Result<R, BaseEthApiError>
            + Send
            + 'static,
        R: Send + 'static,
    {
        async move {
            let (transaction, block) = match self.transaction_and_block(hash).await? {
                None => return Ok(None),
                Some(res) => res,
            };
            let (tx, tx_info) = transaction.split();

            // we need to get the state of the parent block because we're essentially replaying the
            // block the transaction is included in
            let parent_block = block.parent_hash();

            self.spawn_with_state_at_block(parent_block, move |this, mut db| {
                let block_txs = block.transactions_recovered();

                let mut executor = this
                    .evm_config()
                    .executor_for_block(&mut db, block.sealed_block())
                    .map_err(|error| reth_rpc_eth_types::EthApiError::Internal(error.into()))
                    .map_err(BaseEthApiError::from_eth_err)?;
                executor.apply_pre_execution_changes().map_err(BaseEthApiError::from_eth_err)?;

                // replay all transactions prior to the targeted transaction
                for block_tx in block_txs {
                    if block_tx.tx_hash() == tx.tx_hash() {
                        break;
                    }
                    executor
                        .execute_transaction(block_tx)
                        .map_err(BaseEthApiError::from_eth_err)?;
                }

                let tx_env = this.evm_config().tx_env(tx);

                let res = executor.evm_mut().transact(tx_env).map_err(BaseEthApiError::from)?;
                drop(executor);
                f(tx_info, res, db)
            })
            .await
            .map(Some)
        }
    }

    /// Replays all transactions before the target transaction index on the given EVM.
    ///
    /// This executes on a caller provided EVM, so the target transaction can then be run on the
    /// same EVM, keeping any block-scoped EVM state intact. The EVM's inspector configuration is
    /// left untouched; see
    /// [`BaseEthApi::inspect_transaction_in_block`] to replay without inspection and trace the target.
    ///
    /// If the target index is greater than or equal to the iterator length, all transactions are
    /// replayed.
    pub fn replay_transactions_until_with_evm<'a, DB, I, Txs>(
        &self,
        evm: &mut EvmFor<DB, I>,
        transactions: Txs,
        target_tx_index: usize,
    ) -> Result<(), BaseEthApiError>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>> + DatabaseCommit,
        I: InspectorFor<DB>,
        Txs: IntoIterator<Item = Recovered<&'a ProviderTx<BlockchainProvider>>>,
    {
        for (index, tx) in transactions.into_iter().enumerate() {
            if index == target_tx_index {
                // reached the target transaction
                break;
            }

            let tx_env = self.evm_config().tx_env(tx);
            evm.transact_commit(tx_env).map_err(BaseEthApiError::from)?;
        }
        Ok(())
    }

    ///
    /// All `TxEnv` fields are derived from the given [`RpcTxReq`], if fields are
    /// `None`, they fall back to the [`base_execution_evm_blocks::EvmEnv`]'s settings.
    pub fn create_txn_env(
        &self,
        evm_env: &EvmEnvFor,
        mut request: BaseTransactionRequest,
        mut db: impl Database<Error: Into<EthApiError>>,
    ) -> Result<TxEnvFor, BaseEthApiError> {
        if request.as_ref().nonce().is_none() {
            let nonce = db
                .basic(request.as_ref().from().unwrap_or_default())
                .map_err(Into::into)?
                .map(|acc| acc.nonce)
                .unwrap_or_default();
            request.as_mut().set_nonce(nonce);
        }

        Ok(self.converter().tx_env(request, evm_env)?)
    }

    /// Prepares the [`base_execution_evm_blocks::EvmEnv`] for execution of calls.
    ///
    /// Does not commit any changes to the underlying database.
    ///
    /// ## EVM settings
    ///
    /// This modifies certain EVM settings to mirror geth's `SkipAccountChecks` when transacting requests, see also: <https://github.com/ethereum/go-ethereum/blob/380688c636a654becc8f114438c2a5d93d2db032/core/state_transition.go#L145-L148>:
    ///
    ///  - `disable_eip3607` is set to `true`
    ///  - `disable_base_fee` is set to `true`
    ///  - `nonce` is set to `None`
    ///
    /// In addition, this changes the block's gas limit to the configured [`Self::call_gas_limit`].
    #[expect(clippy::type_complexity)]
    pub fn prepare_call_env<DB>(
        &self,
        mut evm_env: EvmEnvFor,
        mut request: BaseTransactionRequest,
        db: &mut DB,
        overrides: EvmOverrides,
    ) -> Result<(EvmEnvFor, TxEnvFor), BaseEthApiError>
    where
        DB: Database + DatabaseCommit + OverrideBlockHashes,
        EthApiError: From<<DB as Database>::Error>,
    {
        // track whether the request has a gas limit set
        let request_has_gas_limit = request.as_ref().gas_limit().is_some();

        if let Some(requested_gas) = request.as_ref().gas_limit() {
            let global_gas_cap = self.call_gas_limit();
            if global_gas_cap != 0 && global_gas_cap < requested_gas {
                warn!(target: "rpc::eth::call", ?request, ?global_gas_cap, "Capping gas limit to global gas cap");
                request.as_mut().set_gas_limit(global_gas_cap);
            }
        } else {
            // cap request's gas limit to call gas limit
            request.as_mut().set_gas_limit(self.call_gas_limit());
        }

        // Disable block gas limit check to allow executing transactions with higher gas limit (call
        // gas limit): https://github.com/paradigmxyz/reth/issues/18577
        evm_env.cfg_env.disable_block_gas_limit = true;

        // Disabled because eth_call is sometimes used with eoa senders
        // See <https://github.com/paradigmxyz/reth/issues/1959>
        evm_env.cfg_env.disable_eip3607 = true;

        // The basefee should be ignored for eth_call
        // See:
        // <https://github.com/ethereum/go-ethereum/blob/ee8e83fa5f6cb261dad2ed0a7bbcde4930c41e6c/internal/ethapi/api.go#L985>
        evm_env.cfg_env.disable_base_fee = true;

        // Disable EIP-7825 transaction gas limit to support larger transactions
        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);

        // Disable additional fee charges, e.g. opstack operator fee charge
        // See:
        // <https://github.com/paradigmxyz/reth/issues/18470>
        evm_env.cfg_env.disable_fee_charge = true;

        evm_env.cfg_env.memory_limit = self.evm_memory_limit();

        // set nonce to None so that the correct nonce is chosen by the EVM
        request.as_mut().take_nonce();

        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, db, evm_env.block_env.inner_mut());
        }
        if let Some(state_overrides) = overrides.state {
            apply_state_overrides(state_overrides, db)
                .map_err(EthApiError::from_state_overrides_err)?;
        }

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut *db)?;

        // lower the basefee to 0 to avoid breaking EVM invariants (basefee < gasprice): <https://github.com/ethereum/go-ethereum/blob/355228b011ef9a85ebc0f21e7196f892038d49f0/internal/ethapi/api.go#L700-L704>
        if tx_env.gas_price() == 0 {
            evm_env.block_env.inner_mut().basefee = 0;
        }

        if !request_has_gas_limit {
            // No gas limit was provided in the request, so we need to cap the transaction gas limit
            if tx_env.gas_price() > 0 {
                // If gas price is specified, cap transaction gas limit with caller allowance
                trace!(target: "rpc::eth::call", ?tx_env, "Applying gas limit cap with caller allowance");
                let cap = self.caller_gas_allowance(db, &evm_env, &tx_env)?;
                // ensure we cap gas_limit to the block's
                tx_env.set_gas_limit(cap.min(evm_env.block_env.gas_limit()));
            }
        }

        Ok((evm_env, tx_env))
    }
}
