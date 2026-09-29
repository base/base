//! Node extension serving the `payer_*` namespace.

use base_execution_rpc::SequencerClient;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_txpool_rpc::SendRawTransactionValidityApiImpl;
use tracing::info;

use crate::{PayerApiServer, PayerProxy, PayerService, PayerSponsor};

/// How a node serves `payer_*`.
#[derive(Debug, Default)]
pub enum PayerExtensionConfig {
    /// The node does not serve `payer_*`.
    #[default]
    Disabled,
    /// The node co-signs and admits sponsored transactions to its own pool.
    Sponsor(Box<PayerSponsor>),
    /// The node forwards `payer_*` to its sequencer.
    Proxy {
        /// Sequencer RPC endpoint.
        sequencer_url: String,
        /// HTTP headers sent to the sequencer, in `name=value` form.
        sequencer_headers: Vec<String>,
    },
}

/// Extension registering the ERC-8168 `payer_*` namespace.
#[derive(Debug)]
pub struct PayerExtension {
    config: PayerExtensionConfig,
}

impl BaseNodeExtension for PayerExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        match self.config {
            PayerExtensionConfig::Disabled => hooks,
            PayerExtensionConfig::Sponsor(sponsor) => {
                hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
                    let transaction_sender =
                        ctx.registry.eth_api().eth_api().tx_batch_sender().clone();
                    let ingress = SendRawTransactionValidityApiImpl::with_validity_limits(
                        ctx.provider().clone(),
                        sponsor.max_validity_predicates,
                        sponsor.max_validity_expiry_secs,
                        transaction_sender,
                    )
                    .with_experimental_override(sponsor.experimental_override);
                    let payer = sponsor.config.terms.payer;
                    let tokens = sponsor.config.tokens.len();
                    let service = PayerService::new(
                        sponsor.config,
                        ctx.provider().clone(),
                        ingress,
                        sponsor.signer,
                    )?;
                    ctx.modules.merge_configured(service.into_rpc())?;
                    info!(payer = %payer, tokens, "serving ERC-8168 token payer");
                    Ok(())
                })
            }
            PayerExtensionConfig::Proxy { sequencer_url, sequencer_headers } => hooks
                .add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
                    let sequencer =
                        SequencerClient::new_http_with_headers(&sequencer_url, sequencer_headers)?;
                    ctx.modules.merge_configured(PayerProxy::new(sequencer).into_rpc())?;
                    Ok(())
                }),
        }
    }
}

impl FromExtensionConfig for PayerExtension {
    type Config = PayerExtensionConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
