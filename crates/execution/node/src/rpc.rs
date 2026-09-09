//! Builder support for rpc components.

use std::{fmt, fmt::Debug, ops::Deref, sync::Arc};

use base_execution_chainspec::ChainSpecProvider;
use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_execution_payload_builder::{BaseEngineValidator, PayloadBuilderHandle};
use base_execution_rpc::{
    AdminApi, BaseEthApi, BaseEthApiBuilder, BaseEthConfigApiServer,
    DebugExecutionWitnessApiServer, DevSigner, EthApiCtx, MinerApiExtServer,
};
use base_node_context::{AddOnsContext, BaseNodePool};
pub use jsonrpsee::{
    core::middleware::layer::Either,
    server::middleware::rpc::{RpcService, RpcServiceBuilder},
};
use reth_chain_state::CanonStateSubscriptions;
use reth_engine_primitives::TreeConfig;
pub use reth_engine_tree::tree::BasicEngineValidator;
use reth_node_core::node_config::NodeConfig;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_builder::{
    RpcConfig, RpcRegistryInner, RpcServerConfig, RpcServerHandle, TransportRpcModules,
};
use reth_rpc_eth_types::{EthStateCache, cache::cache_new_blocks_task};
use reth_storage_overlay::OverlayManager;
use reth_tracing::tracing::{debug, info};

use crate::{InvalidBlockHookBuilder, TxpoolPrewarmSource};

/// Handles for the Base node's public RPC services.
pub type BaseNodeRpcHandle = RpcHandle;

/// Contains the handles to the spawned RPC servers.
///
/// This can be used to access the endpoints of the servers.
#[derive(Debug, Clone)]
pub struct RethRpcServerHandles {
    /// The regular RPC server handle to all configured transports.
    pub rpc: RpcServerHandle,
}

/// Helper container for [`RpcRegistryInner`], [`TransportRpcModules`] and
/// their runtime configuration.
///
/// This can be used to access installed modules, or create commonly used handlers like
/// [`base_execution_rpc::EthApi`], and ultimately merge additional rpc handler into the configured
/// transport modules [`TransportRpcModules`].
#[expect(missing_debug_implementations)]
pub struct RpcContext<'a> {
    /// The node components.
    pub(crate) node: base_node_context::BaseNodeContext,

    /// Gives access to the node configuration.
    pub(crate) config: &'a NodeConfig,

    /// A Helper type the holds instances of the configured modules.
    ///
    /// This provides easy access to rpc handlers, such as [`RpcRegistryInner::eth_api`].
    pub registry: &'a mut RpcRegistryInner,
    /// Holds installed modules per transport type.
    ///
    /// This can be used to merge additional modules into the configured HTTP and WebSocket transports. See [`TransportRpcModules::merge_configured`]
    pub modules: &'a mut TransportRpcModules,
}

impl RpcContext<'_> {
    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig {
        self.config
    }

    /// Returns a reference to the configured node.
    ///
    /// This gives access to the node's components.
    pub const fn node(&self) -> &base_node_context::BaseNodeContext {
        &self.node
    }

    /// Returns the transaction pool instance.
    pub fn pool(&self) -> &BaseNodePool<BlockchainProvider> {
        self.node.pool()
    }

    /// Returns provider to interact with the node.
    pub fn provider(&self) -> &BlockchainProvider {
        self.node.provider()
    }

    /// Returns the handle to the network
    pub fn network(&self) -> &reth_network::NetworkHandle {
        self.node.network()
    }

    /// Returns the handle to the payload builder service
    pub fn payload_builder_handle(&self) -> &PayloadBuilderHandle {
        self.node.payload_builder_handle()
    }
}

/// Handle to the launched RPC servers.
pub struct RpcHandle {
    /// Handles to launched servers.
    pub rpc_server_handles: RethRpcServerHandles,
    /// Configured RPC modules.
    pub rpc_registry: RpcRegistryInner,
}

