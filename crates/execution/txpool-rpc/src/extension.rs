//! `TxPool` RPC extension for registering transaction pool management APIs.

use std::sync::Arc;

use base_execution_candidates::CandidateRuntime;
use base_execution_rpc::SequencerClient;
use base_execution_txpool::AdmissionService;
pub use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_EXPIRY_SECS, DEFAULT_MAX_VALIDITY_PREDICATES, ValiditySignatureMode,
};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use jsonrpsee::RpcModule;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_rpc_server_types::RethRpcModule;

use crate::{
    AdminCandidateSelectionApiImpl, AdminCandidateSelectionApiServer, AdminTxPoolApiImpl,
    AdminTxPoolApiServer, SendRawTransactionValidityApiImpl, SendRawTransactionValidityApiServer,
    SendSwapTransactionApiImpl, SendSwapTransactionApiServer, TransactionStatusApiImpl,
    TransactionStatusApiServer,
};

/// Configuration for the `TxPool` RPC extension.
#[derive(Debug, Clone, Default)]
pub struct TxPoolRpcConfig {
    /// Sequencer RPC endpoint for transaction status proxying.
    /// If None, queries the local transaction pool.
    pub sequencer_rpc: Option<String>,
}

/// Extension that registers the `TxPool` RPC modules (`AdminTxPoolApi` and `TransactionStatusApi`).
#[derive(Debug)]
pub struct TxPoolRpcExtension {
    config: TxPoolRpcConfig,
}

impl BaseNodeExtension for TxPoolRpcExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let sequencer_rpc = self.config.sequencer_rpc;

        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            // Register Base transaction pool APIs.
            let status_api = TransactionStatusApiImpl::new(sequencer_rpc, ctx.pool().clone())
                .expect("Failed to create transaction status API");
            ctx.modules.merge_configured(TransactionStatusApiServer::into_rpc(status_api))?;

            // Register AdminTxPoolApi
            let admin_txpool_api = AdminTxPoolApiImpl::new(ctx.pool().clone());
            ctx.modules
                .merge_if_module_configured(RethRpcModule::Admin, admin_txpool_api.into_rpc())?;

            Ok(())
        })
    }
}

/// Configuration for local validity-bearing transaction ingress.
#[derive(Debug, Clone)]
pub struct SendRawTransactionValidityConfig {
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
    /// Maximum validity-transaction lifetime, in seconds.
    pub max_validity_expiry_secs: u64,
    /// Staged sender-signature policy for validity predicates (default: off).
    pub validity_signature_mode: ValiditySignatureMode,
    /// Upstream sequencer RPC for query nodes that proxy rather than build transactions.
    pub sequencer_url: Option<String>,
    /// HTTP headers sent to the upstream sequencer, in `name=value` form.
    pub sequencer_headers: Vec<String>,
}

impl Default for SendRawTransactionValidityConfig {
    fn default() -> Self {
        Self {
            max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
            max_validity_expiry_secs: DEFAULT_MAX_VALIDITY_EXPIRY_SECS,
            validity_signature_mode: ValiditySignatureMode::Off,
            sequencer_url: None,
            sequencer_headers: Vec::new(),
        }
    }
}

/// Extension registering local validity-bearing transaction ingress.
#[derive(Debug, Default)]
pub struct SendRawTransactionValidityExtension {
    config: SendRawTransactionValidityConfig,
}

impl BaseNodeExtension for SendRawTransactionValidityExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let transaction_sender = ctx.registry.eth_api().eth_api().tx_batch_sender().clone();
            let mut api = SendRawTransactionValidityApiImpl::with_validity_limits(
                ctx.provider().clone(),
                config.max_validity_predicates,
                config.max_validity_expiry_secs,
                transaction_sender,
            )
            .with_validity_signature_mode(config.validity_signature_mode);
            if let Some(url) = &config.sequencer_url {
                api = api.with_sequencer_client(SequencerClient::new_http_with_headers(
                    url,
                    config.sequencer_headers.clone(),
                )?);
            }
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for SendRawTransactionValidityExtension {
    type Config = SendRawTransactionValidityConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

/// Whether a payload builder on this node evaluates candidate groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CandidateEvaluation {
    /// No builder here evaluates groups, so selection must stay off: a group admitted with
    /// selection on would be parked, and hold back its sender's later nonces, until it expires.
    #[default]
    IngressOnly,
    /// The native builder evaluates groups.
    Builder,
}

