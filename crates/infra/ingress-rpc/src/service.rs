use account_abstraction_core::domain::ReputationService;
use account_abstraction_core::infrastructure::base_node::validator::BaseNodeValidator;
use account_abstraction_core::services::ReputationServiceImpl;
use account_abstraction_core::services::interfaces::user_op_validator::UserOperationValidator;
use account_abstraction_core::{Mempool, MempoolEngine};
use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, Bytes, FixedBytes};
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
};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Duration, Instant, timeout};
use tracing::{debug, info, warn};

use crate::metrics::{Metrics, record_histogram};
use crate::queue::{BundleQueuePublisher, MessageQueue, UserOpQueuePublisher};
use crate::validation::validate_bundle;
use crate::{Config, TxSubmissionMethod};
use account_abstraction_core::domain::entrypoints::version::EntryPointVersion;
use account_abstraction_core::domain::types::{UserOperationRequest, VersionedUserOperation};
use std::sync::Arc;

/// RPC providers for different endpoints
pub struct Providers {
    pub mempool: RootProvider<Optimism>,
    pub simulation: RootProvider<Optimism>,
    pub raw_tx_forward: Option<RootProvider<Optimism>>,
}

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
        user_operation: VersionedUserOperation,
        entry_point: Address,
    ) -> RpcResult<FixedBytes<32>>;
}

pub struct IngressService<Q: MessageQueue, M: Mempool> {
    mempool_provider: Arc<RootProvider<Optimism>>,
    simulation_provider: Arc<RootProvider<Optimism>>,
    raw_tx_forward_provider: Option<Arc<RootProvider<Optimism>>>,
    user_op_validator: BaseNodeValidator,
    tx_submission_method: TxSubmissionMethod,
    bundle_queue_publisher: BundleQueuePublisher<Q>,
    user_op_queue_publisher: UserOpQueuePublisher<Q>,
    reputation_service: Arc<ReputationServiceImpl<M>>,
    audit_channel: mpsc::UnboundedSender<BundleEvent>,
    send_transaction_default_lifetime_seconds: u64,
    metrics: Metrics,
    block_time_milliseconds: u64,
    meter_bundle_timeout_ms: u64,
    builder_tx: broadcast::Sender<MeterBundleResponse>,
    backrun_enabled: bool,
    builder_backrun_tx: broadcast::Sender<AcceptedBundle>,
    max_backrun_txs: usize,
    max_backrun_gas_limit: u64,
}

impl<Q: MessageQueue, M: Mempool> IngressService<Q, M> {
    pub fn new(
        providers: Providers,
        queue: Q,
        audit_channel: mpsc::UnboundedSender<BundleEvent>,
        builder_tx: broadcast::Sender<MeterBundleResponse>,
        builder_backrun_tx: broadcast::Sender<AcceptedBundle>,
        mempool_engine: Arc<MempoolEngine<M>>,
        config: Config,
    ) -> Self {
        let mempool_provider = Arc::new(providers.mempool);
        let simulation_provider = Arc::new(providers.simulation);
        let raw_tx_forward_provider = providers.raw_tx_forward.map(Arc::new);
        let user_op_validator = BaseNodeValidator::new(
            simulation_provider.clone(),
            config.validate_user_operation_timeout_ms,
        );
        let queue_connection = Arc::new(queue);
        let reputation_service = ReputationServiceImpl::new(mempool_engine.get_mempool());
        Self {
            mempool_provider,
            simulation_provider,
            raw_tx_forward_provider,
            user_op_validator,
            tx_submission_method: config.tx_submission_method,
            user_op_queue_publisher: UserOpQueuePublisher::new(
                queue_connection.clone(),
                config.user_operation_topic,
            ),
            bundle_queue_publisher: BundleQueuePublisher::new(
                queue_connection.clone(),
                config.ingress_topic,
            ),
            reputation_service: Arc::new(reputation_service),
            audit_channel,
            send_transaction_default_lifetime_seconds: config
                .send_transaction_default_lifetime_seconds,
            metrics: Metrics::default(),
            block_time_milliseconds: config.block_time_milliseconds,
            meter_bundle_timeout_ms: config.meter_bundle_timeout_ms,
            builder_tx,
            backrun_enabled: config.backrun_enabled,
            builder_backrun_tx,
            max_backrun_txs: config.max_backrun_txs,
            max_backrun_gas_limit: config.max_backrun_gas_limit,
        }
    }
}

