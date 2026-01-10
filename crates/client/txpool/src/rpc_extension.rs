//! Contains the [`TransactionStatusRpcExtension`] which wires up the transaction status
//! RPC surface on the Base node builder.

use base_client_primitives::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder};

use crate::{TransactionStatusApiImpl, TransactionStatusApiServer};

/// Helper struct that wires the transaction status RPC into the node builder.
#[derive(Debug, Clone)]
pub struct TransactionStatusRpcExtension {
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

impl TransactionStatusRpcExtension {
    /// Creates a new transaction status RPC extension.
    pub const fn new(sequencer_rpc: Option<String>) -> Self {
        Self { sequencer_rpc }
    }
}

impl BaseNodeExtension for TransactionStatusRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let sequencer_rpc = self.sequencer_rpc;

        builder.extend_rpc_modules(move |ctx| {
            let proxy_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;
            Ok(())
        })
    }
}

/// Configuration trait for [`TransactionStatusRpcExtension`].
///
/// Types implementing this trait can be used to construct a [`TransactionStatusRpcExtension`].
pub trait TransactionStatusRpcConfig {
    /// Returns the sequencer RPC URL if configured.
    fn sequencer_rpc(&self) -> Option<&str>;
}

impl<C: TransactionStatusRpcConfig> ConfigurableBaseNodeExtension<C>
    for TransactionStatusRpcExtension
{
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(config.sequencer_rpc().map(String::from)))
    }
}
