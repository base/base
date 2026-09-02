//! Node-builder extension installing the B20 storage prefetch pool.

use std::sync::Arc;

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_precompile_storage::PrefetchHint;
use tracing::{info, warn};

use crate::B20PrefetchPool;

/// Configuration for [`B20PrefetchExtension`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct B20PrefetchConfig {
    /// Number of prefetch worker threads. Zero disables prefetching entirely.
    pub workers: usize,
}

/// Wires the B20 storage prefetch pool into the node builder.
///
/// When enabled, spawns a [`B20PrefetchPool`] against the node's state provider once the node
/// has started and installs it as the process-wide [`PrefetchHint`] sink consumed by B20
/// precompile dispatch.
#[derive(Debug)]
pub struct B20PrefetchExtension {
    config: B20PrefetchConfig,
}

impl FromExtensionConfig for B20PrefetchExtension {
    type Config = B20PrefetchConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for B20PrefetchExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let workers = self.config.workers;
        if workers == 0 {
            info!(message = "B20 storage prefetch is disabled");
            return hooks;
        }
        hooks.add_node_started_hook(move |node| {
            let pool = B20PrefetchPool::spawn(node.provider().clone(), workers);
            if PrefetchHint::install(Arc::new(pool)) {
                info!(workers = %workers, "installed B20 storage prefetcher");
            } else {
                warn!(message = "B20 storage prefetcher was already installed");
            }
            Ok(())
        })
    }
}
