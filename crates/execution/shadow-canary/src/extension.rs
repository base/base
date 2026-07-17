use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tokio::sync::mpsc;

use crate::{run_exex, spawn_writer};
use base_shadow_canary_db::ShadowDbConfig;

/// Configuration for the shadow canary extension.
#[derive(Clone, Debug)]
pub struct ShadowCanaryConfig {
    /// Whether the shadow canary pipeline is enabled.
    pub enabled: bool,
    /// Database configuration for the shadow canary writer.
    pub db: ShadowDbConfig,
    /// Builder version string to attach to persisted rows.
    pub builder_version: String,
}

/// Wires the shadow canary ExEx into the Base node.
#[derive(Debug)]
pub struct ShadowCanaryExtension {
    cfg: ShadowCanaryConfig,
}

impl FromExtensionConfig for ShadowCanaryExtension {
    type Config = ShadowCanaryConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { cfg: config }
    }
}

impl BaseNodeExtension for ShadowCanaryExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.cfg.enabled {
            return hooks;
        }

        let (tx, rx) = mpsc::channel(1024);
        let db = self.cfg.db.clone();
        let builder_version = self.cfg.builder_version.clone();

        hooks
            .add_node_started_hook(move |node| {
                spawn_writer(node.task_executor.clone(), rx, db, builder_version);
                Ok(())
            })
            .install_exex("shadow-canary", move |ctx| async move { Ok(run_exex(ctx, tx)) })
    }
}