fn validate_backrun_bundle_limits(
    txs_count: usize,
    total_gas_limit: u64,
    max_backrun_txs: usize,
    max_backrun_gas_limit: u64,
) -> Result<(), String> {
    if txs_count < 2 {
        return Err(
            "Backrun bundle must have at least 2 transactions (target + backrun)".to_string(),
        );
    }
    if txs_count > max_backrun_txs {
        return Err(format!(
            "Backrun bundle exceeds max transaction count: {txs_count} > {max_backrun_txs}",
        ));
    }
    if total_gas_limit > max_backrun_gas_limit {
        return Err(format!(
            "Backrun bundle exceeds max gas limit: {total_gas_limit} > {max_backrun_gas_limit}",
        ));
    }
    Ok(())
}

#[async_trait]
impl<Q: MessageQueue + 'static, M: Mempool + 'static> IngressApiServer for IngressService<Q, M> {
    async fn send_backrun_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash> {
        if !self.backrun_enabled {
            return Err(
                EthApiError::InvalidParams("Backrun bundle submission is disabled".into())
                    .into_rpc_err(),
            );
        }

        let start = Instant::now();
        let (accepted_bundle, bundle_hash) =
            self.validate_parse_and_meter_bundle(&bundle, false).await?;

        let total_gas_limit: u64 = accepted_bundle.txs.iter().map(|tx| tx.gas_limit()).sum();
        validate_backrun_bundle_limits(
            accepted_bundle.txs.len(),
            total_gas_limit,
            self.max_backrun_txs,
            self.max_backrun_gas_limit,
        )
        .map_err(|e| EthApiError::InvalidParams(e).into_rpc_err())?;

        self.metrics.backrun_bundles_received_total.increment(1);

        self.builder_backrun_tx
            .send(accepted_bundle.clone())
            .map_err(|e| {
                EthApiError::InvalidParams(format!("Failed to send backrun bundle: {e}"))
                    .into_rpc_err()
            })?;

        self.send_audit_event(&accepted_bundle, bundle_hash);

        self.metrics
            .backrun_bundles_sent_duration
            .record(start.elapsed().as_secs_f64());

        Ok(BundleHash { bundle_hash })
    }

    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash> {
        let (accepted_bundle, bundle_hash) =
            self.validate_parse_and_meter_bundle(&bundle, true).await?;

        // Get meter_bundle_response for builder broadcast
        let meter_bundle_response = accepted_bundle.meter_bundle_response.clone();

        // asynchronously send the meter bundle response to the builder
        self.builder_tx
            .send(meter_bundle_response)
            .map_err(|e| EthApiError::InvalidParams(e.to_string()).into_rpc_err())?;

        // publish the bundle to the queue
        if let Err(e) = self
            .bundle_queue_publisher
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

        self.metrics.transactions_received.increment(1);

        let send_to_kafka = matches!(
            self.tx_submission_method,
            TxSubmissionMethod::Kafka | TxSubmissionMethod::MempoolAndKafka
        );
        let send_to_mempool = matches!(
            self.tx_submission_method,
            TxSubmissionMethod::Mempool | TxSubmissionMethod::MempoolAndKafka
        );

        // Forward before metering
        if let Some(forward_provider) = self.raw_tx_forward_provider.clone() {
            self.metrics.raw_tx_forwards_total.increment(1);
            let tx_data = data.clone();
            let tx_hash = transaction.tx_hash();
            tokio::spawn(async move {
                match forward_provider
                    .send_raw_transaction(tx_data.iter().as_slice())
                    .await
                {
                    Ok(_) => {
                        debug!(message = "Forwarded raw tx", hash = %tx_hash);
                    }
                    Err(e) => {
                        warn!(message = "Failed to forward raw tx", hash = %tx_hash, error = %e);
                    }
                }
            });
        }

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

        self.metrics.bundles_parsed.increment(1);

        let meter_bundle_response = self.meter_bundle(&bundle, bundle_hash).await.ok();

        if let Some(meter_info) = meter_bundle_response.as_ref() {
            self.metrics.successful_simulations.increment(1);
            _ = self.builder_tx.send(meter_info.clone());
        } else {
            self.metrics.failed_simulations.increment(1);
        }

        let accepted_bundle =
            AcceptedBundle::new(parsed_bundle, meter_bundle_response.unwrap_or_default());

        if send_to_kafka {
            if let Err(e) = self
                .bundle_queue_publisher
                .publish(&accepted_bundle, bundle_hash)
                .await
            {
                warn!(message = "Failed to publish Queue::enqueue_bundle", bundle_hash = %bundle_hash, error = %e);
            }

            self.metrics.sent_to_kafka.increment(1);
            info!(message="queued singleton bundle", txn_hash=%transaction.tx_hash());
        }

        if send_to_mempool {
            let response = self
                .mempool_provider
                .send_raw_transaction(data.iter().as_slice())
                .await;
            match response {
                Ok(_) => {
                    self.metrics.sent_to_mempool.increment(1);
                    debug!(message = "sent transaction to the mempool", hash=%transaction.tx_hash());
                }
                Err(e) => {
                    warn!(message = "Failed to send raw transaction to mempool", error = %e);
                }
            }
        }

        info!(
            message = "processed transaction",
            bundle_hash = %bundle_hash,
            transaction_hash = %transaction.tx_hash(),
        );

        self.send_audit_event(&accepted_bundle, accepted_bundle.bundle_hash());

        self.metrics
            .send_raw_transaction_duration
            .record(start.elapsed().as_secs_f64());

        Ok(transaction.tx_hash())
    }

