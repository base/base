use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{B256, Bytes};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_mev::{EthBundleHash, EthCancelBundle, EthSendBundle};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::Optimism;
use reth_rpc_eth_types::EthApiError;
use tips_audit::{MempoolEvent, MempoolEventPublisher};
use tips_datastore::BundleDatastore;
use tracing::{info, warn};

#[rpc(server, namespace = "eth")]
pub trait IngressApi {
    /// `eth_sendBundle` can be used to send your bundles to the builder.
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: EthSendBundle) -> RpcResult<EthBundleHash>;

    /// `eth_cancelBundle` is used to prevent a submitted bundle from being included on-chain.
    #[method(name = "cancelBundle")]
    async fn cancel_bundle(&self, request: EthCancelBundle) -> RpcResult<()>;

    /// Handler for: `eth_sendRawTransaction`
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;
}

pub struct IngressService<Store, Publisher> {
    provider: RootProvider<Optimism>,
    datastore: Store,
    dual_write_mempool: bool,
    publisher: Publisher,
}

impl<Store, Publisher> IngressService<Store, Publisher> {
    pub fn new(
        provider: RootProvider<Optimism>,
        datastore: Store,
        dual_write_mempool: bool,
        publisher: Publisher,
    ) -> Self {
        Self {
            provider,
            datastore,
            dual_write_mempool,
            publisher,
        }
    }
}

#[async_trait]
impl<Store, Publisher> IngressApiServer for IngressService<Store, Publisher>
where
    Store: BundleDatastore + Sync + Send + 'static,
    Publisher: MempoolEventPublisher + Sync + Send + 'static,
{
    async fn send_bundle(&self, _bundle: EthSendBundle) -> RpcResult<EthBundleHash> {
        warn!(
            message = "TODO: implement send_bundle",
            method = "send_bundle"
        );
        todo!("implement send_bundle")
    }

    async fn cancel_bundle(&self, _request: EthCancelBundle) -> RpcResult<()> {
        warn!(
            message = "TODO: implement cancel_bundle",
            method = "cancel_bundle"
        );
        todo!("implement cancel_bundle")
    }

    async fn send_raw_transaction(&self, data: Bytes) -> RpcResult<B256> {
        if data.is_empty() {
            return Err(EthApiError::EmptyRawTransactionData.into_rpc_err());
        }

        let envelope = OpTxEnvelope::decode_2718_exact(data.iter().as_slice())
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;

        let transaction = envelope
            .try_into_recovered()
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;

        // TODO: Validation and simulation
        // TODO: parallelize DB and mempool setup

        let bundle = EthSendBundle {
            txs: vec![data.clone()],
            block_number: 0,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![transaction.tx_hash()],
            ..Default::default()
        };

        let result = self
            .datastore
            .insert_bundle(bundle.clone())
            .await
            .map_err(|_e| ErrorObject::owned(11, "todo", Some(2)))?;

        info!(message="inserted singleton bundle", uuid=%result, txn_hash=%transaction.tx_hash());

        if let Err(e) = self
            .publisher
            .publish(MempoolEvent::Created {
                bundle_id: result,
                bundle,
            })
            .await
        {
            warn!(message = "Failed to publish MempoolEvent::Created", error = %e);
        }

        if self.dual_write_mempool {
            let response = self
                .provider
                .send_raw_transaction(data.iter().as_slice())
                .await;

            match response {
                Ok(_) => {
                    info!(message = "sent transaction to the mempool", hash=%transaction.tx_hash());
                }
                Err(e) => {
                    warn!(
                        message = "Failed to send raw transaction to mempool",
                        error = %e
                    );
                }
            }
        }

        Ok(transaction.tx_hash())
    }
}
