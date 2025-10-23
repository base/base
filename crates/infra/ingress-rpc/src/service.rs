use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider, network::eip2718::Decodable2718};
use alloy_rpc_types_mev::{EthBundleHash, EthCancelBundle, EthSendBundle};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::Optimism;
use reth_rpc_eth_types::EthApiError;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::queue::QueuePublisher;
use crate::validation::{AccountInfoLookup, L1BlockInfoLookup, validate_bundle, validate_tx};

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

pub struct IngressService<Queue> {
    provider: RootProvider<Optimism>,
    dual_write_mempool: bool,
    queue: Queue,
    send_transaction_default_lifetime_seconds: u64,
}

impl<Queue> IngressService<Queue> {
    pub fn new(
        provider: RootProvider<Optimism>,
        dual_write_mempool: bool,
        queue: Queue,
        send_transaction_default_lifetime_seconds: u64,
    ) -> Self {
        Self {
            provider,
            dual_write_mempool,
            queue,
            send_transaction_default_lifetime_seconds,
        }
    }
}

#[async_trait]
impl<Queue> IngressApiServer for IngressService<Queue>
where
    Queue: QueuePublisher + Sync + Send + 'static,
{
    async fn send_bundle(&self, bundle: EthSendBundle) -> RpcResult<EthBundleHash> {
        self.validate_bundle(&bundle).await?;

        // Queue the bundle
        let bundle_hash = bundle.bundle_hash();
        if let Err(e) = self.queue.publish(&bundle, &bundle_hash).await {
            warn!(message = "Failed to publish bundle to queue", bundle_hash = %bundle_hash, error = %e);
            return Err(EthApiError::InvalidParams("Failed to queue bundle".into()).into_rpc_err());
        }

        info!(
            message = "queued bundle",
            bundle_hash = %bundle_hash,
            tx_count = bundle.txs.len(),
        );

        Ok(EthBundleHash {
            bundle_hash: bundle.bundle_hash(),
        })
    }

    async fn cancel_bundle(&self, _request: EthCancelBundle) -> RpcResult<()> {
        warn!(
            message = "TODO: implement cancel_bundle",
            method = "cancel_bundle"
        );
        todo!("implement cancel_bundle")
    }

    async fn send_raw_transaction(&self, data: Bytes) -> RpcResult<B256> {
        let transaction = self.validate_tx(&data).await?;

        let expiry_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + self.send_transaction_default_lifetime_seconds;

        let bundle = EthSendBundle {
            txs: vec![data.clone()],
            block_number: 0,
            min_timestamp: None,
            max_timestamp: Some(expiry_timestamp),
            reverting_tx_hashes: vec![transaction.tx_hash()],
            ..Default::default()
        };

        // queue the bundle
        let bundle_hash = bundle.bundle_hash();
        if let Err(e) = self.queue.publish(&bundle, &bundle_hash).await {
            warn!(message = "Failed to publish Queue::enqueue_bundle", bundle_hash = %bundle_hash, error = %e);
        }

        info!(message="queued singleton bundle", txn_hash=%transaction.tx_hash());

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

impl<Queue> IngressService<Queue>
where
    Queue: QueuePublisher + Sync + Send + 'static,
{
    async fn validate_tx(&self, data: &Bytes) -> RpcResult<Recovered<OpTxEnvelope>> {
        if data.is_empty() {
            return Err(EthApiError::EmptyRawTransactionData.into_rpc_err());
        }

        let envelope = OpTxEnvelope::decode_2718_exact(data.iter().as_slice())
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;

        let transaction = envelope
            .clone()
            .try_into_recovered()
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;

        let mut l1_block_info = self.provider.fetch_l1_block_info().await?;
        let account = self
            .provider
            .fetch_account_info(transaction.signer())
            .await?;
        validate_tx(account, &transaction, data, &mut l1_block_info).await?;

        Ok(transaction)
    }

    async fn validate_bundle(&self, bundle: &EthSendBundle) -> RpcResult<()> {
        if bundle.txs.is_empty() {
            return Err(
                EthApiError::InvalidParams("Bundle cannot have empty transactions".into())
                    .into_rpc_err(),
            );
        }

        let mut total_gas = 0u64;
        for tx_data in &bundle.txs {
            let transaction = self.validate_tx(tx_data).await?;
            total_gas = total_gas.saturating_add(transaction.gas_limit());
        }

        validate_bundle(bundle, total_gas)
    }
}
