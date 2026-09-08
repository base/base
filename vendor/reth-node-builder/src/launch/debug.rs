use std::{
    future::{Future, IntoFuture},
    pin::Pin,
    sync::Arc,
};

use alloy_consensus::transaction::Either;
use alloy_provider::network::AnyNetwork;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_types::{BaseBuiltPayload, BasePayloadBuilderAttributes};
use jsonrpsee::core::{DeserializeOwned, Serialize};
use reth_consensus_debug_client::{
    DebugConsensusClient, EtherscanBlockProvider, PayloadProvider, RpcBlockProvider,
};
use reth_engine_local::{LocalMiner, MiningMode};
use reth_node_api::{FullNodeComponents, PayloadAttributesBuilder};
use reth_primitives_traits::SealedBlock;
use tracing::info;

use super::LaunchNode;
use crate::{NodeHandle, rpc::RethRpcAddOns};

/// Concrete conversions used by the debug launcher.
#[derive(Debug)]
pub struct DebugNodeConfig<R> {
    /// Converts an RPC response to the node's primitive block.
    pub rpc_to_primitive_block: fn(R) -> BaseBlock,
    /// Creates the default local-mining payload attributes builder.
    pub local_payload_attributes_builder: fn(
        &BaseChainSpec,
    ) -> Box<
        dyn PayloadAttributesBuilder<
                BasePayloadBuilderAttributes<BaseTxEnvelope>,
                alloy_consensus::Header,
            >,
    >,
}

impl<R> Copy for DebugNodeConfig<R> {}

impl<R> Clone for DebugNodeConfig<R> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Node launcher with support for launching various debugging utilities.
///
/// This launcher wraps an existing launcher and adds debugging capabilities when
/// certain debug flags are enabled. It provides two main debugging features:
///
/// ## RPC Consensus Client
///
/// When `--debug.rpc-consensus-ws <URL>` is provided, the launcher will:
/// - Connect to an external RPC endpoint (`WebSocket` or HTTP)
/// - Fetch blocks from that endpoint (using subscriptions for `WebSocket`, polling for HTTP)
/// - Submit them to the local engine for execution
/// - Useful for testing engine behavior with real network data
///
/// ## Etherscan Consensus Client
///
/// When `--debug.etherscan [URL]` is provided, the launcher will:
/// - Use Etherscan API as a consensus client
/// - Fetch recent blocks from Etherscan
/// - Submit them to the local engine
/// - Requires `ETHERSCAN_API_KEY` environment variable
/// - Falls back to default Etherscan URL for the chain if URL not provided
#[derive(Debug, Clone)]
pub struct DebugNodeLauncher<L, R> {
    inner: L,
    config: DebugNodeConfig<R>,
}

impl<L, R> DebugNodeLauncher<L, R> {
    /// Creates a new instance of the [`DebugNodeLauncher`].
    pub const fn new(inner: L, config: DebugNodeConfig<R>) -> Self {
        Self { inner, config }
    }
}

/// Type alias for the default debug block provider. We use etherscan provider to satisfy the
/// bounds.
pub type DefaultDebugBlockProvider<R> =
    EtherscanBlockProvider<R, base_common_rpc_types_engine::ExecutionData>;

/// Future for the [`DebugNodeLauncher`].
#[expect(missing_debug_implementations, clippy::type_complexity)]
pub struct DebugNodeLauncherFuture<L, Target, N, R, B = DefaultDebugBlockProvider<R>>
where
    N: FullNodeComponents,
{
    inner: L,
    target: Target,
    config: DebugNodeConfig<R>,
    local_payload_attributes_builder: Option<
        Box<
            dyn PayloadAttributesBuilder<
                    BasePayloadBuilderAttributes<BaseTxEnvelope>,
                    alloy_consensus::Header,
                >,
        >,
    >,
    map_attributes: Option<
        Box<
            dyn Fn(
                    BasePayloadBuilderAttributes<BaseTxEnvelope>,
                ) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
                + Send
                + Sync,
        >,
    >,
    debug_block_provider: Option<B>,
    mining_mode: Option<MiningMode<reth_node_api::BaseNodePool<N::Provider>>>,
}

