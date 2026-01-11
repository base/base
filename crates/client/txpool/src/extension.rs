//! Contains the [`TxPoolExtension`] which wires up the transaction pool features
//! (tracing ExEx and status RPC) on the Base node builder.

use base_client_node::{BaseNodeExtension, OpBuilder};
use tracing::info;

use crate::{TransactionStatusApiImpl, TransactionStatusApiServer, tracex_exex};

/// Transaction pool configuration.
#[derive(Debug, Clone)]
pub struct TxpoolConfig {
    /// Enables the transaction tracing ExEx.
    pub tracing_enabled: bool,
    /// Emits `info`-level logs for the tracing ExEx when enabled.
    pub tracing_logs_enabled: bool,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

/// Helper struct that wires the transaction pool features into the node builder.
#[derive(Debug, Clone)]
pub struct TxPoolExtension {
    /// Transaction pool configuration.
    config: TxpoolConfig,
}

impl TxPoolExtension {
    /// Creates a new transaction pool extension helper.
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(config: TxpoolConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for TxPoolExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let config = self.config;

        // Install the tracing ExEx if enabled
        let logs_enabled = config.tracing_logs_enabled;
        let builder =
            builder.install_exex_if(config.tracing_enabled, "tracex", move |ctx| async move {
                Ok(tracex_exex(ctx, logs_enabled))
            });

        // Extend with RPC modules
        let sequencer_rpc = config.sequencer_rpc;
        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Transaction Status RPC");
            let proxy_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;
            Ok(())
        })
    }
}
