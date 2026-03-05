use std::time::Instant;

use alloy_consensus::transaction::Recovered;
use alloy_eips::Decodable2718;
use base_execution_primitives::OpTransactionSigned;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_transaction_pool::TransactionPool;
use tracing::debug;

use crate::{BasePooledTransaction, BuilderApiMetrics, ValidTransaction};

/// RPC interface for submitting pre-validated transactions to a block builder.
#[rpc(server, namespace = "base")]
pub trait BuilderApi {
    /// Inserts a single pre-validated transaction into the builder's pool.
    ///
    /// The transaction is EIP-2718 encoded with its sender address pre-recovered,
    /// allowing the builder to skip signature verification.
    ///
    /// Returns an error if decoding or pool insertion fails.
    /// Use JSON-RPC batch requests for efficient bulk submission.
    #[method(name = "insertValidatedTransaction")]
    async fn insert_validated_transaction(&self, tx: ValidTransaction) -> RpcResult<()>;
}

/// Server implementation of [`BuilderApi`] backed by a transaction pool.
///
/// This handler decodes incoming pre-validated transactions and inserts them
/// directly into the pool without re-validating signatures, since the sender
/// addresses are trusted from the forwarding mempool node.
#[derive(Debug)]
pub struct BuilderApiImpl<P> {
    pool: P,
    metrics: BuilderApiMetrics,
}

impl<P> BuilderApiImpl<P> {
    /// Creates a new handler backed by the given transaction pool.
    pub fn new(pool: P) -> Self {
        Self { pool, metrics: BuilderApiMetrics::default() }
    }
}

#[async_trait::async_trait]
impl<P> BuilderApiServer for BuilderApiImpl<P>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + 'static,
{
    async fn insert_validated_transaction(&self, tx: ValidTransaction) -> RpcResult<()> {
        debug!(
            sender = %tx.sender,
            "rpc::insert_validated_transaction"
        );
        let sender = tx.sender;

        // Decode the EIP-2718 transaction bytes
        let consensus_tx = OpTransactionSigned::decode_2718(&mut tx.raw.as_ref()).map_err(|e| {
            self.metrics.decode_errors.increment(1);
            ErrorObjectOwned::owned(
                ErrorCode::InvalidParams.code(),
                format!("failed to decode transaction: {e}"),
                None::<()>,
            )
        })?;
        let encoded_len = tx.raw.len();

        // Create recovered transaction without needing signer recovery
        let recovered = Recovered::new_unchecked(consensus_tx, sender);
        let pool_tx = BasePooledTransaction::new(recovered, encoded_len);

        // Insert into the pool
        let start = Instant::now();
        let result = self.pool.add_external_transaction(pool_tx).await;
        self.metrics.insert_duration.record(start.elapsed().as_secs_f64());

        match result {
            Ok(_) => {
                debug!(sender = %sender, "inserted validated transaction");
                self.metrics.txs_inserted.increment(1);
                Ok(())
            }
            Err(e) => {
                debug!(sender = %sender, error = %e, "pool rejected transaction");
                self.metrics.pool_rejections.increment(1);
                Err(ErrorObjectOwned::owned(
                    ErrorCode::InternalError.code(),
                    format!("pool rejected transaction: {e}"),
                    None::<()>,
                ))
            }
        }
    }
}
