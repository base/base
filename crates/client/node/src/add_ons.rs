use std::marker::PhantomData;

use base_client_engine::BaseEngineValidatorBuilder;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_node_api::{BuildNextEnv, FullNodeComponents, HeaderTy, NodeAddOns, PayloadTypes, TxTy};
use reth_node_builder::{
    node::NodeTypes,
    rpc::{
        EngineApiBuilder, EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, Identity,
        PayloadValidatorBuilder, RethRpcAddOns, RethRpcMiddleware, RethRpcServerHandles, RpcAddOns,
        RpcContext, RpcHandle,
    },
};
use reth_optimism_forks::{OpHardfork, OpHardforks};
use reth_optimism_node::{OpEngineApiBuilder, OpEngineValidatorBuilder, OpNodeTypes};
use reth_optimism_payload_builder::{
    OpAttributes, OpPayloadPrimitives,
    config::{OpDAConfig, OpGasLimitConfig},
};
use reth_optimism_rpc::{
    SequencerClient,
    eth::{OpEthApiBuilder, ext::OpEthExtApi},
    historical::{HistoricalRpc, HistoricalRpcClient},
    miner::{MinerApiExtServer, OpMinerExtApi},
    witness::OpDebugWitnessApi,
};
use reth_optimism_txpool::OpPooledTx;
use reth_rpc_api::{DebugApiServer, DebugExecutionWitnessApiServer, L2EthApiExtServer};
use reth_rpc_server_types::RethRpcModule;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::TransactionPool;
use serde::de::DeserializeOwned;
use url::Url;

/// Add-ons w.r.t. optimism.
///
/// This type provides optimism-specific addons to the node and exposes the RPC server and engine
/// API.
#[derive(Debug)]
pub struct BaseAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = OpEngineApiBuilder<PVB>,
    EVB = BaseEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
    /// Data availability configuration for the OP builder.
    pub da_config: OpDAConfig,
    /// Gas limit configuration for the OP builder.
    pub gas_limit_config: OpGasLimitConfig,
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    pub sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    pub sequencer_headers: Vec<String>,
    /// RPC endpoint for historical data.
    ///
    /// This can be used to forward pre-bedrock rpc requests (op-mainnet).
    pub historical_rpc: Option<String>,
    /// Enable transaction conditionals.
    enable_tx_conditional: bool,
    min_suggested_priority_fee: u64,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Creates a new instance from components.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        rpc_add_ons: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
        da_config: OpDAConfig,
        gas_limit_config: OpGasLimitConfig,
        sequencer_url: Option<String>,
        sequencer_headers: Vec<String>,
        historical_rpc: Option<String>,
        enable_tx_conditional: bool,
        min_suggested_priority_fee: u64,
    ) -> Self {
        Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
        }
    }
}

impl<N> Default for BaseAddOns<N, OpEthApiBuilder, OpEngineValidatorBuilder>
where
    N: FullNodeComponents<Types: OpNodeTypes>,
    OpEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::builder().build()
    }
}

impl<N, NetworkT, RpcMiddleware>
    BaseAddOns<
        N,
        OpEthApiBuilder<NetworkT>,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        RpcMiddleware,
    >
where
    N: FullNodeComponents<Types: OpNodeTypes>,
    OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
{
    /// Build a [`OpAddOns`] using [`OpAddOnsBuilder`].
    pub fn builder() -> BaseAddOnsBuilder<NetworkT> {
        BaseAddOnsBuilder::default()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Maps the [`reth_node_builder::rpc::EngineApiBuilder`] builder type.
    pub fn with_engine_api<T>(
        self,
        engine_api_builder: T,
    ) -> BaseAddOns<N, EthB, PVB, T, EVB, RpcMiddleware> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_engine_api(engine_api_builder),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
        )
    }

    /// Maps the [`PayloadValidatorBuilder`] builder type.
    pub fn with_payload_validator<T>(
        self,
        payload_validator_builder: T,
    ) -> BaseAddOns<N, EthB, T, EB, EVB, RpcMiddleware> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            enable_tx_conditional,
            min_suggested_priority_fee,
            historical_rpc,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_payload_validator(payload_validator_builder),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
        )
    }

    /// Sets the RPC middleware stack for processing RPC requests.
    ///
    /// This method configures a custom middleware stack that will be applied to all RPC requests
    /// across HTTP, `WebSocket`, and IPC transports. The middleware is applied to the RPC service
    /// layer, allowing you to intercept, modify, or enhance RPC request processing.
    ///
    /// See also [`RpcAddOns::with_rpc_middleware`].
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOns<N, EthB, PVB, EB, EVB, T> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            enable_tx_conditional,
            min_suggested_priority_fee,
            historical_rpc,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_rpc_middleware(rpc_middleware),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
        )
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, N, EthB::EthApi>, RethRpcServerHandles) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, N, EthB::EthApi>) -> eyre::Result<()> + Send + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.extend_rpc_modules(hook);
        self
    }
}

