//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use base_execution_txpool::{BuilderApiImpl, BuilderApiServer, TransactionValidity};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
///
/// Its boolean configuration controls whether non-empty experimental validity
/// metadata is accepted. Ordinary validated transactions remain available in
/// both modes.
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    accept_experimental_validity_transactions: bool,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let accept_validity = self.accept_experimental_validity_transactions;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                ctx.pool().clone(),
                accept_validity,
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = bool;

    fn from_config(accept_experimental_validity_transactions: Self::Config) -> Self {
        Self { accept_experimental_validity_transactions }
    }
}