/// Configuration for `base_sendSwapTransaction`, the Swap Lane ingress.
///
/// Install the extension only when `--txpool.candidates.enabled` is set; an unregistered method
/// returns `-32601`. While selection is disabled at runtime, the registered method admits only
/// default transactions.
#[derive(Debug, Clone)]
pub struct SendSwapTransactionConfig {
    /// Candidate runtime shared with the pool and the payload builder.
    pub runtime: Arc<CandidateRuntime>,
    /// Lifetime of an admitted candidate group past the next block, in seconds.
    pub group_lifetime_secs: u64,
    /// Upstream node that proxy-mode ingress forwards every submission to, unchanged.
    pub sequencer_url: Option<String>,
    /// HTTP headers sent to the upstream sequencer, in `name=value` form.
    pub sequencer_headers: Vec<String>,
    /// Whether a builder here evaluates groups, which decides whether the selection toggle is
    /// registered.
    pub evaluation: CandidateEvaluation,
}

impl SendSwapTransactionConfig {
    /// Returns the `admin_setCandidateSelectionEnabled` module, or `None` on an ingress-only
    /// node, where the method stays unregistered so selection cannot be turned on.
    pub fn admin_module(&self) -> Option<RpcModule<AdminCandidateSelectionApiImpl>> {
        (self.evaluation == CandidateEvaluation::Builder)
            .then(|| AdminCandidateSelectionApiImpl::new(Arc::clone(&self.runtime)).into_rpc())
    }
}

/// Extension registering Swap Lane ingress and its candidate selection admin toggle.
#[derive(Debug)]
pub struct SendSwapTransactionExtension {
    config: SendSwapTransactionConfig,
}

impl BaseNodeExtension for SendSwapTransactionExtension {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        let config = self.config;
        builder.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            let chain_id = ctx.provider().chain_spec().chain_id();
            let admission = Arc::new(AdmissionService::new(
                ctx.provider().clone(),
                ctx.pool().clone(),
                Arc::clone(&config.runtime),
                config.group_lifetime_secs,
            ));
            let mut api = SendSwapTransactionApiImpl::new(admission, chain_id);
            if let Some(url) = &config.sequencer_url {
                api = api.with_sequencer_client(SequencerClient::new_http_with_headers(
                    url,
                    config.sequencer_headers.clone(),
                )?);
            }
            ctx.modules.merge_configured(api.into_rpc())?;
            if let Some(admin) = config.admin_module() {
                ctx.modules.merge_if_module_configured(RethRpcModule::Admin, admin)?;
            }
            Ok(())
        })
    }
}

impl FromExtensionConfig for SendSwapTransactionExtension {
    type Config = SendSwapTransactionConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

impl FromExtensionConfig for TxPoolRpcExtension {
    type Config = TxPoolRpcConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}

#[cfg(test)]
mod tests {
    use base_execution_candidates::{
        CandidateLimits, CandidateSwitch, GroupLedger, PrincipalsFile, StaticAccessPolicy,
    };

    use super::*;

    const TOGGLE: &str = "admin_setCandidateSelectionEnabled";

    fn config(evaluation: CandidateEvaluation) -> SendSwapTransactionConfig {
        let runtime = Arc::new(CandidateRuntime {
            ledger: GroupLedger::default(),
            policy: Arc::new(StaticAccessPolicy::new(PrincipalsFile {
                policy_version: 1,
                work_gas_quota: None,
                principals: vec![],
            })),
            switch: CandidateSwitch::default(),
            limits: CandidateLimits::default(),
        });
        SendSwapTransactionConfig {
            runtime,
            group_lifetime_secs: 60,
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            evaluation,
        }
    }

    #[test]
    fn ingress_only_nodes_cannot_enable_selection() {
        assert!(config(CandidateEvaluation::IngressOnly).admin_module().is_none());
    }

    #[test]
    fn builder_nodes_register_the_selection_toggle() {
        let admin = config(CandidateEvaluation::Builder).admin_module().expect("toggle registered");
        assert!(admin.method_names().any(|name| name == TOGGLE));
    }
}
