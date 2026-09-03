//! Node-builder extension installing the storage prefetch pool.

use std::sync::Arc;

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_precompile_storage::PrefetchHint;
use tracing::{info, warn};

use crate::StoragePrefetchPool;

/// Configuration for [`StoragePrefetchExtension`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoragePrefetchConfig {
    /// Number of prefetch worker threads. Zero disables prefetching entirely.
    pub workers: usize,
}

/// Wires the storage prefetch pool into the node builder.
///
/// When enabled, spawns a [`StoragePrefetchPool`] against the node's state provider once the
/// node has started and installs it as the process-wide [`PrefetchHint`] sink consumed by hint
/// producers (currently the B20 precompiles).
#[derive(Debug)]
pub struct StoragePrefetchExtension {
    config: StoragePrefetchConfig,
}

impl FromExtensionConfig for StoragePrefetchExtension {
    type Config = StoragePrefetchConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for StoragePrefetchExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let workers = self.config.workers;
        if workers == 0 {
            info!(message = "storage prefetch is disabled");
            return hooks;
        }
        hooks.add_node_started_hook(move |node| {
            let pool = StoragePrefetchPool::spawn(node.provider().clone(), workers);
            if PrefetchHint::install(Arc::new(pool)) {
                info!(workers = %workers, "installed storage prefetcher");
            } else {
                warn!(message = "storage prefetcher was already installed");
            }
            Ok(())
        })
    }
}