impl Clone for RpcHandle {
    fn clone(&self) -> Self {
        Self {
            rpc_server_handles: self.rpc_server_handles.clone(),
            rpc_registry: self.rpc_registry.clone(),
        }
    }
}

impl Deref for RpcHandle {
    type Target = RpcRegistryInner;

    fn deref(&self) -> &Self::Target {
        &self.rpc_registry
    }
}

impl Debug for RpcHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcHandle")
            .field("rpc_server_handles", &self.rpc_server_handles)
            .field("rpc_registry", &self.rpc_registry)
            .finish()
    }
}

impl RpcHandle {
    /// Returns the RPC server handles.
    pub const fn rpc_server_handles(&self) -> &RethRpcServerHandles {
        &self.rpc_server_handles
    }

    /// Returns the `EthApi` instance of the rpc server.
    pub const fn eth_api(&self) -> &BaseEthApi {
        self.rpc_registry.eth_api()
    }

    /// Returns an instance of the [`AdminApi`] for the rpc server.
    pub fn admin_api(
        &self,
    ) -> AdminApi<reth_network::NetworkHandle, BaseNodePool<BlockchainProvider>> {
        self.rpc_registry.admin_api()
    }
}

/// Prepared public RPC modules and configuration.
pub struct RpcSetupContext<'a> {
    pub node: base_node_context::BaseNodeContext,
    pub config: &'a NodeConfig,
    pub modules: TransportRpcModules,
    pub registry: RpcRegistryInner,
    /// Resolved listener configuration.
    pub server_config: RpcServerConfig,
}

impl fmt::Debug for RpcSetupContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcSetupContext").field("modules", &self.modules).finish_non_exhaustive()
    }
}

/// Starts the built-in Base RPC handlers on the configured transports.
#[derive(Debug)]
pub struct BaseRpcServer;

impl BaseRpcServer {
    /// Registers the built-in Base APIs and starts the configured public transports.
    pub async fn launch(
        ctx: AddOnsContext<'_>,
        base: &crate::BaseNode,
        services: crate::BaseRpcServices,
        node_services: &crate::PreparedNodeServices,
    ) -> eyre::Result<RpcHandle> {
        let setup = Self::setup_rpc_components(ctx, base, services, node_services).await?;
        let server_config = setup.server_config;
        let rpc = Self::launch_rpc_server_internal(server_config, &setup.modules).await?;
        let handles = RethRpcServerHandles { rpc };
        Ok(RpcHandle { rpc_server_handles: handles, rpc_registry: setup.registry })
    }

