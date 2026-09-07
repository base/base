//! Base `eth_` endpoint implementation.

pub mod proofs;
pub mod receipt;
pub mod transaction;

mod base_time;
pub use base_time::BaseTimeCache;
use reth_evm::BaseEvmConfig;

mod block;
mod call;
mod pending_block;
mod pubsub;

use std::{
    fmt::{self, Formatter},
    sync::Arc,
};

use alloy_primitives::U256;
use eyre::WrapErr;
pub use receipt::{BaseReceiptBuilder, ReceiptFieldsBuilder};
use reth_node_api::{FullNodeComponents, FullNodeTypes};
mod context;
pub use context::EthApiCtx;
use reth_rpc::eth::core::EthApiInner;
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, FullEthApiServer, RpcConvert, RpcConverter, RpcNodeCore,
    RpcNodeCoreExt,
    helpers::{
        EthApiSpec, EthFees, EthState, GetBlockAccessList, LoadFee, LoadPendingBlock, LoadState,
        SpawnBlocking, Trace,
    },
};
use reth_rpc_eth_types::{EthStateCache, FeeHistoryCache, GasPriceOracle};
use reth_storage_api::ProviderHeader;
use reth_tasks::{
    Runtime,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};

use crate::{
    BaseEthApiError, SequencerClient,
    eth::{receipt::BaseReceiptConverter, transaction::BaseTxInfoMapper},
};

/// Adapter for [`EthApiInner`], which holds all the data required to serve core `eth_` API.
pub type EthApiNodeBackend<N, Rpc> = EthApiInner<N, Rpc>;

/// Base `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
///
/// This wraps a default `Eth` implementation, and provides additional functionality where the
/// Base spec deviates from the default (ethereum) spec, e.g. transaction forwarding to the
/// sequencer, receipts, additional RPC fields for transaction receipts.
///
/// This type implements the [`FullEthApi`](reth_rpc_eth_api::helpers::FullEthApi) by implemented
/// all the `Eth` helper traits and prerequisite traits.
pub struct BaseEthApi<N: RpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    inner: Arc<BaseEthApiInner<N, Rpc>>,
}

impl<N: RpcNodeCore, Rpc: RpcConvert> Clone for BaseEthApi<N, Rpc> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<N: RpcNodeCore, Rpc: RpcConvert> BaseEthApi<N, Rpc> {
    /// Creates a new `BaseEthApi`.
    pub fn new(
        eth_api: EthApiNodeBackend<N, Rpc>,
        sequencer_client: Option<SequencerClient>,
        min_suggested_priority_fee: U256,
        base_time: BaseTimeCache,
    ) -> Self {
        let inner = Arc::new(BaseEthApiInner {
            eth_api,
            sequencer_client,
            min_suggested_priority_fee,
            base_time,
        });
        Self { inner }
    }

    /// Build a [`BaseEthApi`] using [`BaseEthApiBuilder`].
    pub const fn builder() -> BaseEthApiBuilder {
        BaseEthApiBuilder::new()
    }

    /// Returns a reference to the [`EthApiNodeBackend`].
    pub fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        self.inner.eth_api()
    }
    /// Returns the configured sequencer client, if any.
    pub fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.inner.sequencer_client()
    }

    /// Returns the shared cache of validated `BaseTime` timestamps.
    pub fn base_time_cache(&self) -> &BaseTimeCache {
        &self.inner.base_time
    }
}

impl<N, Rpc> EthApiTypes for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
    type Error = BaseEthApiError;

    type RpcConvert = Rpc;

    fn converter(&self) -> &Self::RpcConvert {
        self.inner.eth_api.converter()
    }
}

impl<N, Rpc> RpcNodeCore for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    type Provider = N::Provider;
    type Pool = N::Pool;

    type Network = N::Network;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.eth_api.pool()
    }

    #[inline]
    fn evm_config(&self) -> &BaseEvmConfig {
        self.inner.eth_api.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.eth_api.network()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.eth_api.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    #[inline]
    fn cache(&self) -> &EthStateCache {
        self.inner.eth_api.cache()
    }
}

impl<N, Rpc> EthApiSpec for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.eth_api.starting_block()
    }
}

