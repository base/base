//! Fixed Base RPC handlers and their runtime inputs.

use std::sync::Arc;

use base_execution_payload_builder::SharedMeteringStore;
use base_execution_rpc_handlers::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, BuilderApiConfig, SendRawTransactionValidityApiImpl,
    SendRawTransactionValidityApiServer, ShadowValidityBuilderApi, TransactionStatusApiImpl,
    TransactionStatusApiServer,
};
use base_execution_txpool_pool::BuilderApiServer;
use base_metering::{
    BaseApiExtServer, MeteringApiImpl, MeteringApiServer, MeteringConfig, MeteringStoreExt,
};

use crate::RpcContext;

/// Runtime inputs for Base's built-in transaction and metering RPCs.
#[derive(Debug, Default)]
pub struct BaseRpcServices {
    /// Sequencer endpoint for transaction-status queries.
    pub sequencer: Option<String>,
    /// Sequencer ingress settings; present for a block-building node.
    pub builder: Option<BuilderApiConfig>,
    /// Maximum predicates when experimental transaction ingress is enabled.
    pub validity: Option<usize>,
    /// Bundle execution metering settings.
    pub metering: Option<MeteringConfig>,
    /// Shared resource metering store, when resource metering is enabled.
    pub metering_store: Option<SharedMeteringStore>,
}

impl BaseRpcServices {
    /// Registers Base's built-in handlers before the transports start.
    pub fn register(self, ctx: &mut RpcContext<'_>) -> eyre::Result<()> {
        let status = TransactionStatusApiImpl::new(self.sequencer, ctx.pool().clone())
            .map_err(|error| eyre::eyre!("failed to create transaction status API: {error}"))?;
        ctx.modules.merge_configured(status.into_rpc())?;
        ctx.modules.merge_configured(AdminTxPoolApiImpl::new(ctx.pool().clone()).into_rpc())?;
        if let Some(config) = self.builder {
            ctx.modules.merge_configured(
                ShadowValidityBuilderApi::new(ctx.pool().clone(), config).into_rpc(),
            )?;
        }
        if let Some(max_predicates) = self.validity {
            ctx.modules.merge_configured(
                SendRawTransactionValidityApiImpl::with_max_validity_predicates(
                    ctx.pool().clone(),
                    ctx.provider().clone(),
                    max_predicates,
                )
                .into_rpc(),
            )?;
        }
        if let Some(config) = self.metering.filter(|config| config.enabled) {
            ctx.modules.merge_configured(
                MeteringApiImpl::new(ctx.provider().clone(), Arc::new(config.metered_opcodes))
                    .into_rpc(),
            )?;
        }
        if let Some(store) = self.metering_store {
            ctx.modules.add_or_replace_configured(MeteringStoreExt::new(store).into_rpc())?;
        }
        Ok(())
    }
}
