//! `TxPool` RPC extension for registering transaction pool management APIs.

use std::fmt;

use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use reth_rpc_server_types::RethRpcModule;

use crate::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, TransactionStatusApiImpl, TransactionStatusApiServer,
};

/// Configuration for the `TxPool` RPC extension.
#[derive(Debug, Clone, Default)]
pub struct TxPoolRpcConfig {
    /// Sequencer RPC endpoint for transaction status proxying.
    /// If None, queries the local transaction pool.
    pub sequencer_rpc: Option<String>,
}

/// Extension that registers the `TxPool` RPC modules (`AdminTxPoolApi` and `TransactionStatusApi`).
#[derive(Debug)]
pub struct TxPoolRpcExtension {
    config: TxPoolRpcConfig,
}

impl<E> BaseNodeExtension<E> for TxPoolRpcExtension
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    fn apply(self: Box<Self>, builder: NodeHooks<E>) -> NodeHooks<E> {
        let sequencer_rpc = self.config.sequencer_rpc;

        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_, E>| {
            // Register TransactionStatusApi
            let status_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status API");
            ctx.modules.merge_configured(status_api.into_rpc())?;

            // Register AdminTxPoolApi
            let admin_txpool_api = AdminTxPoolApiImpl::new(ctx.pool().clone());
            ctx.modules
                .merge_if_module_configured(RethRpcModule::Admin, admin_txpool_api.into_rpc())?;

            Ok(())
        })
    }
}

impl<E> FromExtensionConfig<E> for TxPoolRpcExtension
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    type Config = TxPoolRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
