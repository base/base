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
    profiler: CpuProfiler,
}

impl FromExtensionConfig for ProfilingExtension {
    type Config = ProfilingConfig;

    fn from_config(config: Self::Config) -> Self {
        let profiler = CpuProfiler::new(config.max_seconds, config.default_frequency);
        Self { cfg: config, profiler }
    }
}

impl BaseNodeExtension for ProfilingExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.cfg.enabled {
            return hooks;
        }

        let port = self.cfg.port;
        let profiler = self.profiler;

        #[cfg(test)]
        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(count.get() + 1));

        hooks.add_node_started_hook(move |node| {
            let executor = node.task_executor;
            warn!(
                port = %port,
                "CPU profiling endpoint ENABLED - do not run this configuration on the main builder"
            );

            // Unlike shadow-indexer's fail-fast writer, profiling is optional observability. A
            // server failure is logged but must not take the builder down.
            executor.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
                let cancel = CancellationToken::new();
                let server = ProfilingServer::new(port, profiler, cancel.clone());
                let serving = server.serve();
                tokio::pin!(serving);

                let result = tokio::select! {
                    result = &mut serving => result,
                    guard = shutdown => {
                        let _guard = guard;
                        cancel.cancel();
                        serving.await
                    }
                };
                if let Err(error) = result {
                    error!(error = %error, port = %port, "profiling server failed");
                }
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, sync::Arc, time::Duration};

    use tokio::{net::TcpStream, task::yield_now, time::timeout};

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
    fn extension_constructs_profiler_from_configured_values() {
        let extension = ProfilingExtension::from_config(ProfilingConfig {
            max_seconds: 7,
            default_frequency: 307,
            ..config(true)
        });

        assert_eq!(extension.profiler.max_capture_seconds(), 7);
        assert_eq!(extension.profiler.default_frequency_hz(), 307);
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

    #[tokio::test(flavor = "multi_thread")]
    async fn profiling_extension_opens_socket_only_when_enabled() -> eyre::Result<()> {
        let reserved = TcpListener::bind("127.0.0.1:0")?;
        let port = reserved.local_addr()?.port();
        drop(reserved);
        let address = ("127.0.0.1", port);

        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(0));
        let disabled = ProfilingExtension::from_config(ProfilingConfig { port, ..config(false) });
        let disabled_hooks = Box::new(disabled).apply(NodeHooks::new());

        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| assert_eq!(count.get(), 0));
        assert!(TcpStream::connect(address).await.is_err());
        let unclaimed = TcpListener::bind(address)?;
        drop(unclaimed);
        drop(disabled_hooks);

        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| count.set(0));
        let enabled = ProfilingExtension::from_config(ProfilingConfig { port, ..config(true) });
        let enabled_hooks = Box::new(enabled).apply(NodeHooks::new());
        NODE_STARTED_HOOK_REGISTRATIONS.with(|count| assert_eq!(count.get(), 1));
        let cancel = CancellationToken::new();
        let server = ProfilingServer::new(port, CpuProfiler::default(), cancel.clone());
        let serving = tokio::spawn(server.serve());
        let connection = timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(connection) = TcpStream::connect(address).await {
                    break connection;
                }
                yield_now().await;
            }
        })
        .await?;

        drop(connection);
        cancel.cancel();
        timeout(Duration::from_secs(2), serving).await???;
        drop(enabled_hooks);
        Ok(())
    }
}