    async fn send_user_operation(
        &self,
        rpc_user_operation: VersionedUserOperation,
        entry_point: Address,
    ) -> RpcResult<FixedBytes<32>> {
        let entry_point_version = EntryPointVersion::try_from(entry_point).map_err(|_| {
            EthApiError::InvalidParams("Unknown entry point version".into()).into_rpc_err()
        })?;

        let versioned_user_operation = match (rpc_user_operation, entry_point_version) {
            (VersionedUserOperation::UserOperation(op), EntryPointVersion::V06) => {
                VersionedUserOperation::UserOperation(op)
            }
            (VersionedUserOperation::PackedUserOperation(op), EntryPointVersion::V07) => {
                VersionedUserOperation::PackedUserOperation(op)
            }
            _ => {
                return Err(EthApiError::InvalidParams(
                    "User operation type does not match entry point version".into(),
                )
                .into_rpc_err());
            }
        };

        let request = UserOperationRequest {
            user_operation: versioned_user_operation,
            entry_point,
            chain_id: 1,
        };

        // DO Nothing with reputation at the moment as this is scafolding
        let _ = self
            .reputation_service
            .get_reputation(&request.user_operation.sender())
            .await;

        let user_op_hash = request.hash().map_err(|e| {
            warn!(message = "Failed to hash user operation", error = %e);
            EthApiError::InvalidParams(e.to_string()).into_rpc_err()
        })?;

        let _ = self
            .user_op_validator
            .validate_user_operation(&request.user_operation, &entry_point)
            .await
            .map_err(|e| {
                warn!(message = "Failed to validate user operation", error = %e);
                EthApiError::InvalidParams(e.to_string()).into_rpc_err()
            })?;

        if let Err(e) = self
            .user_op_queue_publisher
            .publish(&request.user_operation, &user_op_hash)
            .await
        {
            warn!(
                message = "Failed to publish user operation to queue",
                user_operation_hash = %user_op_hash,
                error = %e
            );
            return Err(
                EthApiError::InvalidParams("Failed to queue user operation".into()).into_rpc_err(),
            );
        }

        Ok(user_op_hash)
    }
}

