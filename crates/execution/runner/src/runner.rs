//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use std::fmt;

use base_execution_payload_builder::{
    RejectionCache,
    config::{BaseDAConfig, GasLimitConfig, ResourceMeteringConfig},
};
use base_node_core::args::RollupArgs;
use eyre::Result;
use reth_node_builder::{Node, NodeHandle, NodeHandleFor};
use reth_provider::providers::BlockchainProvider;
use tracing::info;

use crate::{
    BaseNodeBuilder, BaseNodeExtension, FromExtensionConfig, NodeHooks,
    node::BaseNode,
    service::{DefaultPayloadServiceBuilder, PayloadServiceBuilder},
};

type StartedCallback = Box<dyn FnOnce() -> Result<()> + Send + 'static>;

/// Handle to a launched Base execution node.
#[derive(Debug)]
pub struct LaunchedBaseNode {
    /// The underlying reth node handle.
    pub handle: NodeHandleFor<BaseNode>,
}

/// Wraps the Base node configuration and orchestrates builder wiring.
pub struct BaseNodeRunner<SB: PayloadServiceBuilder = DefaultPayloadServiceBuilder> {
    /// Base node configuration.
    base_node: BaseNode,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Payload service builder.
    service_builder: SB,
    /// Binary-owned callbacks to run after the node has started.
    started_callbacks: Vec<StartedCallback>,
}

impl BaseNodeRunner<DefaultPayloadServiceBuilder> {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self {
            base_node: BaseNode::new(rollup_args),
            extensions: Vec::new(),
            service_builder: DefaultPayloadServiceBuilder,
            started_callbacks: Vec::new(),
        }
    }
}

impl<SB: PayloadServiceBuilder> fmt::Debug for BaseNodeRunner<SB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseNodeRunner")
            .field("rollup_args", &self.base_node.args)
            .field("extensions", &self.extensions.len())
            .field("da_config", &self.base_node.da_config)
            .field("gas_limit_config", &self.base_node.gas_limit_config)
            .field("manifest_precheck_enabled", &self.base_node.manifest_precheck_enabled)
            .field("resource_metering", &self.base_node.resource_metering)
            .field("rejection_cache", &self.base_node.rejection_cache)
            .field("started_callbacks", &self.started_callbacks.len())
            .finish()
    }
}

impl<SB: PayloadServiceBuilder> BaseNodeRunner<SB> {
    /// Sets the shared DA configuration.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.base_node.da_config = da_config;
        self
    }

    /// Sets the shared gas-limit configuration.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.base_node.gas_limit_config = gas_limit_config;
        self
    }

    /// Configures whether EIP-8130 authorization manifests are checked before execution.
    pub const fn with_manifest_precheck_enabled(mut self, enabled: bool) -> Self {
        self.base_node.manifest_precheck_enabled = enabled;
        self
    }

    /// Sets the shared resource-metering configuration.
    pub fn with_resource_metering(mut self, resource_metering: ResourceMeteringConfig) -> Self {
        self.base_node.resource_metering = resource_metering;
        self
    }

    /// Sets the shared rejection cache for permanently rejected transactions.
    pub fn with_rejection_cache(mut self, rejection_cache: RejectionCache) -> Self {
        self.base_node.rejection_cache = rejection_cache;
        self
    }

    /// Swap the payload service builder.
    pub fn with_service_builder<SB2: PayloadServiceBuilder>(self, sb: SB2) -> BaseNodeRunner<SB2> {
        BaseNodeRunner {
            base_node: self.base_node,
            extensions: self.extensions,
            service_builder: sb,
            started_callbacks: self.started_callbacks,
        }
    }

    /// Registers a new builder extension.
    pub fn install_ext<T: FromExtensionConfig + 'static>(&mut self, config: T::Config) {
        self.extensions.push(Box::new(T::from_config(config)));
    }

    /// Registers a callback to run after the node has started.
    pub fn add_started_callback<F>(&mut self, callback: F)
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        self.started_callbacks.push(Box::new(callback));
    }

    /// Applies all Base-specific wiring to the supplied builder, launches the node, and waits for
    /// shutdown.
    pub async fn run(self, builder: BaseNodeBuilder) -> Result<()> {
        let LaunchedBaseNode { handle: NodeHandle { node: _node, node_exit_future } } =
            self.launch(builder).await?;
        node_exit_future.await?;
        Ok(())
    }

    /// Applies all Base-specific wiring to the supplied builder and returns a launched node
    /// handle without waiting for shutdown.
    pub async fn launch(self, builder: BaseNodeBuilder) -> Result<LaunchedBaseNode> {
        info!(target: "base-runner", "starting custom Base node");

        let Self { base_node, extensions, service_builder, started_callbacks } = self;
        let components = service_builder.build_components(&base_node);

        let builder = builder
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(components)
            .with_add_ons(base_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        let hooks = extensions.into_iter().fold(NodeHooks::new(), |hooks, ext| ext.apply(hooks));
        let hooks = started_callbacks
            .into_iter()
            .fold(hooks, |hooks, callback| hooks.add_node_started_hook(move |_| callback()));

        let handle = hooks.apply_to(builder).launch().await?;
        Ok(LaunchedBaseNode { handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestPayloadServiceBuilder;

    impl crate::service::PayloadServiceBuilder for TestPayloadServiceBuilder {
        type ComponentsBuilder = crate::types::BaseComponentsBuilder;

        fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
            base_node.components()
        }
    }

    #[test]
    fn service_builder_swap_preserves_shared_runtime_configs() {
        let da_config = BaseDAConfig::new(100, 200);
        let gas_limit_config = GasLimitConfig::new(30_000_000);

        let runner = BaseNodeRunner::new(RollupArgs::default())
            .with_da_config(da_config.clone())
            .with_gas_limit_config(gas_limit_config.clone())
            .with_manifest_precheck_enabled(false)
            .with_resource_metering(ResourceMeteringConfig {
                enabled: true,
                ..ResourceMeteringConfig::default()
            })
            .with_rejection_cache(RejectionCache::default())
            .with_service_builder(TestPayloadServiceBuilder);

        assert!(!runner.base_node.manifest_precheck_enabled);
        let configured_da = runner.base_node.da_config;
        let configured_gas = runner.base_node.gas_limit_config;
        let configured_metering = runner.base_node.resource_metering;
        let configured_cache = runner.base_node.rejection_cache;

        assert_eq!(configured_da.max_da_tx_size(), Some(100));
        assert_eq!(configured_da.max_da_block_size(), Some(200));
        assert_eq!(configured_gas.gas_limit(), Some(30_000_000));
        assert!(configured_metering.enabled);
        assert_eq!(configured_cache.entry_count(), 0);

        da_config.set_max_da_size(300, 400);
        gas_limit_config.set_gas_limit(40_000_000);

        assert_eq!(configured_da.max_da_tx_size(), Some(300));
        assert_eq!(configured_da.max_da_block_size(), Some(400));
        assert_eq!(configured_gas.gas_limit(), Some(40_000_000));
    }
}
