use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::transaction::{Recovered, SignerRecoverable};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider, network::eip2718::Decodable2718};
use alloy_rpc_types_eth::error::EthRpcErrorCode;
use audit_archiver_lib::BundleEvent;
use base_bundles::{AcceptedBundle, Bundle, BundleExtensions, MeterBundleResponse, ParsedBundle};
use base_common_chains::ChainConfig;
use base_common_consensus::{BaseTxEnvelope, EIP8130_REJECTION_MSG};
use base_common_network::Base;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use moka::future::Cache;
use reth_rpc_eth_types::EthApiError;
use reth_rpc_server_types::result::rpc_err;
use tokio::{
    sync::{broadcast, mpsc},
    time::{Duration, Instant, timeout},
};
use tracing::{debug, info, warn};

use crate::{Config, MeteringForwardMessage, metrics::Metrics};

#[rpc(server, namespace = "eth")]
pub trait IngressApi {
    /// Handler for: `eth_sendRawTransaction`
    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;
}

/// Core ingress RPC service that handles transaction submission.
pub struct IngressService {
    simulation_provider: Arc<RootProvider<Base>>,
    audit_channel: mpsc::Sender<BundleEvent>,
    send_transaction_default_lifetime_seconds: u64,
    block_time_milliseconds: u64,
    meter_bundle_timeout_ms: u64,
    builder_tx: broadcast::Sender<MeteringForwardMessage>,
    bundle_cache: Cache<B256, ()>,
    send_to_builder: bool,
    cobalt_timestamp: Option<u64>,
}

impl std::fmt::Debug for IngressService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressService").finish_non_exhaustive()
    }
}

impl IngressService {
    /// Creates a new ingress service with the given providers and configuration.
    pub fn new(
        simulation_provider: RootProvider<Base>,
        audit_channel: mpsc::Sender<BundleEvent>,
        builder_tx: broadcast::Sender<MeteringForwardMessage>,
        config: Config,
    ) -> Self {
        let cobalt_timestamp = ChainConfig::by_chain_id(config.chain_id)
            .and_then(|chain_config| chain_config.cobalt_timestamp);
        let simulation_provider = Arc::new(simulation_provider);

        // A TTL cache to deduplicate bundles with the same Bundle ID
        let bundle_cache =
            Cache::builder().time_to_live(Duration::from_secs(config.bundle_cache_ttl)).build();
        Self {
            simulation_provider,
            audit_channel,
            send_transaction_default_lifetime_seconds: config
                .send_transaction_default_lifetime_seconds,
            block_time_milliseconds: config.block_time_milliseconds,
            meter_bundle_timeout_ms: config.meter_bundle_timeout_ms,
            builder_tx,
            bundle_cache,
            send_to_builder: config.send_to_builder,
            cobalt_timestamp,
        }
    }
}

