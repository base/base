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
use tips_audit::BundleEvent;
use tips_core::types::ParsedBundle;
use tips_core::{
    AcceptedBundle, Bundle, BundleExtensions, BundleHash, CancelBundle, MeterBundleResponse,
    user_ops_types::{SendUserOperationResponse, UserOperationRequest},
};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Duration, Instant, timeout};
use tracing::{info, warn};

use crate::metrics::{Metrics, record_histogram};
use crate::queue::QueuePublisher;
use crate::validation::validate_bundle;
use crate::{Config, TxSubmissionMethod};

#[rpc(server, namespace = "eth")]
pub trait IngressApi {
    /// `eth_sendBundle` can be used to send your bundles to the builder.
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash>;

    #[method(name = "sendBackrunBundle")]
    async fn send_backrun_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash>;

    /// `eth_cancelBundle` is used to prevent a submitted bundle from being included on-chain.
    #[method(name = "cancelBundle")]
    async fn cancel_bundle(&self, request: CancelBundle) -> RpcResult<()>;

    /// Handler for: `eth_sendRawTransaction`
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;

    /// Handler for: `eth_sendUserOperation`
    #[method(name = "sendUserOperation")]
    async fn send_user_operation(
        &self,
        user_operation: UserOperationRequest,
    ) -> RpcResult<SendUserOperationResponse>;
}

pub struct IngressService<Queue> {
    provider: RootProvider<Optimism>,
    simulation_provider: RootProvider<Optimism>,
    tx_submission_method: TxSubmissionMethod,
    bundle_queue: Queue,
    audit_channel: mpsc::UnboundedSender<BundleEvent>,
    send_transaction_default_lifetime_seconds: u64,
    metrics: Metrics,
    block_time_milliseconds: u64,
    meter_bundle_timeout_ms: u64,
    builder_tx: broadcast::Sender<MeterBundleResponse>,
    backrun_enabled: bool,
    builder_backrun_tx: broadcast::Sender<Bundle>,
}

impl<Queue> IngressService<Queue> {
    pub fn new(
        provider: RootProvider<Optimism>,
        simulation_provider: RootProvider<Optimism>,
        queue: Queue,
        audit_channel: mpsc::UnboundedSender<BundleEvent>,
        builder_tx: broadcast::Sender<MeterBundleResponse>,
        builder_backrun_tx: broadcast::Sender<Bundle>,
        config: Config,
    ) -> Self {
        Self {
            provider,
            simulation_provider,
            tx_submission_method: config.tx_submission_method,
            bundle_queue: queue,
            audit_channel,
            send_transaction_default_lifetime_seconds: config
                .send_transaction_default_lifetime_seconds,
            metrics: Metrics::default(),
            block_time_milliseconds: config.block_time_milliseconds,
            meter_bundle_timeout_ms: config.meter_bundle_timeout_ms,
            builder_tx,
            backrun_enabled: config.backrun_enabled,
            builder_backrun_tx,
        }
    }
}

#[async_trait]
impl<Queue> IngressApiServer for IngressService<Queue>
where
    Queue: QueuePublisher + Sync + Send + 'static,
{
    async fn send_backrun_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash> {
        if !self.backrun_enabled {
            info!(
                message = "Backrun bundle submission is disabled",
                backrun_enabled = self.backrun_enabled
            );
            return Err(
                EthApiError::InvalidParams("Backrun bundle submission is disabled".into())
                    .into_rpc_err(),
            );
        }

        let start = Instant::now();
        let (accepted_bundle, bundle_hash) = self.validate_parse_and_meter_bundle(&bundle).await?;

        self.metrics.backrun_bundles_received_total.increment(1);

        if let Err(e) = self.builder_backrun_tx.send(bundle) {
            warn!(
                message = "Failed to send backrun bundle to builders",
                bundle_hash = %bundle_hash,
                error = %e
            );
        }

        self.send_audit_event(&accepted_bundle, bundle_hash);

        self.metrics
            .backrun_bundles_sent_duration
            .record(start.elapsed().as_secs_f64());

        Ok(BundleHash { bundle_hash })
    }

    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash> {
        let (accepted_bundle, bundle_hash) = self.validate_parse_and_meter_bundle(&bundle).await?;

        // Get meter_bundle_response for builder broadcast
        let meter_bundle_response = accepted_bundle.meter_bundle_response.clone();

        // asynchronously send the meter bundle response to the builder
        self.builder_tx
            .send(meter_bundle_response)
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;

        // publish the bundle to the queue
        if let Err(e) = self
            .bundle_queue
            .publish(&accepted_bundle, &bundle_hash)
            .await
        {
            warn!(message = "Failed to publish bundle to queue", bundle_hash = %bundle_hash, error = %e);
            return Err(EthApiError::InvalidParams("Failed to queue bundle".into()).into_rpc_err());
        }

        info!(
            message = "queued bundle",
            bundle_hash = %bundle_hash,
        );

        // asynchronously send the audit event to the audit channel
        self.send_audit_event(&accepted_bundle, bundle_hash);

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
        let start = Instant::now();
        let transaction = self.get_tx(&data).await?;

        let send_to_kafka = matches!(
            self.tx_submission_method,
            TxSubmissionMethod::Kafka | TxSubmissionMethod::MempoolAndKafka
        );
        let send_to_mempool = matches!(
            self.tx_submission_method,
            TxSubmissionMethod::Mempool | TxSubmissionMethod::MempoolAndKafka
        );

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
        let parsed_bundle: ParsedBundle = bundle
            .clone()
            .try_into()
            .map_err(|e: String| EthApiError::InvalidParams(e).into_rpc_err())?;

        let bundle_hash = &parsed_bundle.bundle_hash();
        let meter_bundle_response = self.meter_bundle(&bundle, bundle_hash).await?;

        let accepted_bundle = AcceptedBundle::new(parsed_bundle, meter_bundle_response.clone());

        self.builder_tx
            .send(meter_bundle_response)
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;

        if send_to_kafka {
            if let Err(e) = self
                .bundle_queue
                .publish(&accepted_bundle, bundle_hash)
                .await
            {
                warn!(message = "Failed to publish Queue::enqueue_bundle", bundle_hash = %bundle_hash, error = %e);
            }

            info!(message="queued singleton bundle", txn_hash=%transaction.tx_hash());
        }

        if send_to_mempool {
            let response = self
                .provider
                .send_raw_transaction(data.iter().as_slice())
                .await;
            match response {
                Ok(_) => {
                    info!(message = "sent transaction to the mempool", hash=%transaction.tx_hash());
                }
                Err(e) => {
                    warn!(message = "Failed to send raw transaction to mempool", error = %e);
                }
            }
        }

        self.send_audit_event(&accepted_bundle, transaction.tx_hash());

        self.metrics
            .send_raw_transaction_duration
            .record(start.elapsed().as_secs_f64());

        Ok(transaction.tx_hash())
    }

    async fn send_user_operation(
        &self,
        user_operation: UserOperationRequest,
    ) -> RpcResult<SendUserOperationResponse> {
        dbg!(&user_operation);

        // STEPS:
        // 1. Reputation Service Validate
        // 2. Base Node Validate User Operation
        // 3. Send to Kafka
        // Send Hash
        // todo!("not yet implemented send_user_operation");
        todo!("not yet implemented send_user_operation");
    }
}

