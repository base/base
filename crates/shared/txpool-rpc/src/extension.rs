//! `TxPool` RPC extension for registering transaction pool management APIs.

use base_client_node::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{
    TransactionStatusApiImpl, TransactionStatusApiServer, TxPoolManagementApiImpl,
    TxPoolManagementApiServer,
};

/// Configuration for the `TxPool` RPC extension.
#[derive(Debug, Clone, Default)]
pub struct TxPoolRpcConfig {
    /// Sequencer RPC endpoint for transaction status proxying.
    /// If None, queries the local transaction pool.
    pub sequencer_rpc: Option<String>,
}

/// Extension that registers the `TxPool` RPC modules (`TxPoolManagementApi` and `TransactionStatusApi`).
#[derive(Debug)]
pub struct TxPoolRpcExtension {
    config: TxPoolRpcConfig,
}

impl BaseNodeExtension for TxPoolRpcExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let sequencer_rpc = self.config.sequencer_rpc;

        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            // Register TransactionStatusApi
            let status_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status API");
            ctx.modules.merge_configured(status_api.into_rpc())?;

            // Register TxPoolManagementApi
            let management_api = TxPoolManagementApiImpl::new(ctx.pool().clone());
            ctx.modules.merge_configured(management_api.into_rpc())?;

            Ok(())
        })
    }
}

impl FromExtensionConfig for TxPoolRpcExtension {
    type Config = TxPoolRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
