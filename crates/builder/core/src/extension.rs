//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use std::sync::Arc;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, MeteringResponseSink, SharedMeteringResponseSink,
};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::SharedMeteringProvider;

/// Adapts [`SharedMeteringProvider`] to the txpool [`MeteringResponseSink`].
#[derive(Debug)]
struct MeteringSinkAdapter(SharedMeteringProvider);

impl MeteringResponseSink for MeteringSinkAdapter {
    fn insert(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        self.0.insert(tx_hash, metering);
    }
}

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    metering: Option<SharedMeteringProvider>,
}

impl BuilderApiExtension {
    /// Creates an extension that also inserts metering into the builder store.
    pub fn with_metering(metering: SharedMeteringProvider) -> Self {
        Self { metering: Some(metering) }
    }
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let metering =
            self.metering.map(|m| Arc::new(MeteringSinkAdapter(m)) as SharedMeteringResponseSink);
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = metering.as_ref().map_or_else(
                || BuilderApiImpl::new(ctx.pool().clone()),
                |sink| BuilderApiImpl::with_metering(ctx.pool().clone(), Arc::clone(sink)),
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = Option<SharedMeteringProvider>;

    fn from_config(config: Self::Config) -> Self {
        Self { metering: config }
    }
}