#[async_trait]
impl IngressApiServer for IngressService {
    async fn send_raw_transaction(&self, data: Bytes) -> RpcResult<B256> {
        let start = Instant::now();
        let transaction = self.get_tx(&data).await?;
        let tx_hash = transaction.tx_hash();

        Metrics::transactions_received().increment(1);
        Self::emit_transaction_event(TransactionEventType::IngressReceived, tx_hash, None);

        let expiry_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
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

        if self.bundle_cache.get(bundle_hash).await.is_some() {
            debug!(
                message = "Duplicate bundle detected, skipping",
                bundle_hash = %bundle_hash,
                transaction_hash = %transaction.tx_hash(),
            );
        } else {
            self.bundle_cache.insert(*bundle_hash, ()).await;
            Metrics::bundles_parsed().increment(1);

            Self::emit_transaction_event(
                TransactionEventType::SimulationStarted,
                tx_hash,
                Some(*bundle_hash),
            );
            let simulation_start = Instant::now();
            let (meter_bundle_response, simulation_accepted): (Option<MeterBundleResponse>, bool) =
                match self.meter_bundle(&bundle, bundle_hash).await {
                    Ok(response) => {
                        Self::emit_simulation_event(
                            TransactionEventType::SimulationSucceeded,
                            tx_hash,
                            *bundle_hash,
                            simulation_start.elapsed(),
                            &response,
                        );
                        info!(message = "Metering succeeded for raw transaction", bundle_hash = %bundle_hash, response = ?response);
                        (Some(response), true)
                    }
                    Err(e) => {
                        Self::emit_simulation_failed_event(
                            tx_hash,
                            *bundle_hash,
                            simulation_start.elapsed(),
                            e.error.to_string(),
                            e.metering.as_ref(),
                        );
                        warn!(
                            bundle_hash = %bundle_hash,
                            error = %e.error,
                            "Metering failed for raw transaction"
                        );
                        (e.metering, false)
                    }
                };

            if let Some(meter_info) = meter_bundle_response.as_ref() {
                if simulation_accepted {
                    Metrics::successful_simulations().increment(1);
                } else {
                    Metrics::failed_simulations().increment(1);
                }

                if self.send_to_builder && simulation_accepted {
                    // Update the current size of the `builder_tx` channel captured right before sending to the builder
                    Metrics::buffered_meter_bundle_responses_size()
                        .set(self.builder_tx.len() as f64);
                    let message = MeteringForwardMessage {
                        tx_hashes: vec![tx_hash],
                        response: meter_info.clone(),
                    };
                    match self.builder_tx.send(message) {
                        Ok(n) => debug!(
                            receivers = n,
                            bundle_hash = %bundle_hash,
                            "Broadcast metering data to builder connectors"
                        ),
                        Err(e) => warn!(
                            bundle_hash = %bundle_hash,
                            error = %e,
                            "No active receivers for metering broadcast"
                        ),
                    }
                }
            } else {
                Metrics::failed_simulations().increment(1);
            }

            let accepted_bundle =
                AcceptedBundle::new(parsed_bundle, meter_bundle_response.unwrap_or_default());

            info!(
                message = "processed transaction",
                bundle_hash = %bundle_hash,
                transaction_hash = %transaction.tx_hash(),
            );

            self.send_audit_event(&accepted_bundle, accepted_bundle.bundle_hash());
        }

        Metrics::send_raw_transaction_duration().record(start.elapsed().as_secs_f64());

        Ok(transaction.tx_hash())
    }
}

impl IngressService {
    async fn get_tx(&self, data: &Bytes) -> RpcResult<Recovered<BaseTxEnvelope>> {
        if data.is_empty() {
            return Err(EthApiError::EmptyRawTransactionData.into_rpc_err());
        }

        let envelope = BaseTxEnvelope::decode_2718_exact(data.iter().as_slice())
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;
        self.ensure_cobalt_active_for_eip8130(&envelope).await?;

        let transaction = envelope
            .try_into_recovered()
            .map_err(|_| EthApiError::FailedToDecodeSignedTransaction.into_rpc_err())?;
        Ok(transaction)
    }

    async fn ensure_cobalt_active_for_eip8130(&self, envelope: &BaseTxEnvelope) -> RpcResult<()> {
        if !envelope.is_eip8130() {
            return Ok(());
        }
        let Some(cobalt_timestamp) = self.cobalt_timestamp else {
            return Err(Self::eip8130_pre_cobalt_error());
        };
        if cobalt_timestamp == 0 {
            return Ok(());
        }

        let block = self
            .simulation_provider
            .get_block(BlockId::Number(BlockNumberOrTag::Latest))
            .await
            .map_err(|error| {
                warn!(error = %error, "failed to fetch latest block for EIP-8130 Cobalt gate");
                EthApiError::InternalEthError.into_rpc_err()
            })?
            .ok_or_else(|| {
                warn!("latest block missing for EIP-8130 Cobalt gate");
                EthApiError::InternalEthError.into_rpc_err()
            })?;

        if block.header.timestamp < cobalt_timestamp {
            return Err(Self::eip8130_pre_cobalt_error());
        }
        Ok(())
    }

    fn eip8130_pre_cobalt_error() -> jsonrpsee::types::ErrorObjectOwned {
        rpc_err(EthRpcErrorCode::TransactionRejected.code(), EIP8130_REJECTION_MSG, None)
    }

