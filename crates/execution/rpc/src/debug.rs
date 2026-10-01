//! Historical proofs RPC server implementation for `debug_` namespace.

use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Sealable};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use base_common_chains::Upgrades;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_payload_builder::{
    Attributes, PayloadPrimitives,
    builder::{BasePayloadBuilderCtx, Builder},
};
use base_execution_trie::{BaseProofsStorage, BaseProofsStore};
use base_execution_txpool::BasePooledTransaction;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::error::ErrorObject;
use reth_basic_payload_builder::PayloadConfig;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_node_api::{BuildNextEnv, NodePrimitives, PayloadBuilderError};
use reth_payload_util::NoopPayloadTransactions;
use reth_primitives_traits::{Block, SealedHeader, TxTy};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, HeaderProvider, NodePrimitivesProvider, ProviderError,
    ProviderResult, StateProviderFactory,
};
use reth_revm::{
    State, cancelled::CancelOnDrop, database::StateProviderDatabase,
    witness::ExecutionWitnessRecord,
};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_rpc_server_types::{ToRpcResult, result::internal_rpc_err};
use reth_tasks::Runtime;
use reth_trie_common::ExecutionWitnessMode;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    time::MissedTickBehavior,
};
use tracing::{debug, warn};

use crate::{
    CanonicalPayloadAttributes, WitnessCache, WitnessCacheConfig,
    metrics::{DebugApiExtMetrics, DebugApis, WitnessCacheMetrics},
    state::BaseStateProviderFactory,
};

/// Payload version used to derive payload IDs for `debug_executePayload`.
const EXECUTE_PAYLOAD_VERSION: u8 = 3;

/// How often the witness cache builder polls the proofs storage tip.
const WITNESS_CACHE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum number of blocks waiting to be prebuilt; the oldest are dropped beyond this.
const WITNESS_CACHE_MAX_PENDING_BUILDS: usize = 64;

/// Number of times the witness cache builder attempts a block before giving up on it.
const WITNESS_CACHE_MAX_BUILD_ATTEMPTS: u32 = 3;

/// Represents the current proofs sync status.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProofsSyncStatus {
    /// The earliest block number for which proofs are available.
    earliest: Option<u64>,
    /// The latest block number for which proofs are available.
    latest: Option<u64>,
}

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait DebugApiOverride<Attributes> {
    /// Executes a payload and returns the execution witness.
    #[method(name = "executePayload")]
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: Attributes,
    ) -> RpcResult<ExecutionWitness>;

    /// Returns the execution witness for a given block.
    #[method(name = "executionWitness")]
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness>;

    /// Returns the current proofs sync status.
    #[method(name = "proofsSyncStatus")]
    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus>;
}

#[derive(Debug)]
/// Overrides applied to the `debug_` namespace of the RPC API for the proofs `ExEx`.
pub struct DebugApiExt<Eth: FullEthApi, Storage, Provider, EvmConfig, Attrs> {
    inner: Arc<DebugApiExtInner<Eth, Storage, Provider, EvmConfig, Attrs>>,
}

impl<Eth, Storage, Provider, EvmConfig, Attrs> DebugApiExt<Eth, Storage, Provider, EvmConfig, Attrs>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    Storage: BaseProofsStore + Clone + 'static,
    Provider: BlockReaderIdExt + NodePrimitivesProvider<Primitives: PayloadPrimitives>,
    EvmConfig: ConfigureEvm<Primitives = Provider::Primitives> + 'static,
{
    /// Creates a new instance of the `DebugApiExt`.
    pub fn new(
        provider: Provider,
        eth_api: Eth,
        preimage_store: BaseProofsStorage<Storage>,
        task_spawner: Runtime,
        evm_config: EvmConfig,
        witness_cache: Option<Arc<WitnessCache>>,
    ) -> Self {
        Self {
            inner: Arc::new(DebugApiExtInner::new(
                provider,
                eth_api,
                preimage_store,
                task_spawner,
                evm_config,
                witness_cache,
            )),
        }
    }
}

