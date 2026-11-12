//! Node extension that serves the `archive_` export namespace behind `--archive.enable`.

use base_history_store::ArchiveApiServer;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

use crate::{ArchiveApiImpl, ArchiveExportLimits};

/// Configuration of [`ArchiveExportExtension`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveExportConfig {
    /// Serve the namespace.
    pub enabled: bool,
    /// Range caps.
    pub limits: ArchiveExportLimits,
}

/// Registers the `archive_` namespace on every configured transport.
#[derive(Debug)]
pub struct ArchiveExportExtension {
    /// Configuration.
    pub config: ArchiveExportConfig,
}

impl BaseNodeExtension for ArchiveExportExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.config.enabled {
            return hooks;
        }
        let limits = self.config.limits;
        hooks.add_rpc_module(move |ctx| {
            info!(
                max_range_blocks = limits.max_range_blocks,
                max_range_bytes = limits.max_range_bytes,
                "starting archive export RPC"
            );
            let api = ArchiveApiImpl::new(ctx.provider().clone(), limits);
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for ArchiveExportExtension {
    type Config = ArchiveExportConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
