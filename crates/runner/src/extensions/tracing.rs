//! Contains the [TransactionTracingExtension] which wires up the `tracex`
//! execution extension on the Base node builder.

use base_tracex::tracex_exex;

use crate::{
    BaseNodeConfig, TracingConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder},
};

/// Helper struct that wires the transaction tracing ExEx into the node builder.
#[derive(Debug, Clone, Copy)]
pub struct TransactionTracingExtension {
    /// Transaction tracing configuration flags.
    pub config: TracingConfig,
}

impl TransactionTracingExtension {
    /// Creates a new transaction tracing extension helper.
    pub const fn new(config: &BaseNodeConfig) -> Self {
        Self { config: config.tracing }
    }
}

impl BaseNodeExtension for TransactionTracingExtension {
    /// Applies the extension to the supplied builder.
    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let tracing = self.config;
        builder.install_exex_if(tracing.enabled, "tracex", move |ctx| async move {
            Ok(tracex_exex(ctx, tracing.logs_enabled))
        })
    }
}

impl ConfigurableBaseNodeExtension for TransactionTracingExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        Ok(Self::new(config))
    }
}
