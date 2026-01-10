//! Contains the [`TransactionTracingExtension`] which wires up the `tracex`
//! execution extension on the Base node builder.

use base_client_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder, TracingConfig,
};

use crate::tracex_exex;

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
}

impl BaseNodeExtension for TransactionTracingExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let tracing = self.config;
        builder.install_exex_if(tracing.enabled, "tracex", move |ctx| async move {
            Ok(tracex_exex(ctx, tracing.logs_enabled))
        })
    }
}

/// Configuration trait for [`TransactionTracingExtension`].
///
/// Types implementing this trait can be used to construct a [`TransactionTracingExtension`].
pub trait TransactionTracingConfig {
    /// Returns the tracing configuration.
    fn tracing(&self) -> &TracingConfig;
}

impl<C: TransactionTracingConfig> ConfigurableBaseNodeExtension<C> for TransactionTracingExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(*config.tracing()))
    }
}
