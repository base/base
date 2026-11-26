//! Hook through which an external archive serves history the node has pruned to the node's
//! own execution and tracing paths.
//!
//! Unset, [`BaseEthApi`] behaves exactly as reth's defaults. Set, executions started through
//! `Call::spawn_with_state_at_block` (`eth_call`, `eth_callMany`, `trace_*`, `debug_trace*`)
//! run over the hook's state when the node refuses its own as pruned, the blocks returned by
//! `recovered_block` and `recovered_block_and_maybe_bal` fall back to the hook when the node
//! has pruned their bodies, and `transaction_and_block_and_maybe_bal` places transactions the
//! node no longer can. State reads outside executions (`eth_getBalance` and friends) never
//! consult the hook. Every hook failure is JSON-RPC 4444, never a wrong answer.

use std::{fmt, sync::Arc, time::Instant};

use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use futures::future::BoxFuture;
use jsonrpsee_types::ErrorObject;
use reth_primitives_traits::{Block, RecoveredBlock};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_eth_api::{
    AsEthApiError, EthApiTypes, FromEthApiError, RpcConvert, RpcNodeCore,
    helpers::{LoadState, SpawnBlocking},
};
use reth_rpc_eth_types::{EthApiError, StateCacheDb, error::ToRpcError};
use reth_storage_api::{
    BlockHashReader, BlockIdReader, BlockNumReader, ProviderBlock, StateProvider, StateProviderBox,
    StateProviderFactory, errors::ProviderError,
};

use crate::BaseEthApi;

/// Why pruned history could not be served; always JSON-RPC 4444 (EIP-4444).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("pruned history unavailable: {reason}")]
pub struct PrunedHistoryError {
    /// What failed.
    pub reason: String,
}

impl PrunedHistoryError {
    /// Creates an error with `reason`.
    pub fn new(reason: impl fmt::Display) -> Self {
        Self { reason: reason.to_string() }
    }
}

impl ToRpcError for PrunedHistoryError {
    /// JSON-RPC 4444 with this error's message.
    fn to_rpc_error(&self) -> ErrorObject<'static> {
        ErrorObject::owned(
            alloy_rpc_types_eth::error::EthRpcErrorCode::PrunedHistory.code(),
            self.to_string(),
            None::<()>,
        )
    }
}

/// State at the end of a pruned block, served by the archive for one execution.
pub struct PrunedState {
    /// State reads at the end of the block.
    pub state: StateProviderBox,
    /// Awaited once the execution (and any trace output built from it) is done. Fails when
    /// any archive read of the execution failed, even one the EVM or an inspector swallowed,
    /// when the archive pruned the block during the execution, or past `deadline`.
    pub conclude: BoxFuture<'static, Result<(), PrunedHistoryError>>,
    /// When the whole execution, from its start through its conclusion, must be done.
    pub deadline: Instant,
}

impl fmt::Debug for PrunedState {
    /// The deadline; the state and conclusion are opaque.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrunedState").field("deadline", &self.deadline).finish_non_exhaustive()
    }
}

/// History the node has pruned, served from an external archive.
pub trait PrunedHistory<B: Block>: Send + Sync + fmt::Debug + 'static {
    /// State at the end of canonical block `number` (hash `hash`), whose local state the node
    /// refused as pruned, for an execution that started at `started`. The returned deadline
    /// bounds the execution from `started`.
    fn state_at(
        &self,
        number: u64,
        hash: B256,
        started: Instant,
    ) -> BoxFuture<'_, Result<PrunedState, PrunedHistoryError>>;

    /// Verified block `number` (hash `hash`) with senders when the node has pruned its body;
    /// `Ok(None)` when the node holds it.
    fn recovered_block(
        &self,
        number: u64,
        hash: B256,
    ) -> BoxFuture<'_, Result<Option<Arc<RecoveredBlock<B>>>, PrunedHistoryError>>;

    /// Block number, block hash and index of mined transaction `hash`, which the node can no
    /// longer place; `Ok(None)` only when the hash is verifiably not a mined transaction.
    fn transaction_location(
        &self,
        hash: B256,
    ) -> BoxFuture<'_, Result<Option<TransactionLocation>, PrunedHistoryError>>;
}

