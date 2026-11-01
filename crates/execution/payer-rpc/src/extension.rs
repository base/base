//! Node extension that registers the payer RPC proxy.

use std::time::Duration;

use base_execution_eip8130_rpc::Eip8130EthApiExt;
use base_execution_rpc::SequencerClient;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};

use crate::{Eip8130ChainState, PayerApiImpl, PayerApiServer};

/// Bound on one upstream payer HTTP call, including connect and the response body.
///
/// Longer than the 10-second backend timeout of a gateway such as proxyd, so a gateway in
/// front of the payer ends a slow call and its error reaches the wallet.
pub const PAYER_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);

/// Upstream payer endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerRpcConfig {
    /// Payer JSON-RPC URL. Callers pass the sequencer URL when the payer is served there.
    pub upstream_url: String,
    /// HTTP headers sent upstream, in `name=value` form.
    pub upstream_headers: Vec<String>,
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
            let client = SequencerClient::new_http_with_headers_and_timeout(
                &config.upstream_url,
                config.upstream_headers.clone(),
                PAYER_UPSTREAM_TIMEOUT,
            )?;
            let state =
                Eip8130ChainState::new(Eip8130EthApiExt::new(ctx.registry.eth_api().clone()));
            let api = PayerApiImpl::new(ctx.provider().clone(), client, state);
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
