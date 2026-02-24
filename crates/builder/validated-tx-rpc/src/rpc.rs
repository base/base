//! RPC trait and implementation for inserting pre-validated transactions.

use base_alloy_consensus::OpPooledTransaction;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use reth_optimism_txpool::OpPooledTx;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use tracing::{debug, warn};

use crate::{
    convert::compute_tx_hash,
    types::{InsertResult, ValidatedTransaction},
};

/// RPC trait for inserting pre-validated transactions into the pool.
#[rpc(server, namespace = "base")]
pub trait ValidatedTxApi {
    /// Inserts pre-validated transactions into the transaction pool.
    ///
    /// Transactions are expected to have been validated by the caller.
    /// The `from` field is trusted as the recovered sender address.
    ///
    /// Returns a result for each transaction indicating success or failure.
    #[method(name = "insertValidatedTransactions")]
    async fn insert_validated_transactions(
        &self,
        txs: Vec<ValidatedTransaction>,
    ) -> RpcResult<Vec<InsertResult>>;
}

/// Implementation of the validated transactions RPC.
#[derive(Debug)]
pub struct ValidatedTxApiImpl<Pool> {
    pool: Pool,
}

impl<Pool> ValidatedTxApiImpl<Pool> {
    /// Creates a new validated transactions API instance.
    pub const fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl<Pool> ValidatedTxApiServer for ValidatedTxApiImpl<Pool>
where
    Pool: TransactionPool + 'static,
    Pool::Transaction: OpPooledTx + PoolTransaction<Pooled = OpPooledTransaction>,
{
    async fn insert_validated_transactions(
        &self,
        txs: Vec<ValidatedTransaction>,
    ) -> RpcResult<Vec<InsertResult>> {
        let tx_count = txs.len();
        debug!(target: "rpc::validated_tx", count = tx_count, "inserting validated transactions");

        let mut results = Vec::with_capacity(tx_count);

        for validated_tx in txs {
            let tx_hash = match compute_tx_hash(&validated_tx) {
                Ok(hash) => hash,
                Err(e) => {
                    warn!(target: "rpc::validated_tx", error = %e, "failed to compute tx hash");
                    results.push(InsertResult::failure(Default::default(), e.to_string()));
                    continue;
                }
            };

            let recovered = match validated_tx.into_recovered() {
                Ok(r) => r,
                Err(e) => {
                    warn!(target: "rpc::validated_tx", %tx_hash, error = %e, "failed to convert transaction");
                    results.push(InsertResult::failure(tx_hash, e.to_string()));
                    continue;
                }
            };

            // Convert from Recovered<OpPooledTransaction> (alloy) to Pool::Transaction
            let pool_tx = Pool::Transaction::from_pooled(recovered);

            match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
                Ok(_outcome) => {
                    debug!(target: "rpc::validated_tx", %tx_hash, "transaction inserted");
                    results.push(InsertResult::success(tx_hash));
                }
                Err(e) => {
                    warn!(target: "rpc::validated_tx", %tx_hash, error = %e, "failed to insert transaction");
                    results.push(InsertResult::failure(tx_hash, e.to_string()));
                }
            }
        }

        let success_count = results.iter().filter(|r| r.success).count();
        let failure_count = results.len() - success_count;

        debug!(
            target: "rpc::validated_tx",
            total = tx_count,
            success = success_count,
            failures = failure_count,
            "completed inserting validated transactions"
        );

        if failure_count > 0 && success_count == 0 {
            return Err(ErrorObjectOwned::owned(
                -32000,
                format!("all {failure_count} transactions failed to insert"),
                Some(results),
            ));
        }

        Ok(results)
    }
}
