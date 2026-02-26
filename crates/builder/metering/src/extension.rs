//! Builder-specific node extensions.

use std::sync::Arc;

use base_builder_core::SharedMeteringProvider;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{BaseApiExtServer, MeteringStore, MeteringStoreExt};

/// Extension that registers the [`MeteringStoreExt`] RPC module.
#[derive(Debug)]
pub struct MeteringStoreExtension {
    metering_provider: SharedMeteringProvider,
}

impl BaseNodeExtension for MeteringStoreExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let metering_provider = self.metering_provider;
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let ext = MeteringStoreExt::new(metering_provider);
            ctx.modules.add_or_replace_configured(ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for MeteringStoreExtension {
    type Config = MeteringStore;

    fn from_config(config: Self::Config) -> Self {
        Self { metering_provider: Arc::new(config) }
    }
}