    /// `meter_bundle` is used to determine how long a bundle will take to execute. A bundle that
    /// is within `block_time_milliseconds` will return the `MeterBundleResponse` that can be passed along
    /// to the builder.
    async fn meter_bundle(
        &self,
        bundle: &Bundle,
        bundle_hash: &B256,
    ) -> Result<MeterBundleResponse, MeterBundleFailure> {
        let start = Instant::now();
        let timeout_duration = Duration::from_millis(self.meter_bundle_timeout_ms);

        // The future we await has the nested type:
        // Result<
        //   RpcResult<MeterBundleResponse>, // 1. The inner operation's result
        //   tokio::time::error::Elapsed     // 2. The outer timeout's result
        // >
        let res: MeterBundleResponse = timeout(
            timeout_duration,
            self.simulation_provider.client().request("base_meterBundle", (bundle,)),
        )
        .await
        .map_err(|_| {
            warn!(message = "Timed out on requesting metering", bundle_hash = %bundle_hash);
            MeterBundleFailure::without_response(
                EthApiError::InvalidParams("Timeout on requesting metering".into()).into_rpc_err(),
            )
        })?
        .map_err(|e| {
            MeterBundleFailure::without_response(
                EthApiError::InvalidParams(e.to_string()).into_rpc_err(),
            )
        })?;

        Metrics::rpc_latency("base_meterBundle").record(start.elapsed().as_secs_f64());

        // we can save some builder payload building computation by not including bundles
        // that we know will take longer than the block time to execute
        let total_execution_time = (res.total_execution_time_us / 1_000) as u64;
        if total_execution_time > self.block_time_milliseconds {
            Metrics::bundles_exceeded_metering_time().increment(1);
            return Err(MeterBundleFailure::with_response(
                EthApiError::InvalidParams("Bundle simulation took too long".into()).into_rpc_err(),
                res,
            ));
        }
        Ok(res)
    }

    /// Helper method to send audit event for a bundle.
    ///
    /// Uses `try_send` on the bounded channel to avoid blocking the RPC handler.
    /// If the channel is full, the event is dropped and a warning is logged.
    fn send_audit_event(&self, accepted_bundle: &AcceptedBundle, bundle_hash: B256) {
        let audit_event = BundleEvent::Received {
            bundle_id: *accepted_bundle.uuid(),
            bundle: Box::new(accepted_bundle.clone()),
        };
        match self.audit_channel.try_send(audit_event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                Metrics::audit_channel_full().increment(1);
                warn!(
                    message = "audit channel full, dropping event",
                    bundle_hash = %bundle_hash,
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    message = "audit channel closed",
                    bundle_hash = %bundle_hash,
                );
            }
        }
    }

    fn emit_transaction_event(
        event_type: TransactionEventType,
        tx_hash: B256,
        bundle_hash: Option<B256>,
    ) {
        Self::emit_transaction_event_with_data(
            event_type,
            tx_hash,
            bundle_hash,
            serde_json::Map::new(),
        );
    }

    fn emit_simulation_event(
        event_type: TransactionEventType,
        tx_hash: B256,
        bundle_hash: B256,
        duration: Duration,
        metering: &MeterBundleResponse,
    ) {
        let mut data = metering_summary_data(bundle_hash, metering);
        data.insert(
            "simulation_duration_ms".to_string(),
            serde_json::json!(duration.as_secs_f64() * 1000.0),
        );

        Self::emit_transaction_event_with_data(event_type, tx_hash, Some(bundle_hash), data);
    }

    fn emit_simulation_failed_event(
        tx_hash: B256,
        bundle_hash: B256,
        duration: Duration,
        reason: String,
        metering: Option<&MeterBundleResponse>,
    ) {
        let mut data = metering.map_or_else(
            || {
                serde_json::Map::from_iter([(
                    "bundle_hash".to_string(),
                    serde_json::json!(bundle_hash.to_string()),
                )])
            },
            |metering| metering_summary_data(bundle_hash, metering),
        );
        data.extend([
            (
                "simulation_duration_ms".to_string(),
                serde_json::json!(duration.as_secs_f64() * 1000.0),
            ),
            ("rejection_reason".to_string(), serde_json::json!(reason)),
            ("rejection_code".to_string(), serde_json::json!("simulation_error")),
        ]);
        Self::emit_transaction_event_with_data(
            TransactionEventType::SimulationFailed,
            tx_hash,
            Some(bundle_hash),
            data,
        );
    }

    fn emit_transaction_event_with_data(
        event_type: TransactionEventType,
        tx_hash: B256,
        bundle_hash: Option<B256>,
        mut data: serde_json::Map<String, serde_json::Value>,
    ) {
        if let Some(bundle_hash) = bundle_hash {
            data.entry("bundle_hash".to_string())
                .or_insert_with(|| serde_json::json!(bundle_hash.to_string()));
        }

        if let Err(err) = transaction_event!(
            producer: TransactionEventProducer::IngressRpc,
            event_type: event_type,
            tx_hash: tx_hash,
            data: data,
        ) {
            debug!(error = %err, event_type = %event_type, tx_hash = %tx_hash, "transaction event not written");
        }
    }
}

