use core::fmt;
use std::collections::BTreeMap;

use alloy_primitives::Address;
use async_trait::async_trait;
use base_common_types_chain::Transaction;
use base_common_types_rpc::{
    TxpoolContent, TxpoolContentFrom, TxpoolInspect, TxpoolInspectSummary, TxpoolStatus,
};
use base_execution_txpool::{AllPoolTransactions, TransactionPool};
use jsonrpsee::core::RpcResult;
use tracing::trace;

use crate::TxPoolApiServer;

/// `txpool` API implementation.
///
/// This type provides the functionality for handling `txpool` related requests.
#[derive(Clone)]
pub struct TxPoolApi {
    /// An interface to interact with the pool
    pool: base_execution_txpool::BaseTransactionPool,
    converter: crate::BaseRpcConverter,
}

impl TxPoolApi {
    /// Creates a new instance of `TxpoolApi`.
    pub const fn new(
        pool: base_execution_txpool::BaseTransactionPool,
        converter: crate::BaseRpcConverter,
    ) -> Self {
        Self { pool, converter }
    }
}

impl TxPoolApi {
    fn content(
        &self,
    ) -> Result<TxpoolContent<base_common_types_rpc::BaseTransaction>, crate::BaseEthApiError> {
        #[inline]
        fn insert(
            tx: &base_execution_txpool::BasePooledTransaction,
            content: &mut BTreeMap<
                Address,
                BTreeMap<String, base_common_types_rpc::BaseTransaction>,
            >,
            resp_builder: &crate::BaseRpcConverter,
        ) -> Result<(), crate::BaseEthApiError> {
            content.entry(tx.sender()).or_default().insert(
                tx.nonce().to_string(),
                resp_builder.fill_pending(tx.clone_into_consensus())?,
            );

            Ok(())
        }

        let AllPoolTransactions { pending, queued } = self.pool.all_transactions();

        let mut content = TxpoolContent::default();
        for pending in pending {
            insert(&pending.transaction, &mut content.pending, &self.converter)?;
        }
        for queued in queued {
            insert(&queued.transaction, &mut content.queued, &self.converter)?;
        }

        Ok(content)
    }
}

#[async_trait]
impl TxPoolApiServer for TxPoolApi {
    /// Returns the number of transactions currently pending for inclusion in the next block(s), as
    /// well as the ones that are being scheduled for future execution only.
    /// Ref: [Here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_status)
    ///
    /// Handler for `txpool_status`
    async fn txpool_status(&self) -> RpcResult<TxpoolStatus> {
        trace!(target: "rpc::eth", "Serving txpool_status");
        let (pending, queued) = self.pool.pending_and_queued_txn_count();
        Ok(TxpoolStatus { pending: pending as u64, queued: queued as u64 })
    }

    /// Returns a summary of all the transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_inspect) for more details
    ///
    /// Handler for `txpool_inspect`
    async fn txpool_inspect(&self) -> RpcResult<TxpoolInspect> {
        trace!(target: "rpc::eth", "Serving txpool_inspect");

        #[inline]
        fn insert(
            tx: &base_execution_txpool::BasePooledTransaction,
            inspect: &mut BTreeMap<Address, BTreeMap<String, TxpoolInspectSummary>>,
        ) {
            let entry = inspect.entry(tx.sender()).or_default();
            let tx = tx.clone_into_consensus();
            entry.insert(tx.nonce().to_string(), tx.into_inner().into());
        }

        let AllPoolTransactions { pending, queued } = self.pool.all_transactions();

        Ok(TxpoolInspect {
            pending: pending.iter().fold(Default::default(), |mut acc, tx| {
                insert(&tx.transaction, &mut acc);
                acc
            }),
            queued: queued.iter().fold(Default::default(), |mut acc, tx| {
                insert(&tx.transaction, &mut acc);
                acc
            }),
        })
    }

    /// Retrieves the transactions contained within the txpool, returning pending as well as queued
    /// transactions of this address, grouped by nonce.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_contentFrom) for more details
    /// Handler for `txpool_contentFrom`
    async fn txpool_content_from(
        &self,
        from: Address,
    ) -> RpcResult<TxpoolContentFrom<base_common_types_rpc::BaseTransaction>> {
        trace!(target: "rpc::eth", ?from, "Serving txpool_contentFrom");
        Ok(self.content()?.remove_from(&from))
    }

    /// Returns the details of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_content) for more details
    /// Handler for `txpool_content`
    async fn txpool_content(
        &self,
    ) -> RpcResult<TxpoolContent<base_common_types_rpc::BaseTransaction>> {
        trace!(target: "rpc::eth", "Serving txpool_content");
        Ok(self.content()?)
    }
}

impl fmt::Debug for TxPoolApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxpoolApi").finish_non_exhaustive()
    }
}

mod extensions;
pub use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
pub use extensions::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, SendRawTransactionValidityApiImpl,
    SendRawTransactionValidityApiServer, SendRawTransactionValidityOptions, Status,
    TransactionStatusApiImpl, TransactionStatusApiServer, TransactionStatusResponse,
    VALIDITY_TX_PRE_ZENITH_RPC_ERROR,
};

mod builder_config;
pub use builder_config::BuilderApiConfig;
mod shadow_validity;
pub use shadow_validity::{
    InjectionOutcome, MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS, ShadowValidityBuilderApi,
    ShadowValidityConfig, ShadowValidityConfigError,
};
mod metrics;
pub use metrics::ValidityMetrics;
