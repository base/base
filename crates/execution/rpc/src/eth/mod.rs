//! Base `eth_` endpoint implementation.

mod proofs;

pub use proofs::*;
mod receipt;
pub use receipt::*;
mod transaction;
pub use transaction::*;

mod base_time;
use base_execution_evm::BaseEvmConfig;
pub use base_time::BaseTimeCache;

mod block;
mod call;
mod pending_block;
mod pubsub;

use std::{fmt, sync::Arc};

use alloy_primitives::U256;
use eyre::WrapErr;
pub use receipt::{BaseReceiptBuilder, ReceiptFieldsBuilder};
use reth_node_api::FullNodeComponents;
mod context;
pub use context::EthApiCtx;
use reth_rpc_eth_api::{
    BaseRpcConverter, EthApiTypes, FromEvmError, FullEthApiServer, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        EthApiSpec, EthFees, EthState, GetBlockAccessList, LoadFee, LoadPendingBlock, LoadState,
        SpawnBlocking, Trace,
    },
};
use reth_rpc_eth_types::{EthStateCache, FeeHistoryCache, GasPriceOracle};
use reth_tasks::{
    Runtime,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};

use crate::{BaseEthApiError, BaseEthApiInner, SequencerClient};

/// Base `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
///
/// Owns the shared backend services and Base transaction forwarding, fee policy, and timestamp
/// cache used by the RPC endpoints.
pub struct BaseEthApi<N: RpcNodeCore> {
    /// Gateway to node's core components.
    pub inner: Arc<BaseEthApiInner<N>>,
}

impl<N: RpcNodeCore> Clone for BaseEthApi<N> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<N: RpcNodeCore> BaseEthApi<N> {
    /// Creates a new `BaseEthApi`.
    pub fn new(
        mut eth_api: BaseEthApiInner<N>,
        sequencer_client: Option<SequencerClient>,
        min_suggested_priority_fee: U256,
        base_time: BaseTimeCache,
    ) -> Self {
        eth_api.sequencer_client = sequencer_client;
        eth_api.min_suggested_priority_fee = min_suggested_priority_fee;
        eth_api.base_time = base_time;
        let inner = Arc::new(eth_api);
        Self { inner }
    }

    /// Build a [`BaseEthApi`] using [`BaseEthApiBuilder`].
    pub const fn builder() -> BaseEthApiBuilder {
        BaseEthApiBuilder::new()
    }

    /// Returns the configured sequencer client, if any.
    pub fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.inner.sequencer_client.as_ref()
    }

    /// Returns the shared cache of validated `BaseTime` timestamps.
    pub fn base_time_cache(&self) -> &BaseTimeCache {
        &self.inner.base_time
    }
}

impl<N> EthApiTypes for BaseEthApi<N>
where
    N: RpcNodeCore,
{
    fn converter(&self) -> &BaseRpcConverter<Self::Provider> {
        self.inner.converter()
    }
}

impl<N> RpcNodeCore for BaseEthApi<N>
where
    N: RpcNodeCore,
{
    type Provider = N::Provider;
    type Pool = N::Pool;

    type Network = N::Network;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.pool()
    }

    #[inline]
    fn evm_config(&self) -> &BaseEvmConfig {
        self.inner.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.network()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.provider()
    }
}

impl<N> RpcNodeCoreExt for BaseEthApi<N>
where
    N: RpcNodeCore,
{
    #[inline]
    fn cache(&self) -> &EthStateCache {
        self.inner.cache()
    }
}

impl<N> EthApiSpec for BaseEthApi<N>
where
    N: RpcNodeCore,
{
    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.starting_block()
    }
}

impl<N> SpawnBlocking for BaseEthApi<N>
where
    N: RpcNodeCore,
{
    #[inline]
    fn io_task_spawner(&self) -> &Runtime {
        self.inner.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.blocking_task_guard()
    }

    #[inline]
    fn blocking_io_task_guard(&self) -> &Arc<tokio::sync::Semaphore> {
        self.inner.blocking_io_request_semaphore()
    }
}

impl<N> LoadFee for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache {
        self.inner.fee_history_cache()
    }

    async fn suggested_priority_fee(&self) -> Result<U256, BaseEthApiError> {
        self.inner
            .gas_oracle()
            .op_suggest_tip_cap(self.inner.min_suggested_priority_fee)
            .await
            .map_err(Into::into)
    }
}

impl<N> LoadState for BaseEthApi<N>
where
    N: RpcNodeCore,
    Self: LoadPendingBlock,
{
}

impl<N> EthState for BaseEthApi<N>
where
    N: RpcNodeCore,
    Self: LoadPendingBlock,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_proof_window()
    }
}

impl<N> EthFees for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
}

impl<N> Trace for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
}

impl<N> GetBlockAccessList for BaseEthApi<N>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
{
}

impl<N: RpcNodeCore> fmt::Debug for BaseEthApi<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseEthApi").finish_non_exhaustive()
    }
}

/// The Base eth API for a node provider, transaction pool, and network.
pub type BaseNodeEthApi<N> = BaseEthApi<N>;

/// Builds [`BaseEthApi`] for Base.
#[derive(Debug)]
pub struct BaseEthApiBuilder {
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Minimum suggested priority fee (tip)
    min_suggested_priority_fee: u64,
}

impl Default for BaseEthApiBuilder {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            min_suggested_priority_fee: 1_000_000,
        }
    }
}

impl BaseEthApiBuilder {
    /// Creates a [`BaseEthApiBuilder`] instance from core components.
    pub const fn new() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            min_suggested_priority_fee: 1_000_000,
        }
    }

    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_url: Option<String>) -> Self {
        self.sequencer_url = sequencer_url;
        self
    }

    /// With headers to use for the sequencer client requests.
    pub fn with_sequencer_headers(mut self, sequencer_headers: Vec<String>) -> Self {
        self.sequencer_headers = sequencer_headers;
        self
    }

    /// With minimum suggested priority fee (tip).
    pub const fn with_min_suggested_priority_fee(mut self, min: u64) -> Self {
        self.min_suggested_priority_fee = min;
        self
    }
}

impl BaseEthApiBuilder {
    /// Constructs the Base eth API from the node components and RPC settings.
    pub async fn build_eth_api<N>(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<BaseNodeEthApi<N>>
    where
        N: FullNodeComponents,
        BaseNodeEthApi<N>: FullEthApiServer<
                Provider = N::Provider,
                Pool = reth_node_api::BaseNodePool<N::Provider>,
            >,
    {
        let Self { sequencer_url, sequencer_headers, min_suggested_priority_fee, .. } = self;
        let base_time = BaseTimeCache::default();

        let sequencer_client = if let Some(url) = sequencer_url {
            Some(
                SequencerClient::new_with_headers(&url, sequencer_headers)
                    .await
                    .wrap_err_with(|| format!("Failed to init sequencer client with: {url}"))?,
            )
        } else {
            None
        };

        let eth_api = ctx.eth_api_builder().base_time_cache(base_time.clone()).build_inner();

        Ok(BaseEthApi::new(
            eth_api,
            sequencer_client,
            U256::from(min_suggested_priority_fee),
            base_time,
        ))
    }
}