fn metering_summary_data(
    bundle_hash: B256,
    metering: &MeterBundleResponse,
) -> serde_json::Map<String, serde_json::Value> {
    let mut data = serde_json::Map::from_iter([(
        "bundle_hash".to_string(),
        serde_json::json!(bundle_hash.to_string()),
    )]);
    if let Ok(metering_response) = serde_json::to_value(metering) {
        data.insert("meter_bundle_response".to_string(), metering_response);
    }
    data
}

#[derive(Debug)]
struct MeterBundleFailure {
    error: ErrorObjectOwned,
    metering: Option<MeterBundleResponse>,
}

impl MeterBundleFailure {
    const fn without_response(error: ErrorObjectOwned) -> Self {
        Self { error, metering: None }
    }

    const fn with_response(error: ErrorObjectOwned, metering: MeterBundleResponse) -> Self {
        Self { error, metering: Some(metering) }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, SocketAddr},
        str::FromStr,
    };

    use alloy_provider::RootProvider;
    use base_bundles::test_utils::create_test_meter_bundle_response;
    use tokio::sync::{broadcast, mpsc};
    use url::Url;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::*;
    use crate::Config;

    fn create_test_config(mock_server: &MockServer) -> Config {
        Config {
            address: IpAddr::from([127, 0, 0, 1]),
            port: 8080,
            deprecated_mempool_url: None,
            send_transaction_default_lifetime_seconds: 300,
            simulation_rpc: mock_server.uri().parse().unwrap(),
            block_time_milliseconds: 1000,
            meter_bundle_timeout_ms: 5000,
            builder_rpcs: vec![],
            max_buffered_meter_bundle_responses: 100,
            health_check_addr: SocketAddr::from(([127, 0, 0, 1], 8081)),
            chain_id: 11,
            deprecated_raw_tx_forward_rpc: None,
            bundle_cache_ttl: 20,
            audit_channel_capacity: 512,
            send_to_builder: false,
            audit_batch_max_size: 100,
            audit_batch_max_wait_ms: 1000,
            audit_rpc_timeout_secs: 5,
            audit_rpc_url: Url::parse("http://localhost:9000").unwrap(),
            transaction_events_enabled: false,
            transaction_events_file_path: "/tmp/transaction-events.jsonl".into(),
            transaction_events_queue_capacity: 1024,
            transaction_events_required: false,
            transaction_events_network: "base-mainnet".to_string(),
        }
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
        // we're assuming that `base_meterBundle` will not error hence the second unwrap
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

        let simulation_provider: RootProvider<Base> =
            RootProvider::new_http(mock_server.uri().parse().unwrap());

        let (audit_tx, _audit_rx) = mpsc::channel(512);
        let (builder_tx, _builder_rx) = broadcast::channel(1);

        let service = IngressService::new(simulation_provider, audit_tx, builder_tx, config);

        let bundle = Bundle::default();
        let bundle_hash = B256::default();

        let result = service.meter_bundle(&bundle, &bundle_hash).await;

        // Test that meter_bundle returns an error, but we handle it gracefully
        assert!(result.is_err());
        let response = result.unwrap_or_else(|_| MeterBundleResponse::default());
        assert_eq!(response, MeterBundleResponse::default());
    }

    #[tokio::test]
    async fn test_send_raw_transaction_meters_broadcasts_and_audits() {
        let simulation_server = MockServer::start().await;

        let meter_response = create_test_meter_bundle_response();

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": meter_response,
            })))
            .expect(1)
            .mount(&simulation_server)
            .await;

        let mut config = create_test_config(&simulation_server);
        config.send_to_builder = true;

        let simulation_provider = RootProvider::new_http(simulation_server.uri().parse().unwrap());
        let (audit_tx, mut audit_rx) = mpsc::channel(512);
        let (builder_tx, mut builder_rx) = broadcast::channel(1);

        let service = IngressService::new(simulation_provider, audit_tx, builder_tx, config);

        // Valid signed transaction bytes
        let tx_bytes = Bytes::from_str("0x02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07").unwrap();

        let result = service.send_raw_transaction(tx_bytes).await;
        assert!(result.is_ok());

        let metering = timeout(Duration::from_secs(1), builder_rx.recv()).await.unwrap().unwrap();
        assert_eq!(metering.response, create_test_meter_bundle_response());

        let audit_event = timeout(Duration::from_secs(1), audit_rx.recv()).await.unwrap().unwrap();
        assert!(matches!(audit_event, BundleEvent::Received { .. }));
    }
}
