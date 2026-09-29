//! Admission of co-signed transactions as validity transactions.

use alloy_primitives::{Bytes, TxHash};
use base_common_chains::Upgrades;
use base_execution_txpool::ValidityPredicate;
use base_txpool_rpc::{
    SendRawTransactionValidityApiImpl, SendRawTransactionValidityApiServer,
    SendRawTransactionValidityOptions,
};
use jsonrpsee::core::{RpcResult, async_trait};
use reth_chainspec::ChainSpecProvider;
use reth_storage_api::BlockReaderIdExt;

/// Validity transaction ingress the payer admits co-signed transactions through.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ValidityIngress: Send + Sync + 'static {
    /// Returns the furthest block a submission made now may name as its
    /// block-number upper bound, or `None` when no canonical head exists.
    fn latest_block_expiry_bound(&self) -> RpcResult<Option<u64>>;

    /// Admits `transaction` guarded by `validity` and returns its hash.
    async fn submit(
        &self,
        transaction: Bytes,
        validity: Vec<ValidityPredicate>,
    ) -> RpcResult<TxHash>;
}

#[async_trait]
impl<Provider> ValidityIngress for SendRawTransactionValidityApiImpl<Provider>
where
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades> + 'static,
{
    fn latest_block_expiry_bound(&self) -> RpcResult<Option<u64>> {
        Self::latest_block_expiry_bound(self)
    }

    async fn submit(
        &self,
        transaction: Bytes,
        validity: Vec<ValidityPredicate>,
    ) -> RpcResult<TxHash> {
        self.send_raw_transaction_validity(
            transaction,
            SendRawTransactionValidityOptions { validity },
        )
        .await
    }
}
