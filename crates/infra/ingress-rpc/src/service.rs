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
use tips_core::types::ParsedBundle;
use tips_core::{
    AcceptedBundle, Bundle, BundleExtensions, BundleHash, CancelBundle, MeterBundleResponse,
};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::metrics::{Metrics, record_histogram};
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
    simulation_provider: RootProvider<Optimism>,
    dual_write_mempool: bool,
    bundle_queue: Queue,
    audit_publisher: Audit,
    send_transaction_default_lifetime_seconds: u64,
    metrics: Metrics,
    block_time_milliseconds: u64,
}

impl<Queue, Audit> IngressService<Queue, Audit> {
    pub fn new(
        provider: RootProvider<Optimism>,
        simulation_provider: RootProvider<Optimism>,
        dual_write_mempool: bool,
        queue: Queue,
        audit_publisher: Audit,
        send_transaction_default_lifetime_seconds: u64,
        block_time_milliseconds: u64,
    ) -> Self {
        Self {
            provider,
            simulation_provider,
            dual_write_mempool,
            bundle_queue: queue,
            audit_publisher,
            send_transaction_default_lifetime_seconds,
            metrics: Metrics::default(),
            block_time_milliseconds,
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
        self.validate_bundle(&bundle).await?;
        let meter_bundle_response = self.meter_bundle(&bundle).await?;
        let parsed_bundle: ParsedBundle = bundle
            .try_into()
            .map_err(|e: String| EthApiError::InvalidParams(e).into_rpc_err())?;
        let accepted_bundle = AcceptedBundle::new(parsed_bundle, meter_bundle_response);

        let bundle_hash = &accepted_bundle.bundle_hash();
        if let Err(e) = self
            .bundle_queue
            .publish(&accepted_bundle, bundle_hash)
            .await
        {
            warn!(message = "Failed to publish bundle to queue", bundle_hash = %bundle_hash, error = %e);
            return Err(EthApiError::InvalidParams("Failed to queue bundle".into()).into_rpc_err());
        }

        info!(
            message = "queued bundle",
            bundle_hash = %bundle_hash,
        );

        let audit_event = BundleEvent::Received {
            bundle_id: *accepted_bundle.uuid(),
            bundle: Box::new(accepted_bundle.clone()),
        };
        if let Err(e) = self.audit_publisher.publish(audit_event).await {
            warn!(message = "Failed to publish audit event", bundle_id = %accepted_bundle.uuid(), error = %e);
        }

        Ok(BundleHash {
            bundle_hash: *bundle_hash,
        })
    }

    async fn cancel_bundle(&self, _request: CancelBundle) -> RpcResult<()> {
        warn!(
            message = "TODO: implement cancel_bundle",
            method = "cancel_bundle"
        );
        todo!("implement cancel_bundle")
    }

    async fn send_raw_transaction(&self, data: Bytes) -> RpcResult<B256> {
        let start = Instant::now();
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
        let meter_bundle_response = self.meter_bundle(&bundle).await?;

        let parsed_bundle: ParsedBundle = bundle
            .try_into()
            .map_err(|e: String| EthApiError::InvalidParams(e).into_rpc_err())?;
        let accepted_bundle = AcceptedBundle::new(parsed_bundle, meter_bundle_response);
        let bundle_hash = &accepted_bundle.bundle_hash();

        if let Err(e) = self
            .bundle_queue
            .publish(&accepted_bundle, bundle_hash)
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
            bundle_id: *accepted_bundle.uuid(),
            bundle: accepted_bundle.clone().into(),
        };
        if let Err(e) = self.audit_publisher.publish(audit_event).await {
            warn!(message = "Failed to publish audit event", bundle_id = %accepted_bundle.uuid(), error = %e);
        }

        self.metrics
            .send_raw_transaction_duration
            .record(start.elapsed().as_secs_f64());
        Ok(transaction.tx_hash())
    }
}

impl<Queue, Audit> IngressService<Queue, Audit>
where
    Queue: QueuePublisher + Sync + Send + 'static,
    Audit: BundleEventPublisher + Sync + Send + 'static,
{
    async fn validate_tx(&self, data: &Bytes) -> RpcResult<Recovered<OpTxEnvelope>> {
        let start = Instant::now();
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

        self.metrics
            .validate_tx_duration
            .record(start.elapsed().as_secs_f64());
        Ok(transaction)
    }

    async fn validate_bundle(&self, bundle: &Bundle) -> RpcResult<()> {
        let start = Instant::now();
        if bundle.txs.is_empty() {
            return Err(
                EthApiError::InvalidParams("Bundle cannot have empty transactions".into())
                    .into_rpc_err(),
            );
        }

        let mut total_gas = 0u64;
        let mut tx_hashes = Vec::new();
        for tx_data in &bundle.txs {
            let transaction = self.validate_tx(tx_data).await?;
            total_gas = total_gas.saturating_add(transaction.gas_limit());
            tx_hashes.push(transaction.tx_hash());
        }
        validate_bundle(bundle, total_gas, tx_hashes)?;

        self.metrics
            .validate_bundle_duration
            .record(start.elapsed().as_secs_f64());
        Ok(())
    }

    /// `meter_bundle` is used to determine how long a bundle will take to execute. A bundle that
    /// is within `block_time_milliseconds` will return the `MeterBundleResponse` that can be passed along
    /// to the builder.
    async fn meter_bundle(&self, bundle: &Bundle) -> RpcResult<MeterBundleResponse> {
        let start = Instant::now();
        let res: MeterBundleResponse = self
            .simulation_provider
            .client()
            .request("base_meterBundle", (bundle,))
            .await
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;
        record_histogram(start.elapsed(), "base_meterBundle".to_string());

        // we can save some builder payload building computation by not including bundles
        // that we know will take longer than the block time to execute
        let total_execution_time = (res.total_execution_time_us / 1_000) as u64;
        if total_execution_time > self.block_time_milliseconds {
            return Err(
                EthApiError::InvalidParams("Bundle simulation took too long".into()).into_rpc_err(),
            );
        }
        Ok(res)
    }
}
