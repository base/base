//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use base_execution_txpool::BuilderApiServer;
pub use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{ShadowValidityBuilderApi, ShadowValidityConfig, ShadowValidityConfigError};

/// Builder RPC configuration for experimental validity-bearing transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderApiExtensionConfig {
    /// Whether the builder accepts non-empty experimental validity metadata.
    pub accept_experimental_validity_transactions: bool,
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
    /// Shadow-only validity injection configuration.
    pub shadow_validity: ShadowValidityConfig,
}

impl BuilderApiExtensionConfig {
    /// Creates a builder RPC configuration.
    pub const fn new(
        accept_experimental_validity_transactions: bool,
        max_validity_predicates: usize,
    ) -> Self {
        Self {
            accept_experimental_validity_transactions,
            max_validity_predicates,
            shadow_validity: ShadowValidityConfig::disabled(),
        }
    }

    /// Enables the supplied shadow validity injection configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if injection is enabled while validity extensions are disabled.
    pub const fn with_shadow_validity(
        mut self,
        shadow_validity: ShadowValidityConfig,
    ) -> Result<Self, ShadowValidityConfigError> {
        if shadow_validity.is_enabled() && !self.accept_experimental_validity_transactions {
            return Err(ShadowValidityConfigError::ValidityTransactionsDisabled);
        }
        self.shadow_validity = shadow_validity;
        Ok(self)
    }
}

impl Default for BuilderApiExtensionConfig {
    fn default() -> Self {
        Self::new(false, DEFAULT_MAX_VALIDITY_PREDICATES)
    }
}

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
///
/// Its configuration controls validity metadata acceptance, predicate limits, and shadow-only
/// validity injection. Ordinary validated transactions remain available in all modes.
#[derive(Debug, Default)]
pub struct BuilderApiExtension {
    config: BuilderApiExtensionConfig,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = ShadowValidityBuilderApi::new(ctx.pool().clone(), config);
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
