//! Node-builder extension installing the state prefetch pool.

use std::sync::Arc;

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_precompile_storage::PrefetchHint;
use tracing::{info, warn};

use crate::{MAX_PREFETCH_WORKERS, StatePrefetchPool};

/// Configuration for [`StatePrefetchExtension`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatePrefetchConfig {
    /// Number of prefetch worker threads. Zero disables prefetching entirely.
    pub workers: usize,
}

/// Wires the state prefetch pool into the node builder.
#[derive(Debug)]
pub struct StatePrefetchExtension {
    config: StatePrefetchConfig,
}

impl FromExtensionConfig for StatePrefetchExtension {
    type Config = StatePrefetchConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for StatePrefetchExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let workers = self.config.workers;
        if workers == 0 {
            return hooks;
        }
        let workers = workers.min(MAX_PREFETCH_WORKERS);
        if workers < self.config.workers {
            warn!(requested = %self.config.workers, clamped = %workers, "state prefetch workers clamped to maximum");
        }
        hooks.add_node_started_hook(move |node| {
            let pool = StatePrefetchPool::spawn(node.provider().clone(), workers);
            if PrefetchHint::install(Arc::new(pool)) {
                info!(workers = %workers, "installed state prefetcher");
            } else {
                warn!("state prefetcher was already installed");
            }
            Ok(())
        })
    }
}