impl<N, EthB, PVB, EB, EVB, Attrs, RpcMiddleware> NodeAddOns<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                ChainSpec: OpHardforks,
                Primitives: OpPayloadPrimitives,
                Payload: PayloadTypes<PayloadBuilderAttributes = Attrs>,
            >,
            Evm: ConfigureEvm<
                NextBlockEnvCtx: BuildNextEnv<
                    Attrs,
                    HeaderTy<N::Types>,
                    <N::Types as NodeTypes>::ChainSpec,
                >,
            >,
            Pool: TransactionPool<Transaction: OpPooledTx>,
        >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
    Attrs: OpAttributes<Transaction = TxTy<N::Types>, RpcPayloadAttributes: DeserializeOwned>,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            enable_tx_conditional,
            historical_rpc,
            ..
        } = self;

        let maybe_pre_bedrock_historical_rpc = historical_rpc
            .and_then(|historical_rpc| {
                ctx.node
                    .provider()
                    .chain_spec()
                    .op_fork_activation(OpHardfork::Bedrock)
                    .block_number()
                    .filter(|activation| *activation > 0)
                    .map(|bedrock_block| (historical_rpc, bedrock_block))
            })
            .map(|(historical_rpc, bedrock_block)| -> eyre::Result<_> {
                info!(target: "reth::cli", %bedrock_block, ?historical_rpc, "Using historical RPC endpoint pre bedrock");
                let provider = ctx.node.provider().clone();
                let client = HistoricalRpcClient::new(&historical_rpc)?;
                let layer = HistoricalRpc::new(provider, client, bedrock_block);
                Ok(layer)
            })
            .transpose()?
            ;

        let rpc_add_ons = rpc_add_ons.option_layer_rpc_middleware(maybe_pre_bedrock_historical_rpc);

        let builder = reth_optimism_payload_builder::OpPayloadBuilder::new(
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
        );
        // install additional OP specific rpc methods
        let debug_ext = OpDebugWitnessApi::<_, _, _, Attrs>::new(
            ctx.node.provider().clone(),
            Box::new(ctx.node.task_executor().clone()),
            builder,
        );
        let miner_ext = OpMinerExtApi::new(da_config, gas_limit_config);

        let sequencer_client = if let Some(url) = sequencer_url {
            Some(SequencerClient::new_with_headers(url, sequencer_headers).await?)
        } else {
            None
        };

        let tx_conditional_ext: OpEthExtApi<N::Pool, N::Provider> = OpEthExtApi::new(
            sequencer_client,
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
        );

        rpc_add_ons
            .launch_add_ons_with(ctx, move |container| {
                let reth_node_builder::rpc::RpcModuleContainer { modules, auth_module, registry } =
                    container;

                debug!(target: "reth::cli", "Installing debug payload witness rpc endpoint");
                modules.merge_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;

                // extend the miner namespace if configured in the regular http server
                modules.add_or_replace_if_module_configured(
                    RethRpcModule::Miner,
                    miner_ext.clone().into_rpc(),
                )?;

                // install the miner extension in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Miner) {
                    debug!(target: "reth::cli", "Installing miner DA rpc endpoint");
                    auth_module.merge_auth_methods(miner_ext.into_rpc())?;
                }

                // install the debug namespace in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Debug) {
                    debug!(target: "reth::cli", "Installing debug rpc endpoint");
                    auth_module.merge_auth_methods(registry.debug_api().into_rpc())?;
                }

                if enable_tx_conditional {
                    // extend the eth namespace if configured in the regular http server
                    modules.merge_if_module_configured(
                        RethRpcModule::Eth,
                        tx_conditional_ext.into_rpc(),
                    )?;
                }

                Ok(())
            })
            .await
    }
}

impl<N, EthB, PVB, EB, EVB, Attrs, RpcMiddleware> RethRpcAddOns<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                ChainSpec: OpHardforks,
                Primitives: OpPayloadPrimitives,
                Payload: PayloadTypes<PayloadBuilderAttributes = Attrs>,
            >,
            Evm: ConfigureEvm<
                NextBlockEnvCtx: BuildNextEnv<
                    Attrs,
                    HeaderTy<N::Types>,
                    <N::Types as NodeTypes>::ChainSpec,
                >,
            >,
        >,
    <<N as FullNodeComponents>::Pool as TransactionPool>::Transaction: OpPooledTx,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
    Attrs: OpAttributes<Transaction = TxTy<N::Types>, RpcPayloadAttributes: DeserializeOwned>,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.rpc_add_ons.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> EngineValidatorAddOn<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: Send,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        EngineValidatorAddOn::engine_validator_builder(&self.rpc_add_ons)
    }
}

