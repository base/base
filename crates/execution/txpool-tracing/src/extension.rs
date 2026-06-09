//! Contains the [`TxPoolExtension`] which wires up the transaction pool tracing
//! subscription on the Base node builder.

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_provider::CanonStateSubscriptions;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

use crate::tracex_subscription;

/// Transaction pool tracing configuration.
#[derive(Debug, Clone)]
pub struct TxpoolConfig {
    /// Enables transaction tracing.
    pub tracing_enabled: bool,
    /// Emits `info`-level logs for transaction tracing when enabled.
    pub tracing_logs_enabled: bool,
}

/// Helper struct that wires the transaction pool tracing into the node builder.
#[derive(Debug, Clone)]
pub struct TxPoolExtension {
    /// Transaction pool configuration.
    config: TxpoolConfig,
}

impl TxPoolExtension {
    /// Creates a new transaction pool extension helper.
    pub const fn new(config: TxpoolConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for TxPoolExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;

        let tracing_enabled = config.tracing_enabled;
        let logs_enabled = config.tracing_logs_enabled;

        // Start tracing subscription if enabled
        if !tracing_enabled {
            return builder;
        }

        builder.add_rpc_module(move |ctx| {
            info!("starting transaction tracing subscription");
            let canonical_stream =
                BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
            let pool = ctx.pool().clone();

            tokio::spawn(tracex_subscription(canonical_stream, pool, logs_enabled));

            Ok(())
        })
    }
}

impl FromExtensionConfig for TxPoolExtension {
    type Config = TxpoolConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
