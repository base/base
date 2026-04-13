//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use std::fmt;

use base_execution_payload_builder::config::OpDAConfig;
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

/// Wraps the Base node configuration and orchestrates builder wiring.
pub struct BaseNodeRunner<SB: PayloadServiceBuilder = DefaultPayloadServiceBuilder> {
    /// Rollup-specific arguments forwarded to the Base node implementation.
    rollup_args: RollupArgs,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Payload service builder.
    service_builder: SB,
    /// Shared DA configuration for the node and metering extension.
    da_config: Option<OpDAConfig>,
    /// Binary-owned callbacks to run after the node has started.
    started_callbacks: Vec<StartedCallback>,
}

impl BaseNodeRunner<DefaultPayloadServiceBuilder> {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self {
            rollup_args,
            extensions: Vec::new(),
            service_builder: DefaultPayloadServiceBuilder,
            da_config: None,
            started_callbacks: Vec::new(),
        }
    }
}

impl<SB: PayloadServiceBuilder> fmt::Debug for BaseNodeRunner<SB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseNodeRunner")
            .field("rollup_args", &self.rollup_args)
            .field("extensions", &self.extensions.len())
            .field("da_config", &self.da_config)
            .field("started_callbacks", &self.started_callbacks.len())
            .finish()
    }
}

impl<SB: PayloadServiceBuilder> BaseNodeRunner<SB> {
    /// Sets the shared DA configuration.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Swap the payload service builder.
    pub fn with_service_builder<SB2: PayloadServiceBuilder>(self, sb: SB2) -> BaseNodeRunner<SB2> {
        BaseNodeRunner {
            rollup_args: self.rollup_args,
            extensions: self.extensions,
            service_builder: sb,
            da_config: self.da_config,
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
        let Self { rollup_args, extensions, service_builder, da_config, started_callbacks } = self;
        let NodeHandle { node: _node, node_exit_future } = Self::launch_node(
            rollup_args,
            extensions,
            service_builder,
            da_config,
            started_callbacks,
            builder,
        )
        .await?;
        node_exit_future.await?;
        Ok(())
    }

    async fn launch_node(
        rollup_args: RollupArgs,
        extensions: Vec<Box<dyn BaseNodeExtension>>,
        service_builder: SB,
        da_config: Option<OpDAConfig>,
        started_callbacks: Vec<StartedCallback>,
        builder: BaseNodeBuilder,
    ) -> Result<NodeHandleFor<BaseNode>> {
        info!(target: "base-runner", "starting custom Base node");

        let mut base_node = BaseNode::new(rollup_args);
        if let Some(da_config) = da_config {
            base_node = base_node.with_da_config(da_config);
        }
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

        hooks.apply_to(builder).launch().await
    }
}
