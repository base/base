//! Contains the [TransactionTracingExtension] which wires up the `tracex`
//! subscription on the Base node builder.

use base_tracex::start_tracex;

use crate::{
    BaseNodeConfig, TracingConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder},
};

/// Helper struct that wires the transaction tracing subscription into the node builder.
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
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let tracing = self.config;

        if !tracing.enabled {
            return builder;
        }

        builder.extend_rpc_modules(move |ctx| {
            start_tracex(ctx.pool().clone(), ctx.provider().clone(), tracing.logs_enabled);
            Ok(())
        })
    }
}

impl ConfigurableBaseNodeExtension for TransactionTracingExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        Ok(Self::new(config))
    }
}