    /// Common setup for RPC server initialization
    async fn setup_rpc_components<'a>(
        ctx: AddOnsContext<'a>,
        base: &crate::BaseNode,
        mut services: crate::BaseRpcServices,
        node_services: &crate::PreparedNodeServices,
    ) -> eyre::Result<RpcSetupContext<'a>> {
        services.sequencer = base.args.sequencer.clone();
        let eth_api_builder = BaseEthApiBuilder::default()
            .with_sequencer(base.args.sequencer.clone())
            .with_sequencer_headers(base.args.sequencer_headers.clone())
            .with_min_suggested_priority_fee(base.args.min_suggested_priority_fee);

        let AddOnsContext { node, config, beacon_engine_handle, engine_events } = ctx;

        let rpc_config = RpcConfig::new(&config.rpc);
        let cache = EthStateCache::spawn_with(
            node.provider().clone(),
            rpc_config.eth.cache,
            node.task_executor().clone(),
        );

        let new_canonical_blocks = node.provider().canonical_state_stream();
        let c = cache.clone();
        node.task_executor().spawn_critical_task("cache canonical blocks task", async move {
            cache_new_blocks_task(c, new_canonical_blocks).await;
        });

        let eth_config = rpc_config.eth.max_batch_size(config.txpool.max_batch_size);
        let ctx = EthApiCtx {
            components: &node,
            config: eth_config,
            cache,
            engine_handle: beacon_engine_handle.clone(),
        };
        let eth_api = eth_api_builder.build_eth_api(ctx).await?;

        let module_config = rpc_config.modules;
        debug!(target: "reth::cli", http=?module_config.http(), ws=?module_config.ws(), "Using RPC module config");

        let mut registry = RpcRegistryInner::new(
            node.provider().clone(),
            node.pool().clone(),
            node.network().clone(),
            node.task_executor().clone(),
            node.consensus().clone(),
            module_config.config().cloned().unwrap_or_default(),
            node.evm_config().clone(),
            eth_api,
            engine_events,
        );
        let mut modules = registry.create_transport_rpc_modules(module_config);
        modules.replace_configured(Eip8130EthApiExt::new(registry.eth_api().clone()).into_rpc())?;

        // in dev mode we generate 20 random dev-signer accounts
        if config.dev.dev {
            let signers = DevSigner::from_mnemonic(config.dev.dev_mnemonic.as_str(), 20);
            registry.eth_api().signers().write().extend(signers);
        }

        let mut ctx = RpcContext {
            node: node.clone(),
            config,
            registry: &mut registry,
            modules: &mut modules,
        };

        services.register(&mut ctx)?;

        let eth_config = base_execution_rpc::BaseEthConfigHandler::new(
            node.provider().clone(),
            node.evm_config().clone(),
        );
        ctx.modules.merge_configured(eth_config.into_rpc())?;
        let payload = base_execution_payload_builder::BasePayloadBuilder::new(
            node.pool().clone(),
            node.provider().clone(),
            node.evm_config().clone(),
        );
        let witness = base_execution_rpc::BaseDebugWitnessApi::new(
            node.provider().clone(),
            node.task_executor().clone(),
            payload,
        );
        ctx.modules.merge_configured(witness.into_rpc())?;
        let miner = base_execution_rpc::BaseMinerExtApi::new(
            base.da_config.clone(),
            base.gas_limit_config.clone(),
        );
        ctx.modules.add_or_replace_configured(miner.into_rpc())?;
        node_services.register_rpc(&mut ctx)?;

        Ok(RpcSetupContext { node, config, modules, registry, server_config: rpc_config.server })
    }

    /// Helper to launch the RPC server
    async fn launch_rpc_server_internal(
        server_config: RpcServerConfig,
        modules: &TransportRpcModules,
    ) -> eyre::Result<RpcServerHandle> {
        let handle = server_config.start(modules).await?;

        if let Some(addr) = handle.http_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC HTTP server started");
        }
        if let Some(addr) = handle.ws_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC WS server started");
        }

        Ok(handle)
    }
}

/// Constructs the Base execution validator and its caches.
#[derive(Debug, Default, Clone)]
pub struct BasicEngineValidatorBuilder;

impl BasicEngineValidatorBuilder {
    /// Constructs the Base execution validator and its caches.
    pub async fn build_tree_validator(
        ctx: &AddOnsContext<'_>,
        tree_config: TreeConfig,
        overlay_manager: OverlayManager,
    ) -> eyre::Result<BasicEngineValidator<BlockchainProvider>> {
        let validator = BaseEngineValidator::new(Arc::clone(&ctx.config.chain));
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = InvalidBlockHookBuilder::build(
            ctx.config,
            &data_dir,
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
            ctx.node.provider().chain_spec().chain().id(),
        )
        .await?;

        let txpool_prewarming = tree_config.txpool_prewarming();
        let mut validator = BasicEngineValidator::new(
            ctx.node.provider().clone(),
            ctx.node.consensus().clone(),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            overlay_manager,
            ctx.node.task_executor().clone(),
        );

        if txpool_prewarming {
            validator =
                validator.with_txpool_prewarming(TxpoolPrewarmSource::new(ctx.node.pool().clone()));
        }

        Ok(validator)
    }
}