impl<N, Rpc> SpawnBlocking for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
    #[inline]
    fn io_task_spawner(&self) -> &Runtime {
        self.inner.eth_api.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.eth_api.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.eth_api.blocking_task_guard()
    }

    #[inline]
    fn blocking_io_task_guard(&self) -> &Arc<tokio::sync::Semaphore> {
        self.inner.eth_api.blocking_io_request_semaphore()
    }
}

impl<N, Rpc> LoadFee for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.inner.eth_api.fee_history_cache()
    }

    async fn suggested_priority_fee(&self) -> Result<U256, Self::Error> {
        self.inner
            .eth_api
            .gas_oracle()
            .op_suggest_tip_cap(self.inner.min_suggested_priority_fee)
            .await
            .map_err(Into::into)
    }
}

impl<N, Rpc> LoadState for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
    Self: LoadPendingBlock,
{
}

impl<N, Rpc> EthState for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Error = BaseEthApiError>,
    Self: LoadPendingBlock,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_api.eth_proof_window()
    }
}

impl<N, Rpc> EthFees for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
}

impl<N, Rpc> Trace for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
}

impl<N, Rpc> GetBlockAccessList for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError,
    Rpc: RpcConvert<Error = BaseEthApiError>,
{
}

impl<N: RpcNodeCore, Rpc: RpcConvert> fmt::Debug for BaseEthApi<N, Rpc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseEthApi").finish_non_exhaustive()
    }
}

/// Container type `BaseEthApi`
pub struct BaseEthApiInner<N: RpcNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    eth_api: EthApiNodeBackend<N, Rpc>,
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
    sequencer_client: Option<SequencerClient>,
    /// Minimum priority fee enforced by rollup-specific logic.
    ///
    /// See also <https://github.com/ethereum-optimism/op-geth/blob/d4e0fe9bb0c2075a9bff269fb975464dd8498f75/eth/gasprice/optimism-gasprice.go#L38-L38>
    min_suggested_priority_fee: U256,
    /// Shared cache of validated `BaseTime` timestamps.
    base_time: BaseTimeCache,
}

impl<N: RpcNodeCore, Rpc: RpcConvert> fmt::Debug for BaseEthApiInner<N, Rpc> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseEthApiInner").finish()
    }
}

impl<N: RpcNodeCore, Rpc: RpcConvert> BaseEthApiInner<N, Rpc> {
    /// Returns a reference to the [`EthApiNodeBackend`].
    const fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        &self.eth_api
    }

    /// Returns the configured sequencer client, if any.
    const fn sequencer_client(&self) -> Option<&SequencerClient> {
        self.sequencer_client.as_ref()
    }
}

/// Converter for Base RPC types.
pub type BaseRpcConvert<N> = RpcConverter<
    BaseReceiptConverter<<N as FullNodeTypes>::Provider>,
    (),
    BaseTxInfoMapper<<N as FullNodeTypes>::Provider>,
>;

/// The Base eth API for a node provider, transaction pool, and network.
pub type BaseNodeEthApi<N> = BaseEthApi<N, BaseRpcConvert<N>>;

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
        BaseRpcConvert<N>: RpcConvert,
        BaseNodeEthApi<N>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
    {
        let Self { sequencer_url, sequencer_headers, min_suggested_priority_fee, .. } = self;
        let provider = ctx.components.provider().clone();
        let base_time = BaseTimeCache::default();
        let rpc_converter =
            RpcConverter::new(BaseReceiptConverter::new(provider.clone(), base_time.clone()))
                .with_mapper(BaseTxInfoMapper::new(provider, base_time.clone()));

        let sequencer_client = if let Some(url) = sequencer_url {
            Some(
                SequencerClient::new_with_headers(&url, sequencer_headers)
                    .await
                    .wrap_err_with(|| format!("Failed to init sequencer client with: {url}"))?,
            )
        } else {
            None
        };

        let eth_api = ctx.eth_api_builder().with_rpc_converter(rpc_converter).build_inner();

        Ok(BaseEthApi::new(
            eth_api,
            sequencer_client,
            U256::from(min_suggested_priority_fee),
            base_time,
        ))
    }
}
