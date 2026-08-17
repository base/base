//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

pub use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
use base_execution_txpool::{BuilderApiImpl, BuilderApiServer, TransactionValidity};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

/// Builder RPC configuration for experimental validity-bearing transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderApiExtensionConfig {
    /// Whether the builder accepts non-empty experimental validity metadata.
    pub accept_experimental_validity_transactions: bool,
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
}

impl BuilderApiExtensionConfig {
    /// Creates a builder RPC configuration.
    pub const fn new(
        accept_experimental_validity_transactions: bool,
        max_validity_predicates: usize,
    ) -> Self {
        Self { accept_experimental_validity_transactions, max_validity_predicates }
    }
}

impl Default for BuilderApiExtensionConfig {
    fn default() -> Self {
        Self::new(false, DEFAULT_MAX_VALIDITY_PREDICATES)
    }
}

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
///
/// Its configuration controls whether non-empty experimental validity metadata
/// is accepted and how many predicates a transaction may carry. Ordinary
/// validated transactions remain available in both modes.
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    config: BuilderApiExtensionConfig,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let accept_validity = self.config.accept_experimental_validity_transactions;
        let max_validity_predicates = self.config.max_validity_predicates;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                ctx.pool().clone(),
                accept_validity,
                max_validity_predicates,
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = BuilderApiExtensionConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
