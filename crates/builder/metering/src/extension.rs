//! Builder-specific node extensions.

use base_builder_core::{BuilderMetrics, SharedMeteringProvider, SharedResourceThrottleStore};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{BaseApiExtServer, MeteringStoreExt, ResourceThrottleApiServer, ResourceThrottleExt};

/// Configuration shared by the builder metering ingestion and control RPCs.
#[derive(Debug, Clone)]
pub struct MeteringStoreExtensionConfig {
    /// Provider receiving metering responses.
    pub metering_provider: SharedMeteringProvider,
    /// Resource-throttle schedule store used by the builder.
    pub resource_throttle_store: SharedResourceThrottleStore,
}

/// Extension that registers the [`MeteringStoreExt`] RPC module.
#[derive(Debug)]
pub struct MeteringStoreExtension {
    metering_provider: SharedMeteringProvider,
    resource_throttle_store: SharedResourceThrottleStore,
}

impl BaseNodeExtension for MeteringStoreExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let metering_provider = self.metering_provider;
        let resource_throttle_store = self.resource_throttle_store;
        BuilderMetrics::resource_throttle_schedule_revision()
            .set(resource_throttle_store.revision() as f64);
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let ext = MeteringStoreExt::new(metering_provider);
            ctx.modules.add_or_replace_configured(ext.into_rpc())?;
            let resource_throttle_ext = ResourceThrottleExt::new(resource_throttle_store);
            ctx.auth_module.merge_auth_methods(resource_throttle_ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for MeteringStoreExtension {
    type Config = MeteringStoreExtensionConfig;

    fn from_config(config: Self::Config) -> Self {
        Self {
            metering_provider: config.metering_provider,
            resource_throttle_store: config.resource_throttle_store,
        }
    }
}
