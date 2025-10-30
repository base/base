use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider, network::eip2718::Decodable2718};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::Optimism;
use reth_rpc_eth_types::EthApiError;
use std::time::{SystemTime, UNIX_EPOCH};
use tips_audit::{BundleEvent, BundleEventPublisher};
use tips_core::{Bundle, BundleHash, BundleWithMetadata, CancelBundle};
use tracing::{info, warn};

use crate::queue::QueuePublisher;
use crate::validation::{AccountInfoLookup, L1BlockInfoLookup, validate_bundle, validate_tx};

#[rpc(server, namespace = "eth")]
pub trait IngressApi {
    /// `eth_sendBundle` can be used to send your bundles to the builder.
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash>;

    /// `eth_cancelBundle` is used to prevent a submitted bundle from being included on-chain.
    #[method(name = "cancelBundle")]
    async fn cancel_bundle(&self, request: CancelBundle) -> RpcResult<()>;

    /// Handler for: `eth_sendRawTransaction`
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;
}

pub struct IngressService<Queue, Audit> {
    provider: RootProvider<Optimism>,
    dual_write_mempool: bool,
    bundle_queue: Queue,
    audit_publisher: Audit,
    send_transaction_default_lifetime_seconds: u64,
}

impl<Queue, Audit> IngressService<Queue, Audit> {
    pub fn new(
        provider: RootProvider<Optimism>,
        dual_write_mempool: bool,
        queue: Queue,
        audit_publisher: Audit,
        send_transaction_default_lifetime_seconds: u64,
    ) -> Self {
        Self {
            provider,
            dual_write_mempool,
            bundle_queue: queue,
            audit_publisher,
            send_transaction_default_lifetime_seconds,
        }
    }
}

#[async_trait]
impl<Queue, Audit> IngressApiServer for IngressService<Queue, Audit>
where
    Queue: QueuePublisher + Sync + Send + 'static,
    Audit: BundleEventPublisher + Sync + Send + 'static,
{
    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash> {
        let bundle_with_metadata = self.validate_bundle(bundle).await?;

        let bundle_hash = bundle_with_metadata.bundle_hash();
        if let Err(e) = self
            .bundle_queue
            .publish(&bundle_with_metadata, &bundle_hash)
            .await
        {
            warn!(message = "Failed to publish bundle to queue", bundle_hash = %bundle_hash, error = %e);
            return Err(EthApiError::InvalidParams("Failed to queue bundle".into()).into_rpc_err());
        }

        info!(
            message = "queued bundle",
            bundle_hash = %bundle_hash,
            tx_count = bundle_with_metadata.transactions().len(),
        );

        let audit_event = BundleEvent::Received {
            bundle_id: *bundle_with_metadata.uuid(),
            bundle: bundle_with_metadata.bundle().clone(),
        };
        if let Err(e) = self.audit_publisher.publish(audit_event).await {
            warn!(message = "Failed to publish audit event", bundle_id = %bundle_with_metadata.uuid(), error = %e);
        }

        Ok(BundleHash { bundle_hash })
    }

    async fn cancel_bundle(&self, _request: CancelBundle) -> RpcResult<()> {
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

        let bundle = Bundle {
            txs: vec![data.clone()],
            max_timestamp: Some(expiry_timestamp),
            reverting_tx_hashes: vec![transaction.tx_hash()],
            ..Default::default()
        };

        let bundle_with_metadata = BundleWithMetadata::load(bundle)
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;
        let bundle_hash = bundle_with_metadata.bundle_hash();

        if let Err(e) = self
            .bundle_queue
            .publish(&bundle_with_metadata, &bundle_hash)
            .await
        {
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

        let audit_event = BundleEvent::Received {
            bundle_id: *bundle_with_metadata.uuid(),
            bundle: bundle_with_metadata.bundle().clone(),
        };
        if let Err(e) = self.audit_publisher.publish(audit_event).await {
            warn!(message = "Failed to publish audit event", bundle_id = %bundle_with_metadata.uuid(), error = %e);
        }

        Ok(transaction.tx_hash())
    }
}

impl<Queue, Audit> IngressService<Queue, Audit>
where
    Queue: QueuePublisher + Sync + Send + 'static,
    Audit: BundleEventPublisher + Sync + Send + 'static,
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

    async fn validate_bundle(&self, bundle: Bundle) -> RpcResult<BundleWithMetadata> {
        if bundle.txs.is_empty() {
            return Err(
                EthApiError::InvalidParams("Bundle cannot have empty transactions".into())
                    .into_rpc_err(),
            );
        }

        let bundle_with_metadata = BundleWithMetadata::load(bundle.clone())
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;
        let tx_hashes = bundle_with_metadata.txn_hashes();

        let mut total_gas = 0u64;
        for tx_data in &bundle.txs {
            let transaction = self.validate_tx(tx_data).await?;
            total_gas = total_gas.saturating_add(transaction.gas_limit());
        }
        validate_bundle(&bundle, total_gas, tx_hashes)?;

        Ok(bundle_with_metadata)
    }
}