impl<Eth: FullEthApi, Storage, Provider, EvmConfig, Attrs> Clone
    for DebugApiExt<Eth, Storage, Provider, EvmConfig, Attrs>
{
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

#[derive(Debug)]
/// Overrides applied to the `debug_` namespace of the RPC API for historical proofs `ExEx`.
pub struct DebugApiExtInner<Eth: FullEthApi, Storage, Provider, EvmConfig, Attrs> {
    provider: Provider,
    eth_api: Eth,
    storage: BaseProofsStorage<Storage>,
    state_provider_factory: BaseStateProviderFactory<Eth, Storage>,
    evm_config: EvmConfig,
    task_spawner: Runtime,
    semaphore: Semaphore,
    witness_cache: Option<Arc<WitnessCache>>,
    _attrs: PhantomData<Attrs>,
}

impl<Eth, P, Provider, EvmConfig, Attrs> DebugApiExtInner<Eth, P, Provider, EvmConfig, Attrs>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
    Provider: NodePrimitivesProvider<Primitives: PayloadPrimitives>,
{
    fn new(
        provider: Provider,
        eth_api: Eth,
        storage: BaseProofsStorage<P>,
        task_spawner: Runtime,
        evm_config: EvmConfig,
        witness_cache: Option<Arc<WitnessCache>>,
    ) -> Self {
        Self {
            provider,
            storage: storage.clone(),
            state_provider_factory: BaseStateProviderFactory::new(eth_api.clone(), storage),
            eth_api,
            evm_config,
            task_spawner,
            semaphore: Semaphore::new(3),
            witness_cache,
            _attrs: PhantomData,
        }
    }
}

impl<Eth, P, Provider, EvmConfig, Attrs> DebugApiExt<Eth, P, Provider, EvmConfig, Attrs>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
    Provider: BlockReaderIdExt
        + NodePrimitivesProvider<Primitives: PayloadPrimitives>
        + HeaderProvider<Header = <Provider::Primitives as NodePrimitives>::BlockHeader>,
{
    fn parent_header(
        &self,
        parent_block_hash: B256,
    ) -> ProviderResult<SealedHeader<Provider::Header>> {
        self.inner
            .provider
            .sealed_header_by_hash(parent_block_hash)?
            .ok_or_else(|| ProviderError::HeaderNotFound(parent_block_hash.into()))
    }
}

impl<Eth, P, Provider, EvmConfig, Attrs, N> DebugApiExt<Eth, P, Provider, EvmConfig, Attrs>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
    Attrs: Attributes<Transaction = TxTy<EvmConfig::Primitives>>,
    N: PayloadPrimitives<_TX = base_common_consensus::BaseTransactionSigned>,
    EvmConfig: ConfigureEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, Provider::ChainSpec>,
        > + 'static,
    Provider: BlockReaderIdExt<Header = N::BlockHeader>
        + StateProviderFactory
        + ChainSpecProvider<ChainSpec: Upgrades>
        + NodePrimitivesProvider<Primitives = N>
        + HeaderProvider<Header = N::BlockHeader>
        + Clone
        + 'static,
{
    /// Builds `attributes` on top of `parent_header` and returns the resulting witness.
    ///
    /// Shared by `debug_executePayload` and the witness cache builder so that cached witnesses
    /// are identical to computed ones.
    async fn build_witness(
        &self,
        parent_header: SealedHeader<N::BlockHeader>,
        attributes: Attrs,
    ) -> Result<ExecutionWitness, PayloadBuilderError> {
        // Cancels the blocking task if this future is dropped (e.g. the client disconnected).
        let cancel = CancelOnDrop::default();
        let task_cancel = cancel.clone();
        let (tx, rx) = oneshot::channel();
        let this = Arc::clone(&self.inner);
        let eth_api = self.inner.eth_api.provider().clone();
        self.inner.task_spawner.spawn_blocking_task(async move {
            let result = async {
                let parent_hash = parent_header.hash();
                let payload_id = attributes.payload_job_id();

                let config = PayloadConfig::new(Arc::new(parent_header), attributes, payload_id);
                let ctx = BasePayloadBuilderCtx {
                    evm_config: this.evm_config.clone(),
                    chain_spec: this.provider.chain_spec(),
                    config,
                    cancel: task_cancel,
                    best_payload: Default::default(),
                    builder_config: Default::default(),
                };

                let state_provider = this
                    .state_provider_factory
                    .state_provider(Some(BlockId::Hash(parent_hash.into())))
                    .await
                    .map_err(PayloadBuilderError::other)?;

                let builder =
                    Builder::new(|_| NoopPayloadTransactions::<BasePooledTransaction>::default());

                builder.witness(state_provider, eth_api, &ctx).map_err(PayloadBuilderError::other)
            };

            let _ = tx.send(result.await);
        });

        rx.await.map_err(PayloadBuilderError::other)?
    }

    /// Spawns the background task that prebuilds witnesses into the witness cache.
    ///
    /// The task follows the proofs storage tip and builds block `N` once proofs storage holds
    /// block `N - 1` and `N` is at least `build_lag` blocks behind the tip, evicting entries
    /// older than the retention window. Builds pause while the proofs storage tip is not
    /// canonical, and failed builds are retried up to [`WITNESS_CACHE_MAX_BUILD_ATTEMPTS`] times
    /// while the block stays in the build window. Does nothing if the API has no witness cache.
    pub fn spawn_witness_cache_builder(&self, config: &WitnessCacheConfig)
    where
        Attrs: Attributes<RpcPayloadAttributes = BasePayloadAttributes>,
    {
        let Some(cache) = self.inner.witness_cache.clone() else { return };
        let this = self.clone();
        let config = config.clone();
        self.inner
            .task_spawner
            .spawn_task(async move { this.run_witness_cache_builder(cache, config).await });
    }

    async fn run_witness_cache_builder(self, cache: Arc<WitnessCache>, config: WitnessCacheConfig)
    where
        Attrs: Attributes<RpcPayloadAttributes = BasePayloadAttributes>,
    {
        let permits = Arc::new(Semaphore::new(config.builder_concurrency.get()));
        let last_built = Arc::new(AtomicU64::new(0));
        let (retry_tx, mut retry_rx) = mpsc::unbounded_channel();
        // Blocks waiting to be built, with the number of failed attempts so far.
        let mut pending = VecDeque::<(u64, u32)>::new();
        let mut next_block = cache.block_range().map(|range| range.end() + 1);
        let mut evicted_below = None;
        let mut interval = tokio::time::interval(WITNESS_CACHE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let (earliest, latest) = match self.canonical_proofs_range() {
                Ok(Some(range)) => range,
                Ok(None) => continue,
                Err(error) => {
                    warn!(error = %error, "failed to read proofs storage range for witness cache");
                    continue;
                }
            };

            let evict_below = latest.saturating_sub(config.retention_blocks);
            if evicted_below != Some(evict_below) {
                evicted_below = Some(evict_below);
                // File deletion is blocking I/O; keep it off the async runtime.
                let evict_cache = Arc::clone(&cache);
                if let Err(error) =
                    tokio::task::spawn_blocking(move || evict_cache.evict_before(evict_below)).await
                {
                    warn!(error = %error, "witness cache eviction failed");
                }
            }
            WitnessCacheMetrics::builder_lag_blocks()
                .set(latest.saturating_sub(last_built.load(Ordering::Relaxed)) as f64);

            let Some(target) = latest.checked_sub(config.build_lag) else { continue };
            // Block `N` is built on the state of block `N - 1`, which must be in proofs storage.
            // On a cold start, backfill the most recent blocks instead of only the tip.
            let window_start = target
                .saturating_sub(WITNESS_CACHE_MAX_PENDING_BUILDS as u64 - 1)
                .max(earliest + 1);
            while let Ok(retry) = retry_rx.try_recv() {
                pending.push_back(retry);
            }
            let start = next_block.unwrap_or(window_start).max(window_start);
            pending.extend((start..=target).map(|block_number| (block_number, 0)));
            next_block = Some(next_block.map_or(target + 1, |next| next.max(target + 1)));
            pending.retain(|(block_number, _)| *block_number >= window_start);
            while pending.len() > WITNESS_CACHE_MAX_PENDING_BUILDS {
                pending.pop_front();
            }

            while !pending.is_empty() {
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { break };
                let Some((block_number, failed_attempts)) = pending.pop_front() else { break };
                let this = self.clone();
                let cache = Arc::clone(&cache);
                let last_built = Arc::clone(&last_built);
                let retry_tx = retry_tx.clone();
                self.inner.task_spawner.spawn_task(async move {
                    let _permit = permit;
                    match this.build_cached_witness(&cache, block_number).await {
                        Ok(()) => {
                            last_built.fetch_max(block_number, Ordering::Relaxed);
                        }
                        Err(error) => {
                            WitnessCacheMetrics::build_failures().increment(1);
                            let attempts = failed_attempts + 1;
                            warn!(
                                error = %error,
                                block = block_number,
                                attempts,
                                "failed to prebuild witness"
                            );
                            if attempts < WITNESS_CACHE_MAX_BUILD_ATTEMPTS {
                                let _ = retry_tx.send((block_number, attempts));
                            }
                        }
                    }
                });
            }
        }
    }

    /// Returns the `(earliest, latest)` block range of proofs storage if its tip is canonical.
    ///
    /// Proofs storage is extended along parent links, so a canonical tip implies every stored
    /// block is canonical. A non-canonical tip means the proofs `ExEx` has not yet caught up with
    /// a reorg, and witnesses built from its state could be stale.
    fn canonical_proofs_range(&self) -> eyre::Result<Option<(u64, u64)>> {
        let (Some((earliest, _)), Some((latest, latest_hash))) = (
            self.inner.storage.get_earliest_block_number()?,
            self.inner.storage.get_latest_block_number()?,
        ) else {
            return Ok(None);
        };
        if self.inner.provider.block_hash(latest)? != Some(latest_hash) {
            debug!(block = latest, "proofs storage tip is not canonical");
            return Ok(None);
        }
        Ok(Some((earliest, latest)))
    }

    /// Returns whether proofs storage holds `block_number` and its tip is canonical, so its state
    /// at `block_number` belongs to the canonical chain rather than a chain being reorged out.
    fn proofs_snapshot_is_canonical(&self, block_number: u64) -> bool {
        self.canonical_proofs_range().is_ok_and(|range| {
            range.is_some_and(|(earliest, latest)| (earliest..=latest).contains(&block_number))
        })
    }

    /// Returns the hash, parent hash and rebuilding payload attributes of canonical block
    /// `block_number`.
    fn canonical_block_attributes(
        &self,
        block_number: u64,
    ) -> eyre::Result<(B256, B256, BasePayloadAttributes)> {
        let block = self
            .inner
            .provider
            .block_by_number(block_number)?
            .ok_or(ProviderError::HeaderNotFound(block_number.into()))?;
        let attributes =
            CanonicalPayloadAttributes::from_block(&block, &*self.inner.provider.chain_spec())?;
        Ok((block.header().hash_slow(), block.header().parent_hash(), attributes))
    }

    /// Builds the witness of canonical block `block_number` and stores it in `cache`.
    ///
    /// The witness is discarded with an error if the chain reorged during the build.
    async fn build_cached_witness(
        &self,
        cache: &Arc<WitnessCache>,
        block_number: u64,
    ) -> eyre::Result<()>
    where
        Attrs: Attributes<RpcPayloadAttributes = BasePayloadAttributes>,
    {
        let (block_hash, parent_hash, attributes) =
            self.canonical_block_attributes(block_number)?;
        let attributes_digest = WitnessCache::attributes_digest(&attributes);
        if cache.contains(block_number, parent_hash, attributes_digest) {
            return Ok(());
        }
        let attributes = Attrs::try_new(parent_hash, attributes, EXECUTE_PAYLOAD_VERSION)?;

        let parent_header = self.parent_header(parent_hash)?;
        let start = Instant::now();
        let witness = self.build_witness(parent_header, attributes).await?;
        WitnessCacheMetrics::build_duration().record(start.elapsed().as_secs_f64());

        if self.canonical_proofs_range()?.is_none()
            || self.inner.provider.block_hash(block_number)? != Some(block_hash)
        {
            eyre::bail!("canonical chain changed while building the witness");
        }

        let cache = Arc::clone(cache);
        tokio::task::spawn_blocking(move || {
            cache.insert(block_number, parent_hash, attributes_digest, &witness)
        })
        .await??;
        Ok(())
    }

    /// Returns whether `attributes_digest` on top of `parent_hash` rebuilds canonical block
    /// `block_number`.
    fn is_canonical_request(
        &self,
        block_number: u64,
        parent_hash: B256,
        attributes_digest: B256,
    ) -> bool {
        self.proofs_snapshot_is_canonical(block_number - 1)
            && self.canonical_block_attributes(block_number).is_ok_and(
                |(_, canonical_parent_hash, attributes)| {
                    canonical_parent_hash == parent_hash
                        && WitnessCache::attributes_digest(&attributes) == attributes_digest
                },
            )
    }
}

#[async_trait]
impl<Eth, P, Provider, EvmConfig, Attrs, N> DebugApiOverrideServer<Attrs::RpcPayloadAttributes>
    for DebugApiExt<Eth, P, Provider, EvmConfig, Attrs>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
    Attrs: Attributes<
            Transaction = TxTy<EvmConfig::Primitives>,
            RpcPayloadAttributes = BasePayloadAttributes,
        >,
    N: PayloadPrimitives<_TX = base_common_consensus::BaseTransactionSigned>,
    EvmConfig: ConfigureEvm<
            Primitives = N,
            NextBlockEnvCtx: BuildNextEnv<Attrs, N::BlockHeader, Provider::ChainSpec>,
        > + 'static,
    Provider: BlockReaderIdExt<Header = N::BlockHeader>
        + StateProviderFactory
        + ChainSpecProvider<ChainSpec: Upgrades>
        + NodePrimitivesProvider<Primitives = N>
        + HeaderProvider<Header = N::BlockHeader>
        + Clone
        + 'static,
{
    /// Serves the witness from the witness cache when present, see [`WitnessCache::get_or_build`].
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: Attrs::RpcPayloadAttributes,
    ) -> RpcResult<ExecutionWitness> {
        DebugApiExtMetrics::record_operation_async(DebugApis::DebugExecutePayload, async {
            let parent_header = self.parent_header(parent_block_hash).to_rpc_result()?;
            let parent_hash = parent_header.hash();
            let block_number = parent_header.number() + 1;
            let attributes_digest = self
                .inner
                .witness_cache
                .as_ref()
                .map(|_| WitnessCache::attributes_digest(&attributes));
            let attributes = Attrs::try_new(parent_hash, attributes, EXECUTE_PAYLOAD_VERSION)
                .map_err(|err| internal_rpc_err(PayloadBuilderError::other(err).to_string()))?;

            // Checked right before building and again before caching, so a witness built from
            // proofs state that a lagging reorg is still replacing is never cached.
            let canonical_before_build = AtomicBool::new(false);
            let build = async {
                let _permit = self.inner.semaphore.acquire().await;
                if self.inner.witness_cache.is_some() {
                    canonical_before_build.store(
                        self.proofs_snapshot_is_canonical(block_number - 1),
                        Ordering::Relaxed,
                    );
                }
                self.build_witness(parent_header, attributes)
                    .await
                    .map_err(|err| internal_rpc_err(err.to_string()))
            };
            match (&self.inner.witness_cache, attributes_digest) {
                (Some(cache), Some(attributes_digest)) => {
                    cache
                        .get_or_build(block_number, parent_hash, attributes_digest, build, || {
                            canonical_before_build.load(Ordering::Relaxed)
                                && self.is_canonical_request(
                                    block_number,
                                    parent_hash,
                                    attributes_digest,
                                )
                        })
                        .await
                }
                _ => build.await,
            }
        })
        .await
    }

    /// Not served from the witness cache: the payload builder additionally loads the
    /// `L2ToL1MessagePasser` account after Isthmus, so its witness can differ from the block
    /// executor witness returned here.
    async fn execution_witness(&self, block_id: BlockNumberOrTag) -> RpcResult<ExecutionWitness> {
        DebugApiExtMetrics::record_operation_async(DebugApis::DebugExecutionWitness, async {
            let _permit = self.inner.semaphore.acquire().await;

            let block = self
                .inner
                .eth_api
                .recovered_block(block_id.into())
                .await?
                .ok_or(EthApiError::HeaderNotFound(block_id.into()))?;

            let this = Arc::clone(&self.inner);
            let block_number = block.header().number();

            let state_provider = this
                .state_provider_factory
                .state_provider(Some(BlockId::Number(block.parent_num_hash().number.into())))
                .await
                .map_err(EthApiError::from)?;
            let db = StateProviderDatabase::new(&state_provider);
            let block_executor = this.eth_api.evm_config().executor(db);

            let mut witness = None;

            let mode = ExecutionWitnessMode::default();
            let _ = block_executor
                .execute_with_state_closure(&block, |statedb: &State<_>| {
                    witness = Some(ExecutionWitnessRecord::new(statedb).into_execution_witness(
                        &statedb.database.0,
                        self.inner.eth_api.provider(),
                        block_number,
                        mode,
                    ));
                })
                .map_err(EthApiError::from)?;

            let witness = witness.unwrap().map_err(EthApiError::from)?;

            Ok(witness)
        })
        .await
    }

    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus> {
        let earliest = self
            .inner
            .storage
            .get_earliest_block_number()
            .map_err(|err| internal_rpc_err(err.to_string()))?;
        let latest = self
            .inner
            .storage
            .get_latest_block_number()
            .map_err(|err| internal_rpc_err(err.to_string()))?;

        Ok(ProofsSyncStatus {
            earliest: earliest.map(|(block_number, _)| block_number),
            latest: latest.map(|(block_number, _)| block_number),
        })
    }
}