impl<Queue> IngressService<Queue>
where
    Queue: QueuePublisher + Sync + Send + 'static,
{
    async fn get_tx(&self, data: &Bytes) -> RpcResult<Recovered<OpTxEnvelope>> {
        if data.is_empty() {
            return Err(EthApiError::EmptyRawTransactionData.into_rpc_err());
        }

        let envelope = OpTxEnvelope::decode_2718_exact(data.iter().as_slice())
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;

        let transaction = envelope
            .clone()
            .try_into_recovered()
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;
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
            let transaction = self.get_tx(tx_data).await?;
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
    async fn meter_bundle(
        &self,
        bundle: &Bundle,
        bundle_hash: &B256,
    ) -> RpcResult<MeterBundleResponse> {
        let start = Instant::now();
        let timeout_duration = Duration::from_millis(self.meter_bundle_timeout_ms);

        // The future we await has the nested type:
        // Result<
        //   RpcResult<MeterBundleResponse>, // 1. The inner operation's result
        //   tokio::time::error::Elapsed     // 2. The outer timeout's result
        // >
        let res: MeterBundleResponse = timeout(
            timeout_duration,
            self.simulation_provider
                .client()
                .request("base_meterBundle", (bundle,)),
        )
        .await
        .map_err(|_| {
            warn!(message = "Timed out on requesting metering", bundle_hash = %bundle_hash);
            EthApiError::InvalidParams("Timeout on requesting metering".into()).into_rpc_err()
        })?
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

    /// Helper method to validate, parse, and meter a bundle
    async fn validate_parse_and_meter_bundle(
        &self,
        bundle: &Bundle,
    ) -> RpcResult<(AcceptedBundle, B256)> {
        self.validate_bundle(bundle).await?;
        let parsed_bundle: ParsedBundle = bundle
            .clone()
            .try_into()
            .map_err(|e: String| EthApiError::InvalidParams(e).into_rpc_err())?;
        let bundle_hash = parsed_bundle.bundle_hash();
        let meter_bundle_response = self.meter_bundle(bundle, &bundle_hash).await?;
        let accepted_bundle = AcceptedBundle::new(parsed_bundle, meter_bundle_response.clone());
        Ok((accepted_bundle, bundle_hash))
    }

    /// Helper method to send audit event for a bundle
    fn send_audit_event(&self, accepted_bundle: &AcceptedBundle, bundle_hash: B256) {
        let audit_event = BundleEvent::Received {
            bundle_id: *accepted_bundle.uuid(),
            bundle: Box::new(accepted_bundle.clone()),
        };
        if let Err(e) = self.audit_channel.send(audit_event) {
            warn!(
                message = "Failed to send audit event",
                bundle_hash = %bundle_hash,
                error = %e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tips_core::test_utils::create_test_meter_bundle_response;

    #[tokio::test]
    async fn test_timeout_logic() {
        let timeout_duration = Duration::from_millis(100);

        // Test a future that takes longer than the timeout
        let slow_future = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<MeterBundleResponse, anyhow::Error>(create_test_meter_bundle_response())
        };

        let result = timeout(timeout_duration, slow_future)
            .await
            .map_err(|_| {
                EthApiError::InvalidParams("Timeout on requesting metering".into()).into_rpc_err()
            })
            .map_err(|e| e.to_string());

        assert!(result.is_err());
        let error_string = format!("{:?}", result.unwrap_err());
        assert!(error_string.contains("Timeout on requesting metering"));
    }

    #[tokio::test]
    async fn test_timeout_logic_success() {
        let timeout_duration = Duration::from_millis(200);

        // Test a future that completes within the timeout
        let fast_future = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<MeterBundleResponse, anyhow::Error>(create_test_meter_bundle_response())
        };

        let result = timeout(timeout_duration, fast_future)
            .await
            .map_err(|_| {
                EthApiError::InvalidParams("Timeout on requesting metering".into()).into_rpc_err()
            })
            .map_err(|e| e.to_string());

        assert!(result.is_ok());
        // we're assumging that `base_meterBundle` will not error hence the second unwrap
        let res = result.unwrap().unwrap();
        assert_eq!(res, create_test_meter_bundle_response());
    }
}
