//! Contains the [`TxForwardingExtension`] which wires up the transaction
//! forwarding pipeline on the Base node builder.

use base_execution_txpool::TransactionValidity;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

use crate::{TxForwardingConfig, TxForwardingService};

/// Helper struct that wires the transaction forwarding pipeline into the node builder.
#[derive(Debug)]
pub struct TxForwardingExtension {
    /// Transaction forwarding configuration.
    pub config: TxForwardingConfig,
}

impl TxForwardingExtension {
    /// Creates a new transaction forwarding extension.
    pub const fn new(config: TxForwardingConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for TxForwardingExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.config.enabled || self.config.builder_urls.is_empty() {
            return hooks;
        }

        let config = self.config;

        hooks.add_node_started_hook(move |ctx| {
            info!(
                builder_urls = ?config.builder_urls,
                resend_after_ms = config.resend_after_ms,
                max_batch_size = config.max_batch_size,
                max_rps = config.max_rps,
                "starting transaction forwarding pipeline"
            );

            let pool = ctx.pool().clone();
            let executor = ctx.task_executor;
            let handle = TxForwardingService::new(config)
                .spawn_with_extensions::<_, TransactionValidity>(pool, &executor);

            executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let _guard = signal.await;
                    let report = handle.shutdown().await;
                    info!(?report, "transaction forwarding pipeline stopped");
                })
            });

            Ok(())
        })
    }
}

impl FromExtensionConfig for TxForwardingExtension {
    type Config = TxForwardingConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
