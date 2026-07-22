//! `BaseNodeExtension` that registers the `payer_*` RPC.

use base_execution_payer::{LocalPayerSigner, PayerCosigner, PayerDigestSigner};
use base_execution_payer_rpc::{PayerApiImpl, PayerApiServer};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use tracing::{info, warn};

use crate::terms::StateBackedPayerTerms;

/// Configuration for the payer RPC extension.
#[derive(Debug, Default)]
pub struct PayerRpcConfig {
    /// Whether the payer service is registered on this node.
    pub enabled: bool,
    /// The builder's payer co-signing key. When absent while [`Self::enabled`]
    /// is set, the extension logs and skips registration (a node cannot co-sign
    /// without the key).
    pub signer: Option<LocalPayerSigner>,
}

impl PayerRpcConfig {
    /// A disabled configuration.
    pub const fn disabled() -> Self {
        Self { enabled: false, signer: None }
    }
}

/// Registers the ERC-8168 `payer_*` RPC, backed by the node's transaction pool,
/// a [`StateBackedPayerTerms`] resolver, and the payer co-signer.
#[derive(Debug)]
pub struct PayerRpcExtension {
    config: PayerRpcConfig,
}

impl BaseNodeExtension for PayerRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.config.enabled {
            return hooks;
        }
        let Some(signer) = self.config.signer else {
            warn!("payer RPC enabled but no payer key configured; skipping registration");
            return hooks;
        };
        info!(payer = %signer.address(), "registering payer_* RPC");

        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let terms = StateBackedPayerTerms::new(ctx.provider().clone());
            let api = PayerApiImpl::new(ctx.pool().clone(), terms, PayerCosigner::new(signer));
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for PayerRpcExtension {
    type Config = PayerRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
