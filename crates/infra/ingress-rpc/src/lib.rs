#![doc = include_str!("../README.md")]

/// Health check HTTP server.
mod health;
pub use health::HealthServer;

/// Prometheus metrics for the ingress RPC service.
mod metrics;
pub use metrics::Metrics;

/// Core RPC service implementation.
mod service;
pub use service::{IngressApiServer, IngressService};

/// Transaction validation implementation.
mod validation;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use alloy_primitives::TxHash;
use alloy_provider::{Provider, RootProvider};
use base_bundles::MeterBundleResponse;
use base_common_network::Base;
use base_observability_events::{
    DEFAULT_QUEUE_CAPACITY, TransactionEventProducer, TransactionEventType,
    TransactionEventWriterConfig, transaction_event,
};
use clap::Args;
use serde_json::{Map, json};
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};
use url::Url;
pub use validation::{AccountInfo, AccountInfoLookup, L1BlockInfoLookup, validate_bundle};

/// Metering response plus the transaction hashes from the original ingress
/// request, used to keep builder-send journal events transaction-scoped even
/// when the simulator returns an empty result set.
#[derive(Debug, Clone)]
pub struct MeteringForwardMessage {
    /// Transaction hashes from the original ingress request.
    pub tx_hashes: Vec<TxHash>,
    /// Metering response returned by the simulation RPC.
    pub response: MeterBundleResponse,
}

