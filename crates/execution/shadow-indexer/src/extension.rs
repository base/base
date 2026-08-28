use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_shadow_indexer_db::ShadowDbConfig;
use tokio::sync::mpsc;

use crate::{ShadowIndexerExEx, ShadowRetention, ShadowRetentionConfig, ShadowWriter};

/// Configuration for the shadow indexer extension.
#[derive(Clone, Debug)]
pub struct ShadowIndexerConfig {
    /// Whether the shadow indexer pipeline is enabled.
    pub enabled: bool,
    /// Database configuration for the shadow indexer writer.
    pub db: ShadowDbConfig,
    /// Builder version string to attach to persisted rows.
    pub builder_version: String,
    /// Retention policy that bounds shadow block table growth.
    pub retention: ShadowRetentionConfig,
}

/// Wires the shadow indexer `ExEx` into the Base node.
#[derive(Debug)]
pub struct ShadowIndexerExtension {
    cfg: ShadowIndexerConfig,
}

impl FromExtensionConfig for ShadowIndexerExtension {
    type Config = ShadowIndexerConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { cfg: config }
    }
}

impl BaseNodeExtension for ShadowIndexerExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.cfg.enabled {
            return hooks;
        }

        let (tx, rx) = mpsc::channel(1024);
        let db = self.cfg.db.clone();
        let retention_db = self.cfg.db.clone();
        let retention = self.cfg.retention;
        let builder_version = self.cfg.builder_version.clone();

        hooks
            .add_node_started_hook(move |node| {
                let executor = node.task_executor;
                ShadowRetention::spawn(&executor, retention_db, retention);
                ShadowWriter::spawn(executor, rx, db, builder_version);
                Ok(())
            })
            .install_exex("shadow-indexer", move |ctx| async move {
                Ok(ShadowIndexerExEx::new(tx).run(ctx))
            })
    }
}
