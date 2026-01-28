//! Contains the [`TxPoolExtension`] which wires up the transaction pool features
//! (tracing subscription and status RPC) on the Base node builder.

use std::sync::Arc;

use base_client_node::{BaseBuilder, BaseNodeExtension, FromExtensionConfig};
use base_flashblocks::{FlashblocksConfig, FlashblocksState};
use reth_provider::CanonStateSubscriptions;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

use crate::{TransactionStatusApiImpl, TransactionStatusApiServer, tracex_subscription};

/// Transaction pool configuration.
#[derive(Debug, Clone)]
pub struct TxpoolConfig {
    /// Enables transaction tracing.
    pub tracing_enabled: bool,
    /// Emits `info`-level logs for transaction tracing when enabled.
    pub tracing_logs_enabled: bool,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
    /// Optional Flashblocks configuration (includes state).
    pub flashblocks_config: Option<FlashblocksConfig>,
}

/// Helper struct that wires the transaction pool features into the node builder.
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
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        let config = self.config;

        // Extend with RPC modules and optionally start tracing subscription
        let sequencer_rpc = config.sequencer_rpc;
        let tracing_enabled = config.tracing_enabled;
        let logs_enabled = config.tracing_logs_enabled;
        let flashblocks_config = config.flashblocks_config;

        builder.add_rpc_module(move |ctx| {
            info!(message = "Starting Transaction Status RPC");
            let proxy_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;

            // Start the tracing subscription if enabled
            if tracing_enabled {
                let canonical_stream =
                    BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
                let pool = ctx.pool().clone();

                // Get flashblocks state from config, or create a default one if not configured
                let fb_state: Arc<FlashblocksState> = flashblocks_config
                    .as_ref()
                    .map(|cfg| Arc::clone(&cfg.state))
                    .unwrap_or_default();

                tokio::spawn(tracex_subscription(canonical_stream, fb_state, pool, logs_enabled));
            }

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
