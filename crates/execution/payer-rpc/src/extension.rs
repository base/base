//! Node extension that registers the payer RPC proxy.

use base_execution_rpc::SequencerClient;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{PayerApiImpl, PayerApiServer};

/// Upstream payer endpoint and whether `payer_getTerms` responses are cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerRpcConfig {
    /// Payer JSON-RPC URL. Callers pass the sequencer URL when the payer is served there.
    pub upstream_url: String,
    /// HTTP headers sent upstream, in `name=value` form.
    pub upstream_headers: Vec<String>,
    /// Cache successful `payer_getTerms` responses as one shared entry.
    ///
    /// Enable this only when the payer returns the same token catalog for every request.
    pub cache_get_terms: bool,
}

/// Registers `payer_getTerms` and `payer_sendTransaction` on the execution RPC.
#[derive(Debug)]
pub struct PayerRpcExtension {
    config: PayerRpcConfig,
}

impl BaseNodeExtension for PayerRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let config = self.config;
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let client = SequencerClient::new_http_with_headers(
                &config.upstream_url,
                config.upstream_headers.clone(),
            )?;
            let api = PayerApiImpl::new(ctx.provider().clone(), client, config.cache_get_terms);
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
