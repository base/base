use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use super::task::ValidTransaction;

/// RPC interface for submitting pre-validated transactions to a block builder.
#[rpc(server, client, namespace = "base")]
pub trait BuilderApi {
    /// Inserts a batch of transactions with pre-recovered senders.
    #[method(name = "insertValidatedTransactions")]
    async fn insert_validated_transactions(&self, txs: Vec<ValidTransaction>) -> RpcResult<()>;
}
