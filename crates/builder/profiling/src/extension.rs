//! Node extension that owns the profiler's lifetime and binds the profiling HTTP server
//! alongside the builder's other observability endpoints.

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::{CpuProfiler, ProfilingServer};

#[cfg(test)]
std::thread_local! {
    static NODE_STARTED_HOOK_REGISTRATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

/// Runtime configuration for the profiling extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfilingConfig {
    /// Whether the profiling HTTP server is enabled.
    pub enabled: bool,
    /// TCP port used by the profiling HTTP server.
    pub port: u16,
    /// Maximum requested profile duration in seconds.
    pub max_seconds: u64,
    /// Sampling frequency used when a request omits one.
    pub default_frequency: u32,
}

/// Wires the profiling HTTP server into the Base node.
#[derive(Debug)]
pub struct ProfilingExtension {
    cfg: ProfilingConfig,
}

impl FromExtensionConfig for ProfilingExtension {
    type Config = ProfilingConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { cfg: config }
    }
}

impl BaseNodeExtension for ProfilingExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.cfg.enabled {
            return hooks;
        }

        let port = self.cfg.port;

        #[cfg(test)]
        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(count.get() + 1));

        hooks.add_node_started_hook(move |node| {
            let executor = node.task_executor;
            let server =
                ProfilingServer::new(port, CpuProfiler::default(), CancellationToken::new());
            warn!(
                port = %port,
                "CPU profiling endpoint ENABLED - do not run this configuration on the main builder"
            );

            // Unlike shadow-indexer's fail-fast writer, profiling is optional observability. A
            // server failure is logged but must not take the builder down.
            executor.spawn_task(async move {
                if let Err(error) = server.serve().await {
                    error!(error = %error, port = %port, "profiling server failed");
                }
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn config(enabled: bool) -> ProfilingConfig {
        ProfilingConfig { enabled, port: 0, max_seconds: 60, default_frequency: 101 }
    }

    #[test]
    fn enabled_extension_registers_one_node_started_hook() {
        // NodeHooks intentionally hides its vectors, so count calls at its registration boundary.
        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(0));
        let extension = ProfilingExtension::from_config(config(true));

        let hooks = Box::new(extension).apply(NodeHooks::new());

        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| assert_eq!(count.get(), 1));
        drop(hooks);
    }

    #[test]
    fn disabled_extension_preserves_existing_hooks_without_registering_one() {
        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(0));
        let marker = Arc::new(());
        let hook_marker = Arc::clone(&marker);
        let hooks = NodeHooks::new().add_node_started_hook(move |_| {
            drop(hook_marker);
            Ok(())
        });
        let extension = ProfilingExtension::from_config(config(false));

        let hooks = Box::new(extension).apply(hooks);

        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| assert_eq!(count.get(), 0));
        assert_eq!(Arc::strong_count(&marker), 2);
        drop(hooks);
        assert_eq!(Arc::strong_count(&marker), 1);
    }
}
