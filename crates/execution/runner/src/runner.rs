//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use std::fmt;

use base_execution_payload_builder::{
    RejectionCache,
    config::{BaseDAConfig, GasLimitConfig, ResourceMeteringConfig},
};
use base_node_core::{BasePayloadServiceConfig, NodeHandle, RollupArgs};
use eyre::Result;
use tracing::info;

use crate::{BaseNode, BaseNodeBuilder, BaseNodeHandle};

/// Handle to a launched Base execution node.
#[derive(Debug)]
pub struct LaunchedBaseNode {
    /// The underlying reth node handle.
    pub handle: BaseNodeHandle,
}

/// Wraps the Base node configuration and orchestrates builder wiring.
pub struct BaseNodeRunner {
    /// Runtime inputs for the built-in Base RPC handlers.
    pub rpc: base_node_core::BaseRpcServices,
    /// Rollup-specific arguments forwarded to the Base node implementation.
    rollup_args: RollupArgs,
    /// Registered builder extensions.
    pub services: base_node_core::NodeServices,
    /// Payload service builder.
    service_builder: Option<BasePayloadServiceConfig>,
    /// Shared DA configuration for the node and payload builder.
    da_config: Option<BaseDAConfig>,
    /// Shared gas-limit configuration for the node and payload builder.
    gas_limit_config: Option<GasLimitConfig>,
    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution.
    manifest_precheck_enabled: bool,
    /// Shared resource-metering configuration for the native payload builder.
    resource_metering: Option<ResourceMeteringConfig>,
    /// Shared rejection cache for permanently rejected transaction hashes.
    rejection_cache: Option<RejectionCache>,
}

impl BaseNodeRunner {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self {
            rpc: Default::default(),
            rollup_args,
            services: Default::default(),
            service_builder: None,
            da_config: None,
            gas_limit_config: None,
            manifest_precheck_enabled: true,
            resource_metering: None,
            rejection_cache: None,
        }
    }
}

impl fmt::Debug for BaseNodeRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseNodeRunner")
            .field("rollup_args", &self.rollup_args)
            .field("services", &self.services)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("manifest_precheck_enabled", &self.manifest_precheck_enabled)
            .field("resource_metering", &self.resource_metering)
            .field("rejection_cache", &self.rejection_cache)
            .finish()
    }
}

impl BaseNodeRunner {
    /// Sets the shared DA configuration.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Sets the shared gas-limit configuration.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = Some(gas_limit_config);
        self
    }

    /// Configures whether EIP-8130 authorization manifests are checked before execution.
    pub const fn with_manifest_precheck_enabled(mut self, enabled: bool) -> Self {
        self.manifest_precheck_enabled = enabled;
        self
    }

    /// Sets the shared resource-metering configuration.
    pub fn with_resource_metering(mut self, resource_metering: ResourceMeteringConfig) -> Self {
        self.resource_metering = Some(resource_metering);
        self
    }

    /// Sets the shared rejection cache for permanently rejected transactions.
    pub fn with_rejection_cache(mut self, rejection_cache: RejectionCache) -> Self {
        self.rejection_cache = Some(rejection_cache);
        self
    }

    /// Selects a concrete payload service configuration.
    pub fn with_service_builder(mut self, service_builder: BasePayloadServiceConfig) -> Self {
        self.service_builder = Some(service_builder);
        self
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
        let handle = self.launch_node(builder).await?;
        Ok(LaunchedBaseNode { handle })
    }

    async fn launch_node(self, mut builder: BaseNodeBuilder) -> Result<BaseNodeHandle> {
        info!(target: "base-runner", "starting custom Base node");

        let Self {
            rollup_args,
            mut rpc,
            services,
            service_builder,
            da_config,
            gas_limit_config,
            manifest_precheck_enabled,
            resource_metering,
            rejection_cache,
        } = self;
        let mut base_node = BaseNode::new(rollup_args.clone());
        if let Some(da_config) = da_config {
            base_node = base_node.with_da_config(da_config);
        }
        if let Some(gas_limit_config) = gas_limit_config {
            base_node = base_node.with_gas_limit_config(gas_limit_config);
        }
        base_node = base_node.with_manifest_precheck_enabled(manifest_precheck_enabled);
        if let Some(resource_metering) = &resource_metering {
            base_node = base_node.with_resource_metering(resource_metering.clone());
        }
        if let Some(rejection_cache) = &rejection_cache {
            base_node = base_node.with_rejection_cache(rejection_cache.clone());
        }
        let payload = service_builder.map(|mut service| {
            if let Some(resource_metering) = resource_metering {
                service.config.resource_metering = resource_metering;
            }
            if let Some(rejection_cache) = rejection_cache {
                service.config.rejection_cache = rejection_cache;
            }
            service
        });
        rpc.sequencer = rollup_args.sequencer.clone();
        builder.base = base_node;
        builder.payload = payload;
        builder.rpc = rpc;
        builder.services = services;

        builder.launch().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .with_service_builder(BasePayloadServiceConfig::default());

        assert!(!runner.manifest_precheck_enabled);
        let configured_da = runner.da_config.expect("DA config should be preserved");
        let configured_gas = runner.gas_limit_config.expect("gas-limit config should be preserved");
        let configured_metering =
            runner.resource_metering.expect("resource metering should be preserved");
        let configured_cache = runner.rejection_cache.expect("rejection cache should be preserved");

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
