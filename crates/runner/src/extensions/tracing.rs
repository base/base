//! Contains the [TransactionTracingExtension] which wires up the `tracex`
//! execution extension on the Base node builder.

use base_tracex::tracex_exex;

use crate::{TracingConfig, extensions::OpBuilder};

/// Helper struct that wires the transaction tracing ExEx into the node builder.
#[derive(Debug, Clone, Copy)]
pub struct TransactionTracingExtension {
    /// Transaction tracing configuration flags.
    pub config: TracingConfig,
}

impl TransactionTracingExtension {
    /// Creates a new transaction tracing extension helper.
    pub const fn new(config: TracingConfig) -> Self {
        Self { config }
    }

    /// Applies the extension to the supplied builder.
    pub fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let tracing = self.config;
        builder.install_exex_if(tracing.enabled, "tracex", move |ctx| async move {
            Ok(tracex_exex(ctx, tracing.logs_enabled))
        })
    }
}
