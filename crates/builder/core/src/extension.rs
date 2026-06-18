//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use base_execution_txpool::{BuilderApiImpl, BuilderApiServer};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_observability_events::TransactionEventWriter;

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    transaction_event_writer: Option<TransactionEventWriter>,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = BuilderApiImpl::new_with_transaction_event_writer(
                ctx.pool().clone(),
                self.transaction_event_writer.clone(),
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = Option<TransactionEventWriter>;

    fn from_config(config: Self::Config) -> Self {
        Self { transaction_event_writer: config }
    }
}