impl<L, Target, N, AddOns, R, B> DebugNodeLauncherFuture<L, Target, N, R, B>
where
    N: FullNodeComponents,
    R: Serialize + DeserializeOwned + 'static,
    AddOns: RethRpcAddOns<N>,
    L: LaunchNode<Target, Node = NodeHandle<N, AddOns>>,
    B: PayloadProvider<ExecutionData = base_common_rpc_types_engine::ExecutionData> + Clone,
{
    /// Sets a custom payload attributes builder for local mining in dev mode.
    pub fn with_payload_attributes_builder(
        self,
        builder: impl PayloadAttributesBuilder<
            BasePayloadBuilderAttributes<BaseTxEnvelope>,
            alloy_consensus::Header,
        >,
    ) -> Self {
        Self {
            inner: self.inner,
            target: self.target,
            config: self.config,
            local_payload_attributes_builder: Some(Box::new(builder)),
            map_attributes: None,
            debug_block_provider: self.debug_block_provider,
            mining_mode: self.mining_mode,
        }
    }

    /// Sets a function to map payload attributes before building.
    pub fn map_debug_payload_attributes(
        self,
        f: impl Fn(
            BasePayloadBuilderAttributes<BaseTxEnvelope>,
        ) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            inner: self.inner,
            target: self.target,
            config: self.config,
            local_payload_attributes_builder: None,
            map_attributes: Some(Box::new(f)),
            debug_block_provider: self.debug_block_provider,
            mining_mode: self.mining_mode,
        }
    }

    /// Sets a custom [`MiningMode`] for the local miner in dev mode.
    ///
    /// This overrides the default mining mode that is derived from the node configuration
    /// (instant or interval). This can be used to provide a custom trigger-based mining mode.
    pub fn with_mining_mode(
        mut self,
        mode: MiningMode<reth_node_api::BaseNodePool<N::Provider>>,
    ) -> Self {
        self.mining_mode = Some(mode);
        self
    }

    /// Sets a custom payload provider for the debug consensus client.
    ///
    /// When set, this provider will be used instead of creating an `EtherscanBlockProvider`
    /// or `RpcBlockProvider` from CLI arguments.
    pub fn with_debug_block_provider<B2>(
        self,
        provider: B2,
    ) -> DebugNodeLauncherFuture<L, Target, N, R, B2>
    where
        B2: PayloadProvider<ExecutionData = base_common_rpc_types_engine::ExecutionData> + Clone,
    {
        DebugNodeLauncherFuture {
            inner: self.inner,
            target: self.target,
            config: self.config,
            local_payload_attributes_builder: self.local_payload_attributes_builder,
            map_attributes: self.map_attributes,
            debug_block_provider: Some(provider),
            mining_mode: self.mining_mode,
        }
    }

    async fn launch_node(self) -> eyre::Result<NodeHandle<N, AddOns>> {
        let Self {
            inner,
            target,
            config: debug_config,
            local_payload_attributes_builder,
            map_attributes,
            debug_block_provider,
            mining_mode,
        } = self;

        let rpc_to_primitive_block = debug_config.rpc_to_primitive_block;
        let handle = inner.launch_node(target).await?;

        let config = &handle.node.config;

        if let Some(provider) = debug_block_provider {
            info!(target: "reth::cli", "Using custom debug block provider");

            let rpc_consensus_client = DebugConsensusClient::new(
                handle.node.add_ons_handle.beacon_engine_handle.clone(),
                Arc::new(provider),
            );

            handle
                .node
                .task_executor
                .spawn_critical_task("custom debug block provider consensus client", async move {
                    rpc_consensus_client.run().await
                });
        } else if let Some(url) = config.debug.rpc_consensus_url.clone() {
            info!(target: "reth::cli", "Using RPC consensus client: {}", url);

            let block_provider = RpcBlockProvider::<AnyNetwork, _>::new(
                url.as_str(),
                move |block_response, extras| {
                    let json = serde_json::to_value(block_response)
                        .expect("Block serialization cannot fail");
                    let rpc_block =
                        serde_json::from_value(json).expect("Block deserialization cannot fail");
                    let primitive_block = rpc_to_primitive_block(rpc_block);
                    BaseBuiltPayload::block_to_payload(
                        SealedBlock::new_unhashed(primitive_block),
                        extras.bal,
                    )
                },
            )
            .await?;

            let rpc_consensus_client = DebugConsensusClient::new(
                handle.node.add_ons_handle.beacon_engine_handle.clone(),
                Arc::new(block_provider),
            );

            handle.node.task_executor.spawn_critical_task("rpc-ws consensus client", async move {
                rpc_consensus_client.run().await
            });
        } else if let Some(maybe_custom_etherscan_url) = config.debug.etherscan.clone() {
            info!(target: "reth::cli", "Using etherscan as consensus client");

            let chain = config.chain.chain();
            let etherscan_url = maybe_custom_etherscan_url.map(Ok).unwrap_or_else(|| {
                chain
                    .etherscan_urls()
                    .map(|urls| urls.0.to_string())
                    .ok_or_else(|| eyre::eyre!("failed to get etherscan url for chain: {chain}"))
            })?;

            let block_provider = EtherscanBlockProvider::new(
                etherscan_url,
                chain.etherscan_api_key().ok_or_else(|| {
                    eyre::eyre!(
                        "etherscan api key not found for rpc consensus client for chain: {chain}"
                    )
                })?,
                chain.id(),
                move |rpc_block| {
                    let primitive_block = rpc_to_primitive_block(rpc_block);
                    BaseBuiltPayload::block_to_payload(
                        SealedBlock::new_unhashed(primitive_block),
                        None,
                    )
                },
            );
            let rpc_consensus_client = DebugConsensusClient::new(
                handle.node.add_ons_handle.beacon_engine_handle.clone(),
                Arc::new(block_provider),
            );
            handle
                .node
                .task_executor
                .spawn_critical_task("etherscan consensus client", async move {
                    rpc_consensus_client.run().await
                });
        }

        if config.dev.dev {
            info!(target: "reth::cli", "Using local payload attributes builder for dev mode");

            let blockchain_db = handle.node.provider.clone();
            let chain_spec = config.chain.clone();
            let beacon_engine_handle = handle.node.add_ons_handle.beacon_engine_handle.clone();
            let pool = handle.node.pool.clone();
            let payload_builder_handle = handle.node.payload_builder_handle.clone();

            let builder = if let Some(builder) = local_payload_attributes_builder {
                Either::Left(builder)
            } else {
                let local = (debug_config.local_payload_attributes_builder)(&chain_spec);
                let builder = if let Some(f) = map_attributes {
                    Either::Left(move |parent| f(local.build(&parent)))
                } else {
                    Either::Right(local)
                };
                Either::Right(builder)
            };

            let dev_mining_mode =
                mining_mode.unwrap_or_else(|| handle.node.config.dev_mining_mode(pool));
            let finality_depth = config.dev.finality_depth;
            let payload_wait_time = config.dev.payload_wait_time;
            if let (Some(wait_time), Some(block_time)) = (payload_wait_time, config.dev.block_time)
            {
                eyre::ensure!(
                    wait_time <= block_time,
                    "--dev.payload-wait-time ({wait_time:?}) must be <= --dev.block-time ({block_time:?})"
                );
            }
            handle.node.task_executor.spawn_critical_task("local engine", async move {
                LocalMiner::new(
                    blockchain_db,
                    builder,
                    beacon_engine_handle,
                    dev_mining_mode,
                    payload_builder_handle,
                )
                .with_finality_depth(finality_depth)
                .with_payload_wait_time_opt(payload_wait_time)
                .run()
                .await
            });
        }

        Ok(handle)
    }
}