/// Configuration for the tips ingress RPC service.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Address to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_ADDRESS", default_value = "0.0.0.0")]
    pub address: IpAddr,

    /// Port to bind the RPC server to
    #[arg(long, env = "TIPS_INGRESS_PORT", default_value = "8080")]
    pub port: u16,

    /// Deprecated. Ingress no longer proxies transactions to the mempool.
    #[arg(long = "mempool-url", env = "TIPS_INGRESS_RPC_MEMPOOL", hide = true)]
    pub deprecated_mempool_url: Option<Url>,

    /// URL of the audit-archiver RPC endpoint that receives bundle events via
    /// `base_persistBatchedBundleEvent`.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_RPC_URL")]
    pub audit_rpc_url: Url,

    /// Per-request timeout for audit RPC calls, in seconds.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_RPC_TIMEOUT_SECS", default_value = "2")]
    pub audit_rpc_timeout_secs: u64,

    /// Flush the audit batch when it reaches this many events.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_BATCH_MAX_SIZE", default_value = "50")]
    pub audit_batch_max_size: usize,

    /// Maximum time (ms) the first event in a batch waits before forced flush.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_BATCH_MAX_WAIT_MS", default_value = "25")]
    pub audit_batch_max_wait_ms: u64,

    /// Default lifetime for sent transactions in seconds (default: 3 hours)
    #[arg(
        long,
        env = "TIPS_INGRESS_SEND_TRANSACTION_DEFAULT_LIFETIME_SECONDS",
        default_value = "10800"
    )]
    pub send_transaction_default_lifetime_seconds: u64,

    /// URL of the simulation RPC service for bundle metering
    #[arg(long, env = "TIPS_INGRESS_RPC_SIMULATION")]
    pub simulation_rpc: Url,

    /// Configurable block time in milliseconds (default: 2000 milliseconds)
    #[arg(long, env = "TIPS_INGRESS_BLOCK_TIME_MILLISECONDS", default_value = "2000")]
    pub block_time_milliseconds: u64,

    /// Timeout for bundle metering in milliseconds (default: 2000 milliseconds)
    #[arg(long, env = "TIPS_INGRESS_METER_BUNDLE_TIMEOUT_MS", default_value = "2000")]
    pub meter_bundle_timeout_ms: u64,

    /// URLs of the builder RPC service for setting metering information
    #[arg(long, env = "TIPS_INGRESS_BUILDER_RPCS", value_delimiter = ',')]
    pub builder_rpcs: Vec<Url>,

    /// Maximum number of `MeterBundleResponse`s to buffer in memory
    #[arg(long, env = "TIPS_INGRESS_MAX_BUFFERED_METER_BUNDLE_RESPONSES", default_value = "100")]
    pub max_buffered_meter_bundle_responses: usize,

    /// Address to bind the health check server to
    #[arg(long, env = "TIPS_INGRESS_HEALTH_CHECK_ADDR", default_value = "0.0.0.0:8081")]
    pub health_check_addr: SocketAddr,

    /// chain id
    #[arg(long, env = "TIPS_INGRESS_CHAIN_ID", default_value = "11")]
    pub chain_id: u64,

    /// Deprecated. Ingress no longer forwards raw transactions to another RPC endpoint.
    #[arg(long = "raw-tx-forward-rpc", env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC", hide = true)]
    pub deprecated_raw_tx_forward_rpc: Option<Url>,

    /// TTL for bundle cache in seconds
    #[arg(long, env = "TIPS_INGRESS_BUNDLE_CACHE_TTL", default_value = "20")]
    pub bundle_cache_ttl: u64,

    /// Capacity of the bounded audit event channel.
    ///
    /// When the channel is full, new audit events are dropped to avoid blocking
    /// the RPC handler.
    #[arg(long, env = "TIPS_INGRESS_AUDIT_CHANNEL_CAPACITY", default_value = "512")]
    pub audit_channel_capacity: usize,

    /// Enable sending to builder
    #[arg(long, env = "TIPS_INGRESS_SEND_TO_BUILDER", default_value = "false")]
    pub send_to_builder: bool,

    /// Enables transaction observability JSONL journal writes.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_ENABLED", default_value = "false")]
    pub transaction_events_enabled: bool,

    /// Dedicated JSONL file path tailed by the transaction-events sidecar.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_FILE_PATH",
        default_value = "/var/log/base/transaction-events.jsonl"
    )]
    pub transaction_events_file_path: PathBuf,

    /// Bounded in-process queue capacity for journal writes.
    #[arg(
        long,
        env = "TIPS_INGRESS_TRANSACTION_EVENTS_QUEUE_CAPACITY",
        default_value_t = DEFAULT_QUEUE_CAPACITY
    )]
    pub transaction_events_queue_capacity: usize,

    /// Fail service initialization if the journal file cannot be opened.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_REQUIRED", default_value = "false")]
    pub transaction_events_required: bool,

    /// Network label written into transaction observability events.
    #[arg(long, env = "TIPS_INGRESS_TRANSACTION_EVENTS_NETWORK", default_value = "base-mainnet")]
    pub transaction_events_network: String,
}

impl Config {
    /// Builds the shared JSONL writer config for ingress transaction events.
    pub fn transaction_event_writer_config(&self) -> TransactionEventWriterConfig {
        TransactionEventWriterConfig {
            enabled: self.transaction_events_enabled,
            file_path: self.transaction_events_file_path.clone(),
            queue_capacity: self.transaction_events_queue_capacity,
            required: self.transaction_events_required,
            producer: TransactionEventProducer::IngressRpc,
            network: self.transaction_events_network.clone(),
        }
    }
}

/// Maximum number of concurrent RPC calls per builder URL.
const MAX_CONCURRENT_RPCS: usize = 64;

/// Connects ingress metering data to builder RPCs.
#[derive(Debug)]
pub struct BuilderConnector;