impl<Q: MessageQueue, M: Mempool> IngressService<Q, M> {
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
        to_meter: bool,
    ) -> RpcResult<(AcceptedBundle, B256)> {
        self.validate_bundle(bundle).await?;
        let parsed_bundle: ParsedBundle = bundle
            .clone()
            .try_into()
            .map_err(|e: String| EthApiError::InvalidParams(e).into_rpc_err())?;
        let bundle_hash = parsed_bundle.bundle_hash();
        let meter_bundle_response = if to_meter {
            self.meter_bundle(bundle, &bundle_hash).await?
        } else {
            MeterBundleResponse::default()
        };
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
                message = "failed to send audit event",
                bundle_hash = %bundle_hash,
                error = %e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, TxSubmissionMethod, queue::MessageQueue};
    use account_abstraction_core::MempoolEvent;
    use account_abstraction_core::domain::PoolConfig;
    use account_abstraction_core::infrastructure::in_memory::mempool::InMemoryMempool;
    use account_abstraction_core::services::interfaces::event_source::EventSource;
    use alloy_provider::RootProvider;
    use anyhow::Result;
    use async_trait::async_trait;
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
    use jsonrpsee::server::{ServerBuilder, ServerHandle};
    use mockall::mock;
    use serde_json::json;
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use tips_core::test_utils::create_test_meter_bundle_response;
    use tokio::sync::{RwLock, broadcast, mpsc};
    use url::Url;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    struct MockQueue;

    #[async_trait]
    impl MessageQueue for MockQueue {
        async fn publish(&self, _topic: &str, _key: &str, _payload: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    struct NoopEventSource;

    #[async_trait]
    impl EventSource for NoopEventSource {
        async fn receive(&self) -> anyhow::Result<MempoolEvent> {
            Err(anyhow::anyhow!("no events"))
        }
    }

    fn create_test_config(mock_server: &MockServer) -> Config {
        Config {
            address: IpAddr::from([127, 0, 0, 1]),
            port: 8080,
            mempool_url: Url::parse("http://localhost:3000").unwrap(),
            tx_submission_method: TxSubmissionMethod::Mempool,
            ingress_kafka_properties: String::new(),
            ingress_topic: String::new(),
            audit_kafka_properties: String::new(),
            audit_topic: String::new(),
            user_operation_consumer_properties: String::new(),
            user_operation_consumer_group_id: "tips-user-operation".to_string(),
            log_level: String::from("info"),
            log_format: tips_core::logger::LogFormat::Pretty,
            send_transaction_default_lifetime_seconds: 300,
            simulation_rpc: mock_server.uri().parse().unwrap(),
            metrics_addr: SocketAddr::from(([127, 0, 0, 1], 9002)),
            block_time_milliseconds: 1000,
            meter_bundle_timeout_ms: 5000,
            validate_user_operation_timeout_ms: 2000,
            builder_rpcs: vec![],
            max_buffered_meter_bundle_responses: 100,
            max_buffered_backrun_bundles: 100,
            health_check_addr: SocketAddr::from(([127, 0, 0, 1], 8081)),
            backrun_enabled: false,
            raw_tx_forward_rpc: None,
            chain_id: 11,
            user_operation_topic: String::new(),
            max_backrun_txs: 5,
            max_backrun_gas_limit: 5000000,
        }
    }

    async fn setup_rpc_server(mock: MockIngressApi) -> (HttpClient, ServerHandle) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();

        let addr = server.local_addr().unwrap();
        let handle = server.start(mock.into_rpc());

        let client = HttpClientBuilder::default()
            .build(format!("http://{}", addr))
            .unwrap();

        (client, handle)
    }

    fn sample_user_operation_v06() -> serde_json::Value {
        json!({
            "sender": "0x773d604960feccc5c2ce1e388595268187cf62bf",
            "nonce": "0x19b0ffe729f0000000000000000",
            "initCode": "0x9406cc6185a346906296840746125a0e449764545fbfb9cf000000000000000000000000f886bc0b4f161090096b82ac0c5eb7349add429d0000000000000000000000000000000000000000000000000000000000000000",
            "callData": "0xb61d27f600000000000000000000000066519fcaee1ed65bc9e0acc25ccd900668d3ed490000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000443f84ac0e000000000000000000000000773d604960feccc5c2ce1e388595268187cf62bf000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000",
            "callGasLimit": "0x5a3c",
            "verificationGasLimit": "0x5b7c7",
            "preVerificationGas": "0x1001744e6",
            "maxFeePerGas": "0x889fca3c",
            "maxPriorityFeePerGas": "0x1e8480",
            "paymasterAndData": "0x",
            "signature": "0x42eff6474dd0b7efd0ca3070e05ee0f3e3c6c665176b80c7768f59445d3415de30b65c4c6ae35c45822b726e8827a986765027e7e2d7d2a8d72c9cf0d23194b81c"
        })
    }

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

    // Replicate a failed `meter_bundle` request and instead of returning an error, we return a default `MeterBundleResponse`
    #[tokio::test]
    async fn test_meter_bundle_success() {
        let mock_server = MockServer::start().await;

        // Mock error response from base_meterBundle
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32000,
                    "message": "Simulation failed"
                }
            })))
            .mount(&mock_server)
            .await;

        let config = create_test_config(&mock_server);

        let provider: RootProvider<Optimism> =
            RootProvider::new_http(mock_server.uri().parse().unwrap());

        let providers = Providers {
            mempool: provider.clone(),
            simulation: provider.clone(),
            raw_tx_forward: None,
        };

        let (audit_tx, _audit_rx) = mpsc::unbounded_channel();
        let (builder_tx, _builder_rx) = broadcast::channel(1);
        let (backrun_tx, _backrun_rx) = broadcast::channel(1);

        let mempool_engine = Arc::new(MempoolEngine::<InMemoryMempool>::new(
            Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig::default()))),
            Arc::new(NoopEventSource),
        ));

        let service = IngressService::new(
            providers,
            MockQueue,
            audit_tx,
            builder_tx,
            backrun_tx,
            mempool_engine,
            config,
        );

        let bundle = Bundle::default();
        let bundle_hash = B256::default();

        let result = service.meter_bundle(&bundle, &bundle_hash).await;

        // Test that meter_bundle returns an error, but we handle it gracefully
        assert!(result.is_err());
        let response = result.unwrap_or_else(|_| MeterBundleResponse::default());
        assert_eq!(response, MeterBundleResponse::default());
    }

    #[tokio::test]
    async fn test_raw_tx_forward() {
        let simulation_server = MockServer::start().await;
        let forward_server = MockServer::start().await;

        // Mock error response from base_meterBundle
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32000,
                    "message": "Simulation failed"
                }
            })))
            .mount(&simulation_server)
            .await;

        // Mock forward endpoint - expect exactly 1 call
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": "0x0000000000000000000000000000000000000000000000000000000000000000"
            })))
            .expect(1)
            .mount(&forward_server)
            .await;

        let mut config = create_test_config(&simulation_server);
        config.tx_submission_method = TxSubmissionMethod::Kafka; // Skip mempool send

        let providers = Providers {
            mempool: RootProvider::new_http(simulation_server.uri().parse().unwrap()),
            simulation: RootProvider::new_http(simulation_server.uri().parse().unwrap()),
            raw_tx_forward: Some(RootProvider::new_http(
                forward_server.uri().parse().unwrap(),
            )),
        };

        let (audit_tx, _audit_rx) = mpsc::unbounded_channel();
        let (builder_tx, _builder_rx) = broadcast::channel(1);
        let (backrun_tx, _backrun_rx) = broadcast::channel(1);

        let mempool_engine = Arc::new(MempoolEngine::<InMemoryMempool>::new(
            Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig::default()))),
            Arc::new(NoopEventSource),
        ));

        let service = IngressService::new(
            providers,
            MockQueue,
            audit_tx,
            builder_tx,
            backrun_tx,
            mempool_engine,
            config,
        );

        // Valid signed transaction bytes
        let tx_bytes = Bytes::from_str("0x02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07").unwrap();

        let result = service.send_raw_transaction(tx_bytes).await;
        assert!(result.is_ok());

        // Wait for spawned forward task to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        // wiremock automatically verifies expect(1) when forward_server is dropped
    }
    mock! {
        pub IngressApi {}

        #[async_trait]
        impl IngressApiServer for IngressApi {
            async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash>;
            async fn send_backrun_bundle(&self, bundle: Bundle) -> RpcResult<BundleHash>;
            async fn cancel_bundle(&self, request: CancelBundle) -> RpcResult<()>;
            async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;
            async fn send_user_operation(
                &self,
                user_operation: VersionedUserOperation,
                entry_point: Address,
            ) -> RpcResult<FixedBytes<32>>;
        }
    }
    #[tokio::test]
    async fn test_send_user_operation_accepts_valid_payload() {
        let mut mock = MockIngressApi::new();
        mock.expect_send_user_operation()
            .times(1)
            .returning(|_, _| Ok(FixedBytes::ZERO));

        let (client, _handle) = setup_rpc_server(mock).await;

        let user_op = sample_user_operation_v06();
        let entry_point =
            account_abstraction_core::domain::entrypoints::version::EntryPointVersion::V06_ADDRESS;

        let result: Result<FixedBytes<32>, _> = client
            .request("eth_sendUserOperation", (user_op, entry_point))
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_user_operation_rejects_invalid_payload() {
        let mut mock = MockIngressApi::new();
        mock.expect_send_user_operation()
            .times(0)
            .returning(|_, _| Ok(FixedBytes::ZERO));

        let (client, _handle) = setup_rpc_server(mock).await;

        let user_op = sample_user_operation_v06();

        // Missing entry point argument should be rejected by the RPC layer
        let result: Result<FixedBytes<32>, _> =
            client.request("eth_sendUserOperation", (user_op,)).await;

        assert!(result.is_err());

        let wrong_user_op = json!({
            "nonce": "0x19b0ffe729f0000000000000000",
            "callGasLimit": "0x5a3c",
            "verificationGasLimit": "0x5b7c7",
            "preVerificationGas": "0x1001744e6",
            "maxFeePerGas": "0x889fca3c",
            "maxPriorityFeePerGas": "0x1e8480",
            "paymasterAndData": "0x",
            "signature": "0x42eff6474dd0b7efd0ca3070e05ee0f3e3c6c665176b80c7768f59445d3415de30b65c4c6ae35c45822b726e8827a986765027e7e2d7d2a8d72c9cf0d23194b81c"
        });

        let wrong_user_op_result: Result<FixedBytes<32>, _> = client
            .request("eth_sendUserOperation", (wrong_user_op, Address::ZERO))
            .await;

        assert!(wrong_user_op_result.is_err());
    }

    #[test]
    fn test_validate_backrun_bundle_rejects_invalid() {
        // Too few transactions (need at least 2: target + backrun)
        let result = validate_backrun_bundle_limits(1, 21000, 5, 5000000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least 2 transactions"));

        // Exceeds max tx count
        let result = validate_backrun_bundle_limits(6, 21000, 5, 5000000);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("exceeds max transaction count")
        );

        // Exceeds max gas limit
        let result = validate_backrun_bundle_limits(2, 6000000, 5, 5000000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds max gas limit"));
    }
}
