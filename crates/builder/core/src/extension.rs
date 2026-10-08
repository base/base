//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use std::sync::Arc;

use base_execution_txpool::BuilderApiServer;
pub use base_execution_txpool::{DEFAULT_MAX_VALIDITY_PREDICATES, ValiditySignatureMode};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{
    NoopMeteringProvider, ShadowValidityBuilderApi, ShadowValidityConfig, SharedMeteringProvider,
};

/// Builder RPC configuration for validity-bearing transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderApiExtensionConfig {
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
    /// Signature rollout policy at both builder ingress endpoints.
    pub validity_signature_mode: ValiditySignatureMode,
    /// Shadow-only validity injection configuration.
    pub shadow_validity: ShadowValidityConfig,
}

impl BuilderApiExtensionConfig {
    /// Creates a builder RPC configuration.
    pub const fn new(max_validity_predicates: usize) -> Self {
        Self {
            max_validity_predicates,
            validity_signature_mode: ValiditySignatureMode::Off,
            shadow_validity: ShadowValidityConfig::disabled(),
        }
    }

    /// Sets the default-off staged validity-signature rollout policy.
    pub const fn with_validity_signature_mode(mut self, mode: ValiditySignatureMode) -> Self {
        self.validity_signature_mode = mode;
        self
    }

    /// Enables the supplied shadow validity injection configuration.
    pub const fn with_shadow_validity(mut self, shadow_validity: ShadowValidityConfig) -> Self {
        self.shadow_validity = shadow_validity;
        self
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
        Self::new(DEFAULT_MAX_VALIDITY_PREDICATES)
    }
}

/// Install arguments for [`BuilderApiExtension`].
#[derive(Debug, Clone)]
pub struct BuilderApiExtensionArgs {
    /// Validity-extension RPC settings.
    pub config: BuilderApiExtensionConfig,
    /// Shared builder metering cache written on `insertValidatedTransaction`.
    pub metering_provider: SharedMeteringProvider,
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
