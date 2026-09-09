use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::{B256, Bytes};
use base_common_consensus::{EthereumTxEnvelope, TxEip4844Variant};
use base_common_network::eip2718::Decodable2718;
use base_execution_rpc::{BaseEthApi, BaseEthApiError, RpcNodeCore};
use base_node_core::RpcRegistry;
use reth_rpc_api::DebugApiServer;

#[expect(missing_debug_implementations)]
pub struct RpcTestContext<EthApi: RpcNodeCore> {
    pub inner: RpcRegistry<EthApi>,
}

impl RpcTestContext<BaseEthApi<base_node_context::BaseNodeContext>> {
    /// Injects a raw transaction into the node tx pool via RPC server
    pub async fn inject_tx(&self, raw_tx: Bytes) -> Result<B256, BaseEthApiError> {
        let eth_api = self.inner.eth_api();
        eth_api.send_raw_transaction(raw_tx).await
    }

    /// Retrieves a transaction envelope by its hash
    pub async fn envelope_by_hash(
        &self,
        hash: B256,
    ) -> eyre::Result<EthereumTxEnvelope<TxEip4844Variant<BlobTransactionSidecarVariant>>> {
        let tx = self.inner.debug_api().raw_transaction(hash).await?.unwrap();
        let tx = tx.to_vec();
        Ok(EthereumTxEnvelope::decode_2718(&mut tx.as_ref()).unwrap())
    }
}
