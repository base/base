//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use std::sync::Arc;

pub use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
use base_execution_txpool::BuilderApiServer;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{
    NoopMeteringProvider, ShadowValidityBuilderApi, ShadowValidityConfig,
    ShadowValidityConfigError, SharedMeteringProvider,
};

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

    /// Pairs this validity config with a metering cache for insert.
    pub fn with_metering_provider(
        self,
        metering_provider: SharedMeteringProvider,
    ) -> BuilderApiExtensionArgs {
        BuilderApiExtensionArgs { config: self, metering_provider }
    }

    /// Pairs this validity config with a no-op metering cache.
    pub fn with_noop_metering(self) -> BuilderApiExtensionArgs {
        self.with_metering_provider(Arc::new(NoopMeteringProvider))
    }
}

impl Default for BuilderApiExtensionConfig {
    fn default() -> Self {
        Self::new(false, DEFAULT_MAX_VALIDITY_PREDICATES)
    }
}

/// Install arguments for [`BuilderApiExtension`].
#[derive(Clone)]
pub struct BuilderApiExtensionArgs {
    /// Validity-extension RPC settings.
    pub config: BuilderApiExtensionConfig,
    /// Shared builder metering cache written on `insertValidatedTransaction`.
    pub metering_provider: SharedMeteringProvider,
}

impl core::fmt::Debug for BuilderApiExtensionArgs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BuilderApiExtensionArgs")
            .field("config", &self.config)
            .field("metering_provider", &self.metering_provider)
            .finish()
    }
}

/// Extension that registers the Builder API RPC module (`base_insertValidatedTransaction`).
///
/// Its configuration controls validity metadata acceptance, predicate limits, and shadow-only
/// validity injection. Ordinary validated transactions remain available in all modes.
#[derive(Debug, Clone)]
pub struct BuilderApiExtension {
    config: BuilderApiExtensionConfig,
    metering_provider: SharedMeteringProvider,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;
        let metering_provider = self.metering_provider;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = ShadowValidityBuilderApi::new(
                ctx.pool().clone(),
                config,
                Arc::clone(&metering_provider),
            );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for BuilderApiExtension {
    type Config = BuilderApiExtensionArgs;

    fn from_config(args: Self::Config) -> Self {
        Self { config: args.config, metering_provider: args.metering_provider }
    }
}
