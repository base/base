//! Node extension for registering the validated transactions RPC.

use base_client_node::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{ValidatedTxApiImpl, ValidatedTxApiServer};

/// Configuration for the validated transactions RPC extension.
#[derive(Debug, Clone, Default)]
pub struct ValidatedTxRpcConfig {
    /// Enable the validated transactions RPC.
    pub enabled: bool,
}

/// Extension that registers the validated transactions RPC module.
#[derive(Debug)]
pub struct ValidatedTxRpcExtension {
    config: ValidatedTxRpcConfig,
}

impl BaseNodeExtension for ValidatedTxRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.config.enabled {
            return hooks;
        }

        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = ValidatedTxApiImpl::new(ctx.pool().clone());
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for ValidatedTxRpcExtension {
    type Config = ValidatedTxRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