/// The hook type a [`BaseEthApi`] over provider `P` holds.
pub type PrunedHistoryFor<P> = Arc<dyn PrunedHistory<ProviderBlock<P>>>;

/// Where a mined transaction sits: block number, block hash and index in the block.
pub type TransactionLocation = (u64, B256, u64);

/// [`BaseEthApi::set_pruned_history`] was called twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a pruned-history hook is already installed")]
pub struct PrunedHistoryAlreadySet;

/// Outcome of an execution's attempt on the node's own state.
enum LocalAttempt<R, F> {
    /// The execution ran locally.
    Ran(R),
    /// The node refused canonical block `number` (hash `hash`) as pruned before the
    /// execution started; the execution `f` is handed back unused.
    Refused {
        /// The unused execution.
        f: F,
        /// Canonical number of the refused block.
        number: u64,
        /// Canonical hash of the refused block.
        hash: B256,
    },
}

impl<N: RpcNodeCore, Rpc: RpcConvert> BaseEthApi<N, Rpc> {
    /// Installs the pruned-history hook. It can be set once; every handle sharing this API's
    /// state (including the `debug_` and `trace_` handlers) sees it.
    pub fn set_pruned_history(
        &self,
        hook: PrunedHistoryFor<N::Provider>,
    ) -> Result<(), PrunedHistoryAlreadySet> {
        self.inner.pruned.set(hook).map_err(|_| PrunedHistoryAlreadySet)
    }

    /// The installed pruned-history hook, if any.
    pub fn pruned_history(&self) -> Option<&PrunedHistoryFor<N::Provider>> {
        self.inner.pruned.get()
    }

    /// Whether `err` is the node's state provider refusing history as pruned. reth checks
    /// both history domains when the provider's first read initializes it.
    pub const fn refuses_pruned_state(err: &ProviderError) -> bool {
        matches!(
            err,
            ProviderError::StateAtBlockPruned(_) | ProviderError::InsufficientChangesets { .. }
        )
    }

    /// The EVM database reth builds over `state` for an execution.
    pub fn execution_db(state: StateProviderBox) -> StateCacheDb {
        State::builder()
            .with_database(StateProviderDatabase::new(state.into_evm_state_provider()))
            .build()
    }
}

impl<N, Rpc> BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
    Self: EthApiTypes,
{
    /// Converts a hook failure into this API's error (4444).
    pub fn pruned_error(err: PrunedHistoryError) -> <Self as EthApiTypes>::Error {
        <Self as EthApiTypes>::Error::from_eth_err(EthApiError::other(err))
    }

    /// Whether `err` is the node refusing history as pruned (`BlockExpired` and friends).
    pub fn is_prune_refusal(err: &<Self as EthApiTypes>::Error) -> bool {
        matches!(err.as_err(), Some(EthApiError::PrunedHistoryUnavailable { .. }))
    }

    /// The hook, when it is installed and the local answer `local` is a miss or a prune
    /// refusal that it may replace.
    pub fn block_fallback<T>(
        &self,
        local: &Result<Option<T>, <Self as EthApiTypes>::Error>,
    ) -> Option<PrunedHistoryFor<N::Provider>> {
        let replaceable = match local {
            Ok(None) => true,
            Err(err) => Self::is_prune_refusal(err),
            Ok(Some(_)) => false,
        };
        self.pruned_history().filter(|_| replaceable).cloned()
    }

    /// Canonical number and hash of `at` from the node's headers (never pruned); `None` for
    /// pending, an unknown block or a non-canonical hash.
    pub fn canonical_block(
        &self,
        at: BlockId,
    ) -> Result<Option<(u64, B256)>, <Self as EthApiTypes>::Error> {
        let provider = self.provider();
        let number = match at {
            BlockId::Number(tag) if tag.is_pending() => return Ok(None),
            BlockId::Number(tag) => provider.convert_block_number(tag),
            BlockId::Hash(hash) => provider.block_number(hash.block_hash),
        }
        .map_err(<Self as EthApiTypes>::Error::from_eth_err)?;
        let Some(number) = number else { return Ok(None) };
        let hash =
            provider.block_hash(number).map_err(<Self as EthApiTypes>::Error::from_eth_err)?;
        Ok(match (at, hash) {
            (BlockId::Hash(named), Some(hash)) if named.block_hash != hash => None,
            (_, hash) => hash.map(|hash| (number, hash)),
        })
    }

    /// The hook's copy of block `number`/`hash`, `None` when the node holds it.
    pub async fn pruned_block(
        hook: &PrunedHistoryFor<N::Provider>,
        number: u64,
        hash: B256,
    ) -> Result<Option<Arc<RecoveredBlock<ProviderBlock<N::Provider>>>>, <Self as EthApiTypes>::Error>
    {
        let block = hook.recovered_block(number, hash).await.map_err(Self::pruned_error)?;
        if let Some(block) = &block
            && (block.number() != number || block.hash() != hash)
        {
            return Err(Self::pruned_error(PrunedHistoryError::new(format!(
                "archived block {} ({}) is not the requested block {number} ({hash})",
                block.number(),
                block.hash()
            ))));
        }
        Ok(block)
    }
}

