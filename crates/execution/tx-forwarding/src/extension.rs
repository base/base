//! Contains the [`TxForwardingExtension`] which wires up the transaction
//! forwarding pipeline on the Base node builder.

use base_execution_txpool::{SpawnedConsumer, SpawnedForwarder};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_observability_events::{TransactionEventWriter, TransactionEventWriterConfig};
use tracing::{info, warn};

use crate::TxForwardingConfig;

/// Helper struct that wires the transaction forwarding pipeline into the node builder.
#[derive(Debug)]
pub struct TxForwardingExtension {
    /// Transaction forwarding configuration.
    pub config: TxForwardingConfig,
    /// Optional writer config for durable transaction events.
    pub transaction_event_writer_config: Option<TransactionEventWriterConfig>,
}

impl TxForwardingExtension {
    /// Creates a new transaction forwarding extension.
    pub const fn new(
        config: TxForwardingConfig,
        transaction_event_writer_config: Option<TransactionEventWriterConfig>,
    ) -> Self {
        Self { config, transaction_event_writer_config }
    }
}

impl BaseNodeExtension for TxForwardingExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.config.enabled || self.config.builder_urls.is_empty() {
            return hooks;
        }

        let config = self.config;
        let writer_config = self.transaction_event_writer_config;

        hooks.add_node_started_hook(move |ctx| {
            info!(
                builder_urls = ?config.builder_urls,
                resend_after_ms = config.resend_after_ms,
                max_batch_size = config.max_batch_size,
                max_rps = config.max_rps,
                "starting transaction forwarding pipeline"
            );

            let pool = ctx.pool().clone();
            let consumer_config = config.to_consumer_config();
            let executor = ctx.task_executor;
            let task_executor = executor.clone();

            executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let event_writer = match writer_config {
                        Some(config) => match TransactionEventWriter::from_config(config).await {
                            Ok(writer) => Some(writer),
                            Err(err) => {
                                warn!(error = %err, "transaction forwarding event journal disabled");
                                None
                            }
                        },
                        None => None,
                    };

                    let forwarder_config =
                        config.with_transaction_event_writer(event_writer).to_forwarder_config();
                    let consumer = SpawnedConsumer::spawn(pool, consumer_config, &task_executor);
                    let forwarder =
                        SpawnedForwarder::spawn(&consumer.sender, forwarder_config, &task_executor);

                    let _guard = signal.await;
                    consumer.shutdown();
                    forwarder.shutdown().await;
                })
            });

            Ok(())
        })
    }
}

impl FromExtensionConfig for TxForwardingExtension {
    type Config = (TxForwardingConfig, Option<TransactionEventWriterConfig>);

    fn from_config(config: Self::Config) -> Self {
        Self::new(config.0, config.1)
    }
}
