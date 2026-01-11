//! Contains the [`TxPoolExtension`] which wires up the transaction pool features
//! (tracing ExEx and status RPC) on the Base node builder.

use base_client_node::{BaseNodeExtension, OpBuilder};
use tracing::info;

use crate::{TransactionStatusApiImpl, TransactionStatusApiServer, tracex_exex};

/// Transaction tracing toggles.
#[derive(Debug, Clone, Copy)]
pub struct TracingConfig {
    /// Enables the transaction tracing ExEx.
    pub enabled: bool,
    /// Emits `info`-level logs for the tracing ExEx when enabled.
    pub logs_enabled: bool,
}

/// Helper struct that wires the transaction pool features into the node builder.
#[derive(Debug, Clone)]
pub struct TxPoolExtension {
    /// Transaction tracing configuration flags.
    pub tracing: TracingConfig,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

impl TxPoolExtension {
    /// Creates a new transaction pool extension helper.
    pub const fn new(tracing: TracingConfig, sequencer_rpc: Option<String>) -> Self {
        Self { tracing, sequencer_rpc }
    }
}

impl BaseNodeExtension for TxPoolExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let tracing = self.tracing;
        let sequencer_rpc = self.sequencer_rpc;

        // Install the tracing ExEx if enabled
        let builder = builder.install_exex_if(tracing.enabled, "tracex", move |ctx| async move {
            Ok(tracex_exex(ctx, tracing.logs_enabled))
        });

        // Extend with RPC modules
        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Transaction Status RPC");
            let proxy_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;
            Ok(())
        })
    }
}