impl<N, Rpc> BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
    Self: LoadState + SpawnBlocking,
{
    /// Execution `f` as a closure over a state provider, as reth's default
    /// `Call::spawn_with_state_at_block` adapts it.
    pub fn over_state<F, R>(
        f: F,
    ) -> impl FnOnce(Self, StateProviderBox) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static,
        R: Send + 'static,
    {
        move |this, state| f(this, Self::execution_db(state))
    }

    /// Runs execution `f` over the state at `at`, as `Call::spawn_with_state_at_block`.
    ///
    /// Unset hook, `pending` and `latest`: reth's default. Otherwise the node's state is
    /// opened and read once on the blocking pool before `f` runs, which makes reth's
    /// provider decide (from fresh account and storage history checkpoints) whether the
    /// state is pruned. A typed prune refusal hands `f` back unused and it runs on the
    /// hook's state instead (see [`Self::archived_task`]); any other local error is returned
    /// unchanged.
    pub fn execution_task<F, R>(
        &self,
        at: BlockId,
        f: F,
    ) -> impl Future<Output = Result<R, <Self as EthApiTypes>::Error>> + Send
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static,
        R: Send + 'static,
    {
        let hook = self.pruned_history().filter(|_| !at.is_pending() && !at.is_latest()).cloned();
        let this = self.clone();
        async move {
            let Some(hook) = hook else {
                return this.spawn_blocking_io_with_state(at, Self::over_state(f)).await;
            };
            let started = Instant::now();
            let (f, number, hash) = match this.local_attempt(at, f).await? {
                LocalAttempt::Ran(result) => return Ok(result),
                LocalAttempt::Refused { f, number, hash } => (f, number, hash),
            };
            let state = hook.state_at(number, hash, started).await.map_err(Self::pruned_error)?;
            this.archived_task(number, state, f).await
        }
    }

    /// Runs execution `f` over the node's state at `at` on the blocking pool, unless the
    /// node refuses that state as pruned: the provider is opened and read once first, which
    /// makes reth decide from fresh account and storage history checkpoints, so a refusal
    /// hands `f` back unused. Other local errors are returned unchanged.
    fn local_attempt<F, R>(
        &self,
        at: BlockId,
        f: F,
    ) -> impl Future<Output = Result<LocalAttempt<R, F>, <Self as EthApiTypes>::Error>> + Send
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static,
        R: Send + 'static,
    {
        self.spawn_blocking_io(move |this: Self| {
            let local = this
                .provider()
                .state_by_block_id(at)
                .and_then(|state| state.basic_account(&Address::ZERO).map(|_| state));
            match local {
                Ok(state) => Ok(LocalAttempt::Ran(f(this, Self::execution_db(state))?)),
                Err(err) if Self::refuses_pruned_state(&err) => match this.canonical_block(at)? {
                    Some((number, hash)) => Ok(LocalAttempt::Refused { f, number, hash }),
                    None => Err(<Self as EthApiTypes>::Error::from_eth_err(err)),
                },
                Err(err) => Err(<Self as EthApiTypes>::Error::from_eth_err(err)),
            }
        })
    }

    /// Runs execution `f` over the hook's `state` at block `number`. The wait for the blocking
    /// pool, the execution and the conclusion all end by the state's deadline; past it the
    /// answer is 4444 (dropping the task cancels the execution and its in-flight reads), and
    /// an execution that finished late is never a success. A failed conclusion wins over
    /// whatever `f` produced.
    pub async fn archived_task<F, R>(
        &self,
        number: u64,
        state: PrunedState,
        f: F,
    ) -> Result<R, <Self as EthApiTypes>::Error>
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static,
        R: Send + 'static,
    {
        let PrunedState { state, conclude, deadline } = state;
        let late = || {
            Self::pruned_error(PrunedHistoryError::new(format!(
                "execution over archived block {number} exceeded its deadline"
            )))
        };
        let task = self.spawn_blocking_io(move |this: Self| f(this, Self::execution_db(state)));
        let result = tokio::time::timeout_at(deadline.into(), task).await.map_err(|_| late())?;
        conclude.await.map_err(Self::pruned_error)?;
        if Instant::now() > deadline {
            return Err(late());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use alloy_consensus::{Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Address, Signature, U256};
    use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope};
    use base_common_rpc_types::BaseRpcTypes;
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_execution_txpool::BasePooledTransaction;
    use reth_network_api::noop::NoopNetwork;
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_rpc::EthApi;
    use reth_rpc_convert::RpcConverter;
    use reth_rpc_eth_api::{
        helpers::{LoadBlock, LoadTransaction},
        node::RpcNodeCoreAdapter,
    };
    use reth_rpc_eth_types::TransactionSource;
    use reth_tasks::Runtime;
    use reth_transaction_pool::noop::NoopTransactionPool;

    use super::*;
    use crate::{
        BaseEthApiError, BaseTimeCache,
        eth::{receipt::BaseReceiptConverter, transaction::BaseTxInfoMapper},
    };

    type TestProvider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
    type TestEvmConfig = BaseEvmConfig<BaseChainSpec>;
    type TestNode = RpcNodeCoreAdapter<
        TestProvider,
        NoopTransactionPool<BasePooledTransaction>,
        NoopNetwork,
        TestEvmConfig,
    >;
    type TestRpcConvert = RpcConverter<
        BaseRpcTypes,
        TestEvmConfig,
        BaseReceiptConverter<TestProvider>,
        (),
        BaseTxInfoMapper<TestProvider>,
    >;
    type TestEthApi = BaseEthApi<TestNode, TestRpcConvert>;

    /// The account whose balance tells local state from hook state.
    const ACCOUNT: Address = Address::repeat_byte(0xaa);
    /// Its balance in the node's state.
    const LOCAL_BALANCE: u64 = 5;
    /// Its balance in the hook's state.
    const ARCHIVED_BALANCE: u64 = 9;

    /// A fake archive: what it answers and which calls it saw.
    #[derive(Debug, Default)]
    struct FakeHistory {
        /// Conclusion of served state (`Ok` when unset).
        state: Option<Result<(), PrunedHistoryError>>,
        /// Block to serve instead of `None`.
        block: Option<RecoveredBlock<BaseBlock>>,
        /// Location to serve instead of `None`.
        location: Option<TransactionLocation>,
        /// Calls seen, in order.
        calls: Mutex<Vec<String>>,
    }

    impl FakeHistory {
        /// Records `call`.
        fn saw(&self, call: String) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl PrunedHistory<BaseBlock> for FakeHistory {
        fn state_at(
            &self,
            number: u64,
            hash: B256,
            started: Instant,
        ) -> BoxFuture<'_, Result<PrunedState, PrunedHistoryError>> {
            self.saw(format!("state_at {number} {hash}"));
            let conclusion = self.state.clone().unwrap_or(Ok(()));
            Box::pin(async move {
                Ok(PrunedState {
                    state: Box::new(archived_state()),
                    conclude: Box::pin(async move { conclusion }),
                    deadline: started + Duration::from_secs(5),
                })
            })
        }

        fn recovered_block(
            &self,
            number: u64,
            hash: B256,
        ) -> BoxFuture<'_, Result<Option<Arc<RecoveredBlock<BaseBlock>>>, PrunedHistoryError>>
        {
            self.saw(format!("recovered_block {number} {hash}"));
            let block = self.block.clone().map(Arc::new);
            Box::pin(async move { Ok(block) })
        }

        fn transaction_location(
            &self,
            hash: B256,
        ) -> BoxFuture<'_, Result<Option<TransactionLocation>, PrunedHistoryError>> {
            self.saw(format!("transaction_location {hash}"));
            let location = self.location;
            Box::pin(async move { Ok(location) })
        }
    }

    /// An API over a mock node holding genesis (with [`ACCOUNT`] funded) and the header, but
    /// not the body, of block 1. Returns the API, the genesis hash and block 1.
    fn api(hook: Option<Arc<FakeHistory>>) -> (TestEthApi, B256, RecoveredBlock<BaseBlock>) {
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        let provider = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        provider.add_account(ACCOUNT, ExtendedAccount::new(0, U256::from(LOCAL_BALANCE)));
        let genesis = provider.block_hash(0).expect("read").expect("genesis");
        let tx = TxLegacy { nonce: 3, gas_limit: 21_000, ..Default::default() };
        let tx: BaseTxEnvelope = tx.into_signed(Signature::test_signature()).into();
        let header = Header { number: 1, parent_hash: genesis, ..Default::default() };
        let block = BaseBlock {
            header: header.clone(),
            body: alloy_consensus::BlockBody { transactions: vec![tx], ..Default::default() },
        };
        let hash = header.hash_slow();
        provider.add_header(hash, header);
        let block = RecoveredBlock::new_sealed(
            SealedBlock::new_unchecked(block, hash),
            vec![Address::repeat_byte(0x11)],
        );
        let evm_config = BaseEvmConfig::base(chain_spec);
        let base_time = BaseTimeCache::default();
        let converter =
            RpcConverter::new(BaseReceiptConverter::new(provider.clone(), base_time.clone()))
                .with_mapper(BaseTxInfoMapper::new(provider.clone(), base_time.clone()));
        let inner = EthApi::builder(
            provider,
            NoopTransactionPool::<BasePooledTransaction>::new(),
            NoopNetwork::default(),
            evm_config,
        )
        .with_rpc_converter(converter)
        .task_spawner(Runtime::test())
        .build_inner();
        let api = BaseEthApi::new(inner, None, U256::ZERO, base_time);
        if let Some(hook) = hook {
            api.set_pruned_history(hook).expect("first hook");
        }
        (api, genesis, block)
    }

    /// State the fake archive serves: [`ACCOUNT`] with [`ARCHIVED_BALANCE`].
    fn archived_state() -> MockEthProvider<BasePrimitives> {
        let archived = MockEthProvider::<BasePrimitives>::new();
        archived.add_account(ACCOUNT, ExtendedAccount::new(0, U256::from(ARCHIVED_BALANCE)));
        archived
    }

    /// Balance of [`ACCOUNT`] read by an execution `api` runs at `at`.
    async fn balance(api: &TestEthApi, at: BlockId) -> Result<Option<U256>, BaseEthApiError> {
        api.execution_task(at, |_, mut db| {
            Ok(revm::Database::basic(&mut db, ACCOUNT)
                .map_err(BaseEthApiError::from_eth_err)?
                .map(|account| account.balance))
        })
        .await
    }

    /// Balance of [`ACCOUNT`] read by an execution over archived state with `conclusion`,
    /// whose deadline is `deadline` and which takes `takes` to run.
    async fn archived_balance(
        api: &TestEthApi,
        conclusion: Result<(), PrunedHistoryError>,
        deadline: Instant,
        takes: Duration,
    ) -> Result<Option<U256>, BaseEthApiError> {
        let state = PrunedState {
            state: Box::new(archived_state()),
            conclude: Box::pin(async move { conclusion }),
            deadline,
        };
        api.archived_task(1, state, move |_, mut db| {
            std::thread::sleep(takes);
            Ok(revm::Database::basic(&mut db, ACCOUNT)
                .map_err(BaseEthApiError::from_eth_err)?
                .map(|account| account.balance))
        })
        .await
    }

    /// The JSON-RPC code of `err`.
    fn code(err: BaseEthApiError) -> i32 {
        ErrorObject::from(err).code()
    }

    /// Executions run over the node's state whenever it serves it: unset hook, `latest`,
    /// and a retained block never ask the hook. A second hook is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn executions_over_retained_state_never_ask_the_hook() {
        let (unset, genesis, _) = api(None);
        assert_eq!(
            balance(&unset, BlockId::number(0)).await.expect("local state"),
            Some(U256::from(LOCAL_BALANCE)),
            "unset: the node's state"
        );

        let hook = Arc::new(FakeHistory::default());
        let (api, _, _) = api(Some(Arc::clone(&hook)));
        for at in [BlockId::number(0), BlockId::hash(genesis), BlockId::latest()] {
            assert_eq!(
                balance(&api, at).await.expect("local state"),
                Some(U256::from(LOCAL_BALANCE)),
                "{at:?}: the node serves its own state"
            );
        }
        assert!(
            hook.calls.lock().expect("calls").is_empty(),
            "a node serving its state never asks the hook"
        );
        assert_eq!(
            api.set_pruned_history(Arc::new(FakeHistory::default())),
            Err(PrunedHistoryAlreadySet),
            "the hook is set once"
        );
    }

    /// Only typed history refusals hand an execution to the archive.
    #[test]
    fn only_history_refusals_go_to_the_archive() {
        assert!(TestEthApi::refuses_pruned_state(&ProviderError::StateAtBlockPruned(3)), "pruned");
        assert!(
            TestEthApi::refuses_pruned_state(&ProviderError::InsufficientChangesets {
                requested: 3,
                available: 4..=9
            }),
            "changesets missing"
        );
        assert!(
            !TestEthApi::refuses_pruned_state(&ProviderError::BestBlockNotFound),
            "an unrelated provider error stays the node's"
        );
    }

    /// Over archived state: the archive's state is read; a failed conclusion is 4444 even
    /// though the execution succeeded; an execution outliving its deadline is 4444 without
    /// waiting for it; and one that finishes after its deadline is never a success.
    #[tokio::test(flavor = "multi_thread")]
    async fn archived_executions_conclude_within_the_deadline() {
        let (api, _, _) = api(None);
        let soon = || Instant::now() + Duration::from_secs(5);
        assert_eq!(
            archived_balance(&api, Ok(()), soon(), Duration::ZERO).await.expect("archived"),
            Some(U256::from(ARCHIVED_BALANCE)),
            "the archive's state is executed over"
        );

        let err = archived_balance(
            &api,
            Err(PrunedHistoryError::new("store down")),
            soon(),
            Duration::ZERO,
        )
        .await
        .expect_err("a failed conclusion fails the execution");
        assert_eq!(err.to_string(), "pruned history unavailable: store down", "reason kept");
        assert_eq!(code(err), 4444, "an archive failure is 4444");

        let started = Instant::now();
        let err = archived_balance(
            &api,
            Ok(()),
            Instant::now() + Duration::from_millis(100),
            Duration::from_secs(3),
        )
        .await
        .expect_err("a late execution fails");
        assert!(started.elapsed() < Duration::from_secs(2), "the answer does not wait for it");
        assert_eq!(
            err.to_string(),
            "pruned history unavailable: execution over archived block 1 exceeded its deadline",
            "the deadline is named"
        );
        assert_eq!(code(err), 4444, "a late execution is 4444");

        let deadline = Instant::now() + Duration::from_millis(50);
        let late = PrunedState {
            state: Box::new(archived_state()),
            conclude: Box::pin(async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(())
            }),
            deadline,
        };
        let err = api
            .archived_task(1, late, |_, _| Ok(()))
            .await
            .expect_err("a conclusion past the deadline fails");
        assert_eq!(code(err), 4444, "no late success");
    }

    /// A block whose body the node lacks is the hook's copy only when one is installed and
    /// it answers; a copy that is not the requested block is 4444.
    #[tokio::test(flavor = "multi_thread")]
    async fn missing_bodies_follow_the_hook() {
        let (unset, _, block) = api(None);
        let id = BlockId::number(1);
        assert!(unset.recovered_block(id).await.expect("local").is_none(), "unset: no body");

        let declining = Arc::new(FakeHistory::default());
        let (api_none, _, _) = api(Some(Arc::clone(&declining)));
        assert!(api_none.recovered_block(id).await.expect("local").is_none(), "hook None: none");
        assert_eq!(
            *declining.calls.lock().expect("calls"),
            vec![format!("recovered_block 1 {}", block.hash())],
            "asked for the canonical number and hash"
        );

        let serving = Arc::new(FakeHistory { block: Some(block.clone()), ..Default::default() });
        let (api_some, _, _) = api(Some(serving));
        let served = api_some.recovered_block(id).await.expect("archived").expect("block");
        assert_eq!(served.hash(), block.hash(), "the hook's block is served");
        let (served, bal) = api_some
            .recovered_block_and_maybe_bal(BlockId::hash(block.hash()))
            .await
            .expect("archived")
            .expect("block");
        assert_eq!(served.hash(), block.hash(), "the hook's block is served by hash");
        assert!(bal.is_none(), "an archived block has no cached BAL");
        assert!(
            api_some.recovered_block(BlockId::number(2)).await.expect("unknown").is_none(),
            "a block the node has no header for never asks the hook"
        );

        let mut other = block.clone();
        other.set_block_number(7);
        let (api_wrong, _, _) =
            api(Some(Arc::new(FakeHistory { block: Some(other), ..Default::default() })));
        let err = api_wrong.recovered_block(id).await.expect_err("wrong block");
        assert_eq!(code(err), 4444, "a block that is not the requested one is 4444");
    }

    /// A transaction the node cannot place is located by the hook and read from the hook's
    /// block, with no BAL; a location whose index holds another transaction is 4444; unset,
    /// the node's miss stands.
    #[tokio::test(flavor = "multi_thread")]
    async fn unplaced_transactions_follow_the_hook() {
        let (unset, _, block) = api(None);
        let tx_hash = block.body().transactions[0].tx_hash();
        assert!(
            unset.transaction_and_block_and_maybe_bal(tx_hash).await.expect("local").is_none(),
            "unset: the node's miss"
        );

        let serving = Arc::new(FakeHistory {
            block: Some(block.clone()),
            location: Some((1, block.hash(), 0)),
            ..Default::default()
        });
        let (api_some, _, _) = api(Some(Arc::clone(&serving)));
        let (source, served, bal) = api_some
            .transaction_and_block_and_maybe_bal(tx_hash)
            .await
            .expect("archived")
            .expect("located");
        assert_eq!(served.hash(), block.hash(), "the hook's block");
        assert!(bal.is_none(), "no BAL for an archived block");
        match source {
            TransactionSource::Block { index, block_number, block_hash, transaction, .. } => {
                assert_eq!(
                    (index, block_number, block_hash, transaction.tx_hash()),
                    (0, 1, block.hash(), tx_hash),
                    "the located transaction"
                );
                assert_eq!(transaction.signer(), Address::repeat_byte(0x11), "block sender");
            }
            TransactionSource::Pool(_) => panic!("a mined transaction is not pooled"),
        }
        assert_eq!(
            *serving.calls.lock().expect("calls"),
            vec![
                format!("transaction_location {tx_hash}"),
                format!("recovered_block 1 {}", block.hash()),
            ],
            "located, then the block read"
        );

        let (api_wrong, _, _) = api(Some(Arc::new(FakeHistory {
            block: Some(block.clone()),
            location: Some((1, block.hash(), 1)),
            ..Default::default()
        })));
        let err = api_wrong
            .transaction_and_block_and_maybe_bal(tx_hash)
            .await
            .expect_err("index past the block");
        assert_eq!(code(err), 4444, "a location that does not hold the hash is 4444");
    }
}
