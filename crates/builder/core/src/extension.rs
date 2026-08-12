//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use std::sync::Arc;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, MeteringResponseSink, SharedMeteringResponseSink,
    TransactionValidity,
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
///
/// Its boolean configuration controls whether non-empty experimental validity
/// metadata is accepted. Ordinary validated transactions remain available in
/// both modes. Optional metering sinks inbound `meterBundle` responses into the
/// builder store.
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    accept_experimental_validity_transactions: bool,
    metering: Option<SharedMeteringProvider>,
}

impl BuilderApiExtension {
    /// Creates an extension with the given experimental-validity opt-in.
    pub const fn new(accept_experimental_validity_transactions: bool) -> Self {
        Self { accept_experimental_validity_transactions, metering: None }
    }

    /// Also inserts inbound metering responses into the builder store.
    pub fn with_metering(mut self, metering: SharedMeteringProvider) -> Self {
        self.metering = Some(metering);
        self
    }
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let accept_validity = self.accept_experimental_validity_transactions;
        let metering =
            self.metering.map(|m| Arc::new(MeteringSinkAdapter(m)) as SharedMeteringResponseSink);
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = metering.as_ref().map_or_else(
                || {
                    BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                        ctx.pool().clone(),
                        accept_validity,
                    )
                },
                |sink| {
                    BuilderApiImpl::<_, TransactionValidity>::with_extensions_and_metering(
                        ctx.pool().clone(),
                        accept_validity,
                        Arc::clone(sink),
                    )
                },
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = Self;

    fn from_config(config: Self::Config) -> Self {
        config
    }
}
