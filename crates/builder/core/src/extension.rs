//! Builder API RPC extension for registering the `base_insertValidatedTransaction` endpoint.

use std::sync::Arc;

use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, InsertMetering, TransactionValidity,
};
pub use base_execution_txpool::{DEFAULT_MAX_VALIDITY_PREDICATES, ValiditySignatureMode};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{NoopMeteringProvider, SharedMeteringProvider};

/// Builder RPC configuration for validity-bearing transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderApiExtensionConfig {
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
    /// Signature rollout policy at both builder ingress endpoints.
    pub validity_signature_mode: ValiditySignatureMode,
}

impl BuilderApiExtensionConfig {
    /// Creates a builder RPC configuration.
    pub const fn new(max_validity_predicates: usize) -> Self {
        Self { max_validity_predicates, validity_signature_mode: ValiditySignatureMode::Off }
    }

    /// Sets the default-off staged validity-signature rollout policy.
    pub const fn with_validity_signature_mode(mut self, mode: ValiditySignatureMode) -> Self {
        self.validity_signature_mode = mode;
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
/// Its configuration controls predicate limits and validity-signature authorization.
/// Ordinary validated transactions remain available in all modes.
#[derive(Debug, Clone)]
pub struct BuilderApiExtension {
    config: BuilderApiExtensionConfig,
    metering_provider: SharedMeteringProvider,
}

impl BaseNodeExtension for BuilderApiExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;
        let metering_cache: Arc<dyn InsertMetering> = Arc::<Self>::from(self);
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let api = BuilderApiImpl::<_, TransactionValidity>::with_extensions(
                ctx.pool().clone(),
                true,
                config.max_validity_predicates,
            )
            .with_validity_signature_mode(config.validity_signature_mode)
            .with_metering_cache(Arc::clone(&metering_cache));
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

impl InsertMetering for BuilderApiExtension {
    fn insert_metering(&self, tx_hash: TxHash, metering: MeterBundleResponse) {
        self.metering_provider.insert(tx_hash, metering);
    }
}
