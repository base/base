//! `BaseNodeExtension` that registers the EIP-8130 `eth_getTransactionCount`,
//! `eth_estimateGas` and `eth_call` overrides on the node.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// JSON-RPC name of the method whose owner is configurable.
const GET_TRANSACTION_COUNT: &str = "eth_getTransactionCount";

/// Which extension serves `eth_getTransactionCount`.
///
/// `replace_configured` overwrites methods, so exactly one extension may register it. The owner
/// is chosen once from the node arguments and handed to every extension that cares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransactionCountOwner {
    /// [`Eip8130RpcExtension`] serves it (no archive configured).
    #[default]
    Eip8130,
    /// The archive-backed `eth_` override serves it and delegates non-zero `nonce_key` reads
    /// to the EIP-8130 implementation.
    Archive,
}

/// Wires the EIP-8130 `eth_getTransactionCount`, `eth_estimateGas` and `eth_call`
/// overrides into the node.
///
/// The overrides use reth's `TransportRpcModules::replace_configured`, which
/// overwrites all three methods, so no other extension may register them. When
/// [`TransactionCountOwner::Archive`] is configured, `eth_getTransactionCount` is left
/// to the archive override.
#[derive(Debug, Default)]
pub struct Eip8130RpcExtension {
    /// Owner of `eth_getTransactionCount`.
    pub transaction_count: TransactionCountOwner,
}

impl BaseNodeExtension for Eip8130RpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let transaction_count = self.transaction_count;
        hooks.add_rpc_module(move |ctx| {
            info!(message = "Starting EIP-8130 RPC override");
            let api_ext = Eip8130EthApiExt::new(ctx.registry.eth_api().clone());
            let mut module = api_ext.into_rpc();
            if transaction_count == TransactionCountOwner::Archive {
                module.remove_method(GET_TRANSACTION_COUNT);
            }
            ctx.modules.replace_configured(module)?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for Eip8130RpcExtension {
    type Config = TransactionCountOwner;

    fn from_config(transaction_count: TransactionCountOwner) -> Self {
        Self { transaction_count }
    }
}
