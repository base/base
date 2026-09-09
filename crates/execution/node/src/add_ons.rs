use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};
use base_execution_rpc::{
    BaseDebugWitnessApi, BaseEthApiBuilder, BaseEthConfigApiServer, BaseEthConfigHandler,
    BaseMinerExtApi, BaseNodeEthApi, DebugExecutionWitnessApiServer, MinerApiExtServer,
};
use base_execution_txpool::{BasePooledTx, TransactionPool};
use base_node_context::NodeAddOns;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_server_types::RethRpcModule;
use reth_tracing::tracing::debug;

use crate::{
    Identity, RethRpcAddOns, RethRpcMiddleware, RethRpcServerHandles, RpcAddOns, RpcContext,
    RpcHandle,
};

/// Add-ons w.r.t. Base.
///
/// This type provides Base-specific addons to the node and exposes the RPC server and engine
/// API.
#[derive(Debug)]
pub struct BaseAddOns<RpcMiddleware = Identity> {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns<RpcMiddleware>,
    /// Data availability configuration for the payload builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the payload builder.
    pub gas_limit_config: GasLimitConfig,
}

impl<RpcMiddleware> BaseAddOns<RpcMiddleware> {
    /// Creates a new instance from components.
    pub const fn new(
        rpc_add_ons: RpcAddOns<RpcMiddleware>,
        da_config: BaseDAConfig,
        gas_limit_config: GasLimitConfig,
    ) -> Self {
        Self { rpc_add_ons, da_config, gas_limit_config }
    }
}

impl Default for BaseAddOns {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl BaseAddOns {
    /// Build a [`BaseAddOns`] using [`BaseAddOnsBuilder`].
    pub fn builder() -> BaseAddOnsBuilder {
        BaseAddOnsBuilder::default()
    }
}

impl<RpcMiddleware> BaseAddOns<RpcMiddleware> {
    /// Sets the RPC middleware stack for processing RPC requests.
    ///
    /// This method configures a custom middleware stack that will be applied to all RPC requests
    /// across HTTP, `WebSocket`, and IPC transports. The middleware is applied to the RPC service
    /// layer, allowing you to intercept, modify, or enhance RPC request processing.
    ///
    /// See also [`RpcAddOns::with_rpc_middleware`].
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOns<T> {
        let Self { rpc_add_ons, da_config, gas_limit_config, .. } = self;
        BaseAddOns::new(
            rpc_add_ons.with_rpc_middleware(rpc_middleware),
            da_config,
            gas_limit_config,
        )
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, BaseNodeEthApi<base_node_context::BaseNodeContext>>,
                RethRpcServerHandles,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, BaseNodeEthApi<base_node_context::BaseNodeContext>>,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.extend_rpc_modules(hook);
        self
    }
}

impl<RpcMiddleware> NodeAddOns for BaseAddOns<RpcMiddleware>
where
    RpcMiddleware: RethRpcMiddleware,
{
    type Handle = RpcHandle<BaseNodeEthApi<base_node_context::BaseNodeContext>>;

    async fn launch_add_ons(
        self,
        ctx: base_node_context::AddOnsContext<'_>,
    ) -> eyre::Result<Self::Handle> {
        let Self { rpc_add_ons, da_config, gas_limit_config, .. } = self;
        let eth_config =
            BaseEthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        let builder = base_execution_payload_builder::BasePayloadBuilder::new(
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
        );
        // Install additional rollup-specific RPC methods.
        let debug_ext = BaseDebugWitnessApi::<_, _>::new(
            ctx.node.provider().clone(),
            ctx.node.task_executor().clone(),
            builder,
        );
        let miner_ext = BaseMinerExtApi::new(da_config, gas_limit_config);

        rpc_add_ons
            .launch_add_ons_with(ctx, move |container| {
                let crate::RpcModuleContainer { modules, .. } = container;

                modules.merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                debug!(target: "reth::cli", "Installing debug payload witness rpc endpoint");
                modules.merge_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;

                // extend the miner namespace if configured in the regular http server
                modules.add_or_replace_if_module_configured(
                    RethRpcModule::Miner,
                    miner_ext.into_rpc(),
                )?;

                Ok(())
            })
            .await
    }
}

impl<RpcMiddleware> RethRpcAddOns for BaseAddOns<RpcMiddleware>
where
    <base_node_context::BaseNodePool<BlockchainProvider> as TransactionPool>::Transaction:
        BasePooledTx,
    RpcMiddleware: RethRpcMiddleware,
{
    fn hooks_mut(
        &mut self,
    ) -> &mut crate::RpcHooks<BaseNodeEthApi<base_node_context::BaseNodeContext>> {
        self.rpc_add_ons.hooks_mut()
    }
}

/// A regular Base EVM and executor builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BaseAddOnsBuilder<RpcMiddleware = Identity> {
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Data availability configuration for the payload builder.
    da_config: Option<BaseDAConfig>,
    /// Gas limit configuration for the payload builder.
    gas_limit_config: Option<GasLimitConfig>,
    /// Minimum suggested priority fee (tip)
    min_suggested_priority_fee: u64,
    /// RPC middleware to use
    rpc_middleware: RpcMiddleware,
    /// Optional tokio runtime to use for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
}

impl Default for BaseAddOnsBuilder {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            da_config: None,
            gas_limit_config: None,
            min_suggested_priority_fee: 1_000_000,
            rpc_middleware: Identity::new(),
            tokio_runtime: None,
        }
    }
}

impl<RpcMiddleware> BaseAddOnsBuilder<RpcMiddleware> {
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

    /// Configure the data availability configuration for the Base builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Configure the gas limit configuration for the Base payload builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = Some(gas_limit_config);
        self
    }

    /// Configure the minimum priority fee (tip)
    pub const fn with_min_suggested_priority_fee(mut self, min: u64) -> Self {
        self.min_suggested_priority_fee = min;
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
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOnsBuilder<T> {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            tokio_runtime,
            ..
        } = self;
        BaseAddOnsBuilder {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            rpc_middleware,
            tokio_runtime,
        }
    }
}

impl<RpcMiddleware> BaseAddOnsBuilder<RpcMiddleware> {
    /// Builds an instance of [`BaseAddOns`].
    pub fn build(self) -> BaseAddOns<RpcMiddleware> {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            rpc_middleware,
            tokio_runtime,
            ..
        } = self;

        BaseAddOns::new(
            RpcAddOns::new(
                BaseEthApiBuilder::default()
                    .with_sequencer(sequencer_url)
                    .with_sequencer_headers(sequencer_headers)
                    .with_min_suggested_priority_fee(min_suggested_priority_fee),
                rpc_middleware,
            )
            .with_tokio_runtime(tokio_runtime),
            da_config.unwrap_or_default(),
            gas_limit_config.unwrap_or_default(),
        )
    }
}
