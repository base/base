//! Contains the [`TxForwardingExtension`] which wires up the transaction
//! forwarding pipeline on the Base node builder.

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_txpool::{ConsumerHandle, ForwarderHandle};
use tracing::info;

use crate::TxForwardingConfig;

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

        hooks.add_rpc_module(move |ctx| {
            info!(
                builder_urls = ?config.builder_urls,
                resend_after_ms = config.resend_after_ms,
                max_batch_size = config.max_batch_size,
                max_rps = config.max_rps,
                "starting transaction forwarding pipeline"
            );

            let pool = ctx.pool().clone();

            // Spawn the consumer on a dedicated OS thread. It continuously reads
            // from best_transactions(), deduplicates, and broadcasts to subscribers.
            let consumer_config = config.to_consumer_config();
            let consumer_handle = ConsumerHandle::spawn(pool, consumer_config);

            // This forwarder handle will spawn one forwarder async task per builder URL. Each subscribes to
            // the consumer's broadcast channel and forwards transactions via RPC.
            let forwarder_config = config.to_forwarder_config();
            let forwarder_handle =
                ForwarderHandle::spawn(&consumer_handle.sender, forwarder_config);

            // Detach handles so the consumer and forwarder tasks run for the
            // process lifetime. Both handles cancel their tasks on Drop, but this
            // closure's scope is too short — detach() prevents Drop from firing
            // while keeping the Drop impl available for callers who want clean shutdown.
            consumer_handle.detach();
            forwarder_handle.detach();

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