impl BuilderConnector {
    /// Spawns a background task that forwards metering data to the builder RPC.
    ///
    /// RPC calls are dispatched concurrently (up to [`MAX_CONCURRENT_RPCS`]) so
    /// that slow responses don't block the recv loop and risk broadcast channel
    /// lag.
    pub fn connect(
        metering_rx: broadcast::Receiver<MeteringForwardMessage>,
        builder_rpc: Url,
        destination_index: usize,
    ) {
        let rpc_url = builder_rpc.clone();
        let builder: RootProvider<Base> = RootProvider::new_http(builder_rpc);

        tokio::spawn(async move {
            let mut event_rx = metering_rx;
            let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_RPCS));
            let mut join_set = JoinSet::new();
            info!(url = %rpc_url, "BuilderConnector started, waiting for metering data");
            loop {
                // Drain completed tasks to observe panics / errors.
                while let Some(result) = join_set.try_join_next() {
                    if let Err(e) = result {
                        error!(url = %rpc_url, error = %e, "RPC forwarding task failed");
                    }
                }

                match event_rx.recv().await {
                    Ok(message) => {
                        let event = message.response;
                        if event.results.is_empty() {
                            for tx_hash in &message.tx_hashes {
                                emit_metering_send_event(
                                    TransactionEventType::IngressMeteringSendDropped,
                                    *tx_hash,
                                    event.bundle_hash,
                                    destination_index,
                                    Map::from_iter([(
                                        "drop_reason".to_string(),
                                        json!("empty_results"),
                                    )]),
                                );
                            }
                            warn!(
                                url = %rpc_url,
                                hash = %event.bundle_hash,
                                "Received metering information with no transactions"
                            );
                            continue;
                        }

                        let tx_hash = event.results[0].tx_hash;
                        let bundle_hash = event.bundle_hash;
                        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
                            break;
                        };
                        let builder = builder.clone();
                        let url = rpc_url.clone();
                        join_set.spawn(async move {
                            emit_metering_send_event(
                                TransactionEventType::IngressMeteringSendAttempt,
                                tx_hash,
                                bundle_hash,
                                destination_index,
                                Map::new(),
                            );
                            match builder
                                .client()
                                .request::<(TxHash, MeterBundleResponse), ()>(
                                    "base_setMeteringInformation",
                                    (tx_hash, event),
                                )
                                .await
                            {
                                Ok(()) => {
                                    emit_metering_send_event(
                                        TransactionEventType::IngressMeteringSendSuccess,
                                        tx_hash,
                                        bundle_hash,
                                        destination_index,
                                        Map::new(),
                                    );
                                    debug!(
                                        url = %url,
                                        tx_hash = %tx_hash,
                                        "Forwarded metering information"
                                    );
                                }
                                Err(e) => {
                                    emit_metering_send_event(
                                        TransactionEventType::IngressMeteringSendFailure,
                                        tx_hash,
                                        bundle_hash,
                                        destination_index,
                                        Map::from_iter([(
                                            "error".to_string(),
                                            json!(e.to_string()),
                                        )]),
                                    );
                                    error!(
                                        url = %url,
                                        error = %e,
                                        tx_hash = %tx_hash,
                                        "Failed to set metering information"
                                    );
                                }
                            }
                            drop(permit);
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            url = %rpc_url,
                            skipped = n,
                            "BuilderConnector lagged behind, skipped messages"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(url = %rpc_url, "BuilderConnector channel closed, shutting down");
                        break;
                    }
                }
            }

            // Drain remaining in-flight tasks on shutdown.
            while let Some(result) = join_set.join_next().await {
                if let Err(e) = result {
                    error!(url = %rpc_url, error = %e, "RPC forwarding task failed during shutdown");
                }
            }
        });
    }
}

