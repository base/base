//! `TxPool` RPC extension for registering transaction pool management APIs.

use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use reth_rpc_server_types::RethRpcModule;

use crate::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, SendRawTransactionValidityApiImpl,
    SendRawTransactionValidityApiServer, TransactionStatusApiImpl, TransactionStatusApiServer,
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

impl BaseNodeExtension for TxPoolRpcExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let sequencer_rpc = self.config.sequencer_rpc;

        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            // Register Base transaction pool APIs.
            let status_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status API");
            ctx.modules.merge_configured(TransactionStatusApiServer::into_rpc(status_api))?;

            // Register AdminTxPoolApi
            let admin_txpool_api = AdminTxPoolApiImpl::new(ctx.pool().clone());
            ctx.modules
                .merge_if_module_configured(RethRpcModule::Admin, admin_txpool_api.into_rpc())?;

            Ok(())
        })
    }
}

/// Extension registering local validity-bearing transaction ingress.
#[derive(Debug, Default)]
pub struct SendRawTransactionValidityExtension;

impl BaseNodeExtension for SendRawTransactionValidityExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        builder.add_rpc_module(|ctx: &mut BaseRpcContext<'_>| {
            let api = SendRawTransactionValidityApiImpl::new(ctx.pool().clone());
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for SendRawTransactionValidityExtension {
    type Config = ();

    fn from_config(_config: Self::Config) -> Self {
        Self
    }
}

impl FromExtensionConfig for TxPoolRpcExtension {
    type Config = TxPoolRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
