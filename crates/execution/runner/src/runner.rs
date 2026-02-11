//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

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

/// Wraps the Base node configuration and orchestrates builder wiring.
#[derive(Debug)]
pub struct BaseNodeRunner<SB: PayloadServiceBuilder = DefaultPayloadServiceBuilder> {
    /// Rollup-specific arguments forwarded to the Base node implementation.
    rollup_args: RollupArgs,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Payload service builder.
    service_builder: SB,
    /// Shared DA configuration for the node and metering extension.
    da_config: Option<OpDAConfig>,
}

impl BaseNodeRunner<DefaultPayloadServiceBuilder> {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self {
            rollup_args,
            extensions: Vec::new(),
            service_builder: DefaultPayloadServiceBuilder,
            da_config: None,
        }
    }

    /// Sets the shared DA configuration.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }
}

impl<SB: PayloadServiceBuilder> BaseNodeRunner<SB> {
    /// Swap the payload service builder.
    pub fn with_service_builder<SB2: PayloadServiceBuilder>(self, sb: SB2) -> BaseNodeRunner<SB2> {
        BaseNodeRunner {
            rollup_args: self.rollup_args,
            extensions: self.extensions,
            service_builder: sb,
            da_config: self.da_config,
        }
    }

    /// Registers a new builder extension.
    pub fn install_ext<T: FromExtensionConfig + 'static>(&mut self, config: T::Config) {
        self.extensions.push(Box::new(T::from_config(config)));
    }

    /// Applies all Base-specific wiring to the supplied builder, launches the node, and waits for
    /// shutdown.
    pub async fn run(self, builder: BaseNodeBuilder) -> Result<()> {
        let Self { rollup_args, extensions, service_builder, da_config } = self;
        let NodeHandle { node: _node, node_exit_future } =
            Self::launch_node(rollup_args, extensions, service_builder, da_config, builder).await?;
        node_exit_future.await?;
        Ok(())
    }

    async fn launch_node(
        rollup_args: RollupArgs,
        extensions: Vec<Box<dyn BaseNodeExtension>>,
        service_builder: SB,
        da_config: Option<OpDAConfig>,
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

        extensions
            .into_iter()
            .fold(NodeHooks::new(), |b, ext| ext.apply(b))
            .add_node_started_hook(|_| {
                base_cli_utils::register_version_metrics!();
                Ok(())
            })
            .apply_to(builder)
            .launch()
            .await
    }
}