/// A regular optimism evm and executor builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BaseAddOnsBuilder<NetworkT, RpcMiddleware = Identity> {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// RPC endpoint for historical data.
    historical_rpc: Option<String>,
    /// Data availability configuration for the OP builder.
    da_config: Option<OpDAConfig>,
    /// Gas limit configuration for the OP builder.
    gas_limit_config: Option<OpGasLimitConfig>,
    /// Enable transaction conditionals.
    enable_tx_conditional: bool,
    /// Marker for network types.
    _nt: PhantomData<NetworkT>,
    /// Minimum suggested priority fee (tip)
    min_suggested_priority_fee: u64,
    /// RPC middleware to use
    rpc_middleware: RpcMiddleware,
    /// Optional tokio runtime to use for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
    /// A URL pointing to a secure websocket service that streams out flashblocks.
    flashblocks_url: Option<Url>,
}

impl<NetworkT> Default for BaseAddOnsBuilder<NetworkT> {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            historical_rpc: None,
            da_config: None,
            gas_limit_config: None,
            enable_tx_conditional: false,
            min_suggested_priority_fee: 1_000_000,
            _nt: PhantomData,
            rpc_middleware: Identity::new(),
            tokio_runtime: None,
            flashblocks_url: None,
        }
    }
}

impl<NetworkT, RpcMiddleware> BaseAddOnsBuilder<NetworkT, RpcMiddleware> {
    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<String>) -> Self {
        self.sequencer_url = sequencer_client;
        self
    }

    /// With headers to use for the sequencer client requests.
    pub fn with_sequencer_headers(mut self, sequencer_headers: Vec<String>) -> Self {
        self.sequencer_headers = sequencer_headers;
        self
    }

    /// Configure the data availability configuration for the OP builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Configure the gas limit configuration for the OP payload builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: OpGasLimitConfig) -> Self {
        self.gas_limit_config = Some(gas_limit_config);
        self
    }

    /// Configure if transaction conditional should be enabled.
    pub const fn with_enable_tx_conditional(mut self, enable_tx_conditional: bool) -> Self {
        self.enable_tx_conditional = enable_tx_conditional;
        self
    }

    /// Configure the minimum priority fee (tip)
    pub const fn with_min_suggested_priority_fee(mut self, min: u64) -> Self {
        self.min_suggested_priority_fee = min;
        self
    }

    /// Configures the endpoint for historical RPC forwarding.
    pub fn with_historical_rpc(mut self, historical_rpc: Option<String>) -> Self {
        self.historical_rpc = historical_rpc;
        self
    }

    /// Configures a custom tokio runtime for the RPC server.
    ///
    /// Caution: This runtime must not be created from within asynchronous context.
    pub fn with_tokio_runtime(mut self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        self.tokio_runtime = tokio_runtime;
        self
    }

    /// Configure the RPC middleware to use
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOnsBuilder<NetworkT, T> {
        let Self {
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            da_config,
            gas_limit_config,
            enable_tx_conditional,
            min_suggested_priority_fee,
            tokio_runtime,
            _nt,
            flashblocks_url,
            ..
        } = self;
        BaseAddOnsBuilder {
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            da_config,
            gas_limit_config,
            enable_tx_conditional,
            min_suggested_priority_fee,
            _nt,
            rpc_middleware,
            tokio_runtime,
            flashblocks_url,
        }
    }

    /// With a URL pointing to a flashblocks secure websocket subscription.
    pub fn with_flashblocks(mut self, flashblocks_url: Option<Url>) -> Self {
        self.flashblocks_url = flashblocks_url;
        self
    }
}

impl<NetworkT, RpcMiddleware> BaseAddOnsBuilder<NetworkT, RpcMiddleware> {
    /// Builds an instance of [`OpAddOns`].
    pub fn build<N, PVB, EB, EVB>(
        self,
    ) -> BaseAddOns<N, OpEthApiBuilder<NetworkT>, PVB, EB, EVB, RpcMiddleware>
    where
        N: FullNodeComponents<Types: NodeTypes>,
        OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
        PVB: PayloadValidatorBuilder<N> + Default,
        EB: Default,
        EVB: Default,
    {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            enable_tx_conditional,
            min_suggested_priority_fee,
            historical_rpc,
            rpc_middleware,
            tokio_runtime,
            flashblocks_url,
            ..
        } = self;

        BaseAddOns::new(
            RpcAddOns::new(
                OpEthApiBuilder::default()
                    .with_sequencer(sequencer_url.clone())
                    .with_sequencer_headers(sequencer_headers.clone())
                    .with_min_suggested_priority_fee(min_suggested_priority_fee)
                    .with_flashblocks(flashblocks_url),
                PVB::default(),
                EB::default(),
                EVB::default(),
                rpc_middleware,
            )
            .with_tokio_runtime(tokio_runtime),
            da_config.unwrap_or_default(),
            gas_limit_config.unwrap_or_default(),
            sequencer_url,
            sequencer_headers,
            historical_rpc,
            enable_tx_conditional,
            min_suggested_priority_fee,
        )
    }
}
