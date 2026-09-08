use core::fmt;
use std::collections::BTreeMap;

use alloy_consensus::Transaction;
use alloy_primitives::Address;
use alloy_rpc_types_txpool::{
    TxpoolContent, TxpoolContentFrom, TxpoolInspect, TxpoolInspectSummary, TxpoolStatus,
};
use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_execution_txpool::{AllPoolTransactions, PoolTransaction, TransactionPool};
use jsonrpsee::core::RpcResult;
use reth_rpc_api::TxPoolApiServer;
use tracing::trace;

/// `txpool` API implementation.
///
/// This type provides the functionality for handling `txpool` related requests.
#[derive(Clone)]
pub struct TxPoolApi<Pool, Eth> {
    /// An interface to interact with the pool
    pool: Pool,
    converter: reth_rpc_eth_types::BaseRpcConverter<Eth>,
}

impl<Pool, Eth> TxPoolApi<Pool, Eth> {
    /// Creates a new instance of `TxpoolApi`.
    pub const fn new(pool: Pool, converter: reth_rpc_eth_types::BaseRpcConverter<Eth>) -> Self {
        Self { pool, converter }
    }
}

impl<Pool, Eth> TxPoolApi<Pool, Eth>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>> + 'static,
    Eth: reth_storage_api::BlockReader<
            Block = base_common_consensus::BaseBlock,
            Transaction = base_common_consensus::BaseTxEnvelope,
            Receipt = base_common_consensus::BaseReceipt,
        > + base_execution_chainspec::ChainSpecProvider
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
{
    fn content(
        &self,
    ) -> Result<
        TxpoolContent<base_common_rpc_types::Transaction>,
        reth_rpc_eth_types::BaseEthApiError,
    > {
        #[inline]
        fn insert<Tx, RpcTxB>(
            tx: &Tx,
            content: &mut BTreeMap<Address, BTreeMap<String, base_common_rpc_types::Transaction>>,
            resp_builder: &reth_rpc_eth_types::BaseRpcConverter<RpcTxB>,
        ) -> Result<(), reth_rpc_eth_types::BaseEthApiError>
        where
            Tx: PoolTransaction<Consensus = BaseTxEnvelope>,
            RpcTxB: reth_storage_api::BlockReader<
                    Block = base_common_consensus::BaseBlock,
                    Transaction = base_common_consensus::BaseTxEnvelope,
                    Receipt = base_common_consensus::BaseReceipt,
                > + base_execution_chainspec::ChainSpecProvider
                + Clone
                + Send
                + Sync
                + Unpin
                + 'static,
        {
            content.entry(tx.sender()).or_default().insert(
                tx.nonce().to_string(),
                resp_builder.fill_pending(tx.clone_into_consensus())?,
            );

            Ok(())
        }

        let AllPoolTransactions { pending, queued } = self.pool.all_transactions();

        let mut content = TxpoolContent::default();
        for pending in pending {
            insert::<_, Eth>(&pending.transaction, &mut content.pending, &self.converter)?;
        }
        for queued in queued {
            insert::<_, Eth>(&queued.transaction, &mut content.queued, &self.converter)?;
        }

        Ok(content)
    }
}

#[async_trait]
impl<Pool, Eth> TxPoolApiServer<base_common_rpc_types::Transaction> for TxPoolApi<Pool, Eth>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>> + 'static,
    Eth: reth_storage_api::BlockReader<
            Block = base_common_consensus::BaseBlock,
            Transaction = base_common_consensus::BaseTxEnvelope,
            Receipt = base_common_consensus::BaseReceipt,
        > + base_execution_chainspec::ChainSpecProvider
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static + 'static,
{
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
        fn insert<T: PoolTransaction<Consensus = BaseTxEnvelope>>(
            tx: &T,
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
    ) -> RpcResult<TxpoolContentFrom<base_common_rpc_types::Transaction>> {
        trace!(target: "rpc::eth", ?from, "Serving txpool_contentFrom");
        Ok(self.content()?.remove_from(&from))
    }

    /// Returns the details of all transactions currently pending for inclusion in the next
    /// block(s), as well as the ones that are being scheduled for future execution only.
    ///
    /// See [here](https://geth.ethereum.org/docs/rpc/ns-txpool#txpool_content) for more details
    /// Handler for `txpool_content`
    async fn txpool_content(&self) -> RpcResult<TxpoolContent<base_common_rpc_types::Transaction>> {
        trace!(target: "rpc::eth", "Serving txpool_content");
        Ok(self.content()?)
    }
}

impl<Pool, Eth> fmt::Debug for TxPoolApi<Pool, Eth> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxpoolApi").finish_non_exhaustive()
    }
}
