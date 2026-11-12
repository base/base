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

/// Registers the `archive_` namespace on the node's authenticated RPC server (`--authrpc.*`,
/// JWT) and on IPC only, never on the public HTTP or WebSocket servers. Range calls are
/// expensive, so only operators' writers holding the JWT secret may make them.
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
            let methods = ArchiveApiImpl::new(ctx.provider().clone(), limits).into_rpc();
            ctx.auth_module.merge_auth_methods(methods.clone())?;
            let ipc = ctx.modules.merge_ipc(methods)?;
            info!(
                max_range_blocks = limits.max_range_blocks,
                max_range_bytes = limits.max_range_bytes,
                max_concurrent_ranges = limits.max_concurrent_ranges,
                ipc,
                "starting archive export RPC on the authenticated server"
            );
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