fn emit_metering_send_event(
    event_type: TransactionEventType,
    tx_hash: TxHash,
    bundle_hash: alloy_primitives::B256,
    destination_index: usize,
    mut data: Map<String, serde_json::Value>,
) {
    data.entry("bundle_hash".to_string()).or_insert_with(|| json!(bundle_hash.to_string()));
    data.entry("target".to_string()).or_insert_with(|| json!("builder_metering"));
    data.entry("rpc_method".to_string()).or_insert_with(|| json!("base_setMeteringInformation"));
    data.entry("destination_index".to_string()).or_insert_with(|| json!(destination_index));

    if let Err(err) = transaction_event!(
        producer: TransactionEventProducer::IngressRpc,
        event_type: event_type,
        tx_hash: tx_hash,
        id: {
            "destination_index" => destination_index,
            "bundle_hash" => bundle_hash.to_string(),
        },
        data: data,
    ) {
        debug!(error = %err, event_type = %event_type, "transaction event not written");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, TxHash, U256};
    use base_bundles::{MeterBundleResponse, TransactionResult};
    use tokio::sync::broadcast;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::{BuilderConnector, MeteringForwardMessage};

    fn response_with_results() -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: Address::ZERO,
                gas_fees: U256::ZERO,
                gas_price: U256::ZERO,
                gas_used: 21000,
                to_address: Some(Address::ZERO),
                tx_hash: TxHash::ZERO,
                value: U256::ZERO,
                execution_time_us: 500,
                opcode_gas: vec![],
            }],
            ..Default::default()
        }
    }

    fn forwarding_message(response: MeterBundleResponse) -> MeteringForwardMessage {
        let tx_hashes = if response.results.is_empty() {
            vec![TxHash::ZERO]
        } else {
            response.results.iter().map(|result| result.tx_hash).collect()
        };
        MeteringForwardMessage { tx_hashes, response }
    }

    fn jsonrpc_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        }))
    }

    #[tokio::test]
    async fn test_builder_connector_survives_lagged_receiver() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(jsonrpc_ok())
            .expect(1..)
            .mount(&mock_server)
            .await;

        // Create a tiny broadcast channel so it's easy to overflow.
        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(2);

        // Overflow the buffer before the connector starts reading.
        // The receiver will get RecvError::Lagged on its first recv().
        let event = forwarding_message(response_with_results());
        for _ in 0..5 {
            tx.send(event.clone()).unwrap();
        }

        // Start the connector with the already-lagged receiver.
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Give the connector time to hit Lagged and recover.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a new message after recovery — this must be forwarded.
        // send() fails with SendError when there are zero receivers,
        // which is exactly what happened with the old buggy code: the
        // connector task exited on Lagged, dropping the only receiver.
        assert!(
            tx.send(event).is_ok(),
            "connector task died — receiver was dropped after Lagged error"
        );

        // Wait for the RPC call to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // wiremock verifies expect(1..) — at least one call was made,
        // proving the connector survived the Lagged error.
    }

    #[tokio::test]
    async fn test_builder_connector_forwards_metering_data() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(1).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        tx.send(forwarding_message(response_with_results())).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies exactly 1 call was made.
    }

    #[tokio::test]
    async fn test_builder_connector_skips_empty_results() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(0).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Default response has empty results — should be skipped.
        tx.send(forwarding_message(MeterBundleResponse::default())).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies 0 calls were made.
    }

    #[tokio::test]
    async fn test_builder_connector_forwards_concurrently() {
        let mock_server = MockServer::start().await;

        // Each response takes 200ms. Sequential forwarding would need >=1000ms
        // for 5 messages. Concurrent forwarding completes in ~200ms; we allow
        // a generous 2s budget so CI load doesn't cause flaky failures.
        Mock::given(method("POST"))
            .respond_with(jsonrpc_ok().set_delay(Duration::from_millis(200)))
            .expect(5)
            .mount(&mock_server)
            .await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        for _ in 0..5 {
            tx.send(forwarding_message(response_with_results())).unwrap();
        }

        // 2s is generous for concurrent (~200ms) but well under sequential (>=1s).
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // wiremock verifies exactly 5 calls were made within the time window.
    }

    #[tokio::test]
    async fn test_builder_connector_shuts_down_on_channel_close() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(1).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeteringForwardMessage>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap(), 0);

        // Send one message, then close the channel.
        tx.send(forwarding_message(response_with_results())).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(tx);

        // The task should exit gracefully without panic.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