impl<L, Target, N, AddOns, R, B> IntoFuture for DebugNodeLauncherFuture<L, Target, N, R, B>
where
    Target: Send + 'static,
    N: FullNodeComponents,
    R: Serialize + DeserializeOwned + 'static,
    AddOns: RethRpcAddOns<N> + 'static,
    L: LaunchNode<Target, Node = NodeHandle<N, AddOns>> + 'static,
    B: PayloadProvider<ExecutionData = base_common_rpc_types_engine::ExecutionData>
        + Clone
        + 'static,
{
    type Output = eyre::Result<NodeHandle<N, AddOns>>;
    type IntoFuture = Pin<Box<dyn Future<Output = eyre::Result<NodeHandle<N, AddOns>>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.launch_node())
    }
}

impl<L, Target, N, AddOns, R> LaunchNode<Target> for DebugNodeLauncher<L, R>
where
    Target: Send + 'static,
    N: FullNodeComponents,
    R: Serialize + DeserializeOwned + 'static,
    AddOns: RethRpcAddOns<N> + 'static,
    L: LaunchNode<Target, Node = NodeHandle<N, AddOns>> + 'static,
    DefaultDebugBlockProvider<R>:
        PayloadProvider<ExecutionData = base_common_rpc_types_engine::ExecutionData> + Clone,
{
    type Node = NodeHandle<N, AddOns>;
    type Future = DebugNodeLauncherFuture<L, Target, N, R>;

    fn launch_node(self, target: Target) -> Self::Future {
        DebugNodeLauncherFuture {
            inner: self.inner,
            target,
            config: self.config,
            local_payload_attributes_builder: None,
            map_attributes: None,
            debug_block_provider: None,
            mining_mode: None,
        }
    }
}
