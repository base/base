//! Contains the [`TxForwardingExtension`] which wires up the transaction
//! forwarding pipeline on the Base node builder.

use std::{sync::Arc, time::Duration};

use base_execution_txpool::{InlineSimQueue, TransactionValidity};
use base_metering::MeteringApiImpl;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tokio::sync::mpsc;
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
        let config = self.config;
        let inline_rx = if config.inline_simulation {
            let (sender, receiver) = mpsc::channel(config.inline_simulation_queue_capacity);
            InlineSimQueue::install(sender);
            info!(
                workers = config.inline_simulation_workers,
                queue_capacity = config.inline_simulation_queue_capacity,
                timeout_ms = config.inline_simulation_timeout_ms,
                "installed inline simulation queue"
            );
            Some(receiver)
        } else {
            None
        };

        if (!config.enabled || config.builder_urls.is_empty()) && inline_rx.is_none() {
            return hooks;
        }

        hooks.add_node_started_hook(move |ctx| {
            if let Some(receiver) = inline_rx {
                let meter = Arc::new(MeteringApiImpl::new(
                    ctx.provider.clone(),
                    config.flashblocks_state.clone().unwrap_or_default(),
                    Arc::clone(&config.metered_opcodes),
                ));
                InlineSimQueue::spawn_workers(
                    ctx.pool().clone(),
                    move |tx| meter.meter_transaction(tx).map_err(|error| error.to_string()),
                    receiver,
                    config.inline_simulation_workers,
                    Duration::from_millis(config.inline_simulation_timeout_ms),
                );
                info!(
                    workers = config.inline_simulation_workers,
                    "started inline simulation workers"
                );
            }

            if !config.enabled || config.builder_urls.is_empty() {
                return Ok(());
            }

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
