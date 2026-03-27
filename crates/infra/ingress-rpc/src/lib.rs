#![doc = include_str!("../README.md")]

/// Health check HTTP server.
mod health;
pub use health::HealthServer;

/// Prometheus metrics for the ingress RPC service.
mod metrics;
pub use metrics::Metrics;

/// Kafka message queue publishing.
mod queue;
pub use queue::{BundleQueuePublisher, KafkaMessageQueue, MessageQueue};

/// Core RPC service implementation.
mod service;
pub use service::{IngressApiServer, IngressService, Providers};

/// Transaction validation implementation.
mod validation;
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use alloy_primitives::TxHash;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use base_alloy_network::Base;
use base_bundles::MeterBundleResponse;
use clap::Args;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use url::Url;
pub use validation::{AccountInfo, AccountInfoLookup, L1BlockInfoLookup, validate_bundle};

/// Method used to submit transactions to the mempool and/or Kafka.
#[derive(Debug, Clone, Copy)]
pub enum TxSubmissionMethod {
    /// Submit via the mempool RPC only.
    Mempool,
    /// Submit via Kafka only.
    Kafka,
    /// Submit via both mempool RPC and Kafka.
    MempoolAndKafka,
    /// Do not submit transactions.
    None,
}

impl FromStr for TxSubmissionMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mempool" => Ok(Self::Mempool),
            "kafka" => Ok(Self::Kafka),
            "mempool,kafka" | "kafka,mempool" => Ok(Self::MempoolAndKafka),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "Invalid submission method: '{s}'. Valid options: mempool, kafka, mempool,kafka, kafka,mempool, none"
            )),
        }
    }
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

    /// URL of the mempool service to proxy transactions to
    #[arg(long, env = "TIPS_INGRESS_RPC_MEMPOOL")]
    pub mempool_url: Url,

    /// Method to submit transactions to the mempool
    #[arg(long, env = "TIPS_INGRESS_TX_SUBMISSION_METHOD", default_value = "mempool")]
    pub tx_submission_method: TxSubmissionMethod,

    /// Kafka brokers for publishing mempool events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_PROPERTIES_FILE")]
    pub ingress_kafka_properties: String,

    /// Kafka topic for queuing transactions before the DB Writer
    #[arg(long, env = "TIPS_INGRESS_KAFKA_INGRESS_TOPIC", default_value = "tips-ingress")]
    pub ingress_topic: String,

    /// Kafka properties file for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_PROPERTIES_FILE")]
    pub audit_kafka_properties: String,

    /// Kafka topic for audit events
    #[arg(long, env = "TIPS_INGRESS_KAFKA_AUDIT_TOPIC", default_value = "tips-audit")]
    pub audit_topic: String,

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

    /// URL of third-party RPC endpoint to forward raw transactions to (enables forwarding if set)
    #[arg(long, env = "TIPS_INGRESS_RAW_TX_FORWARD_RPC")]
    pub raw_tx_forward_rpc: Option<Url>,

    /// TTL for bundle cache in seconds
    #[arg(long, env = "TIPS_INGRESS_BUNDLE_CACHE_TTL", default_value = "20")]
    pub bundle_cache_ttl: u64,

    /// Enable sending to builder
    #[arg(long, env = "TIPS_INGRESS_SEND_TO_BUILDER", default_value = "false")]
    pub send_to_builder: bool,
}

/// Connects ingress metering data to builder RPCs.
#[derive(Debug)]
pub struct BuilderConnector;

impl BuilderConnector {
    /// Spawns a background task that forwards metering data to the builder RPC.
    pub fn connect(metering_rx: broadcast::Receiver<MeterBundleResponse>, builder_rpc: Url) {
        let rpc_url = builder_rpc.clone();
        let builder: RootProvider<Base> = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(builder_rpc);

        tokio::spawn(async move {
            let mut event_rx = metering_rx;
            info!(url = %rpc_url, "BuilderConnector started, waiting for metering data");
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        if event.results.is_empty() {
                            warn!(
                                url = %rpc_url,
                                hash = %event.bundle_hash,
                                "Received metering information with no transactions"
                            );
                            continue;
                        }

                        let tx_hash = event.results[0].tx_hash;
                        match builder
                            .client()
                            .request::<(TxHash, MeterBundleResponse), ()>(
                                "base_setMeteringInformation",
                                (tx_hash, event),
                            )
                            .await
                        {
                            Ok(()) => debug!(
                                url = %rpc_url,
                                tx_hash = %tx_hash,
                                "Forwarded metering information"
                            ),
                            Err(e) => error!(
                                url = %rpc_url,
                                error = %e,
                                tx_hash = %tx_hash,
                                "Failed to set metering information"
                            ),
                        }
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
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, TxHash, U256};
    use base_bundles::{MeterBundleResponse, TransactionResult};
    use tokio::sync::broadcast;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::BuilderConnector;

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
            }],
            ..Default::default()
        }
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
        let (tx, rx) = broadcast::channel::<MeterBundleResponse>(2);

        // Overflow the buffer before the connector starts reading.
        // The receiver will get RecvError::Lagged on its first recv().
        let event = response_with_results();
        for _ in 0..5 {
            tx.send(event.clone()).unwrap();
        }

        // Start the connector with the already-lagged receiver.
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap());

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

        let (tx, rx) = broadcast::channel::<MeterBundleResponse>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap());

        tx.send(response_with_results()).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies exactly 1 call was made.
    }

    #[tokio::test]
    async fn test_builder_connector_skips_empty_results() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(0).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeterBundleResponse>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap());

        // Default response has empty results — should be skipped.
        tx.send(MeterBundleResponse::default()).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        // wiremock verifies 0 calls were made.
    }

    #[tokio::test]
    async fn test_builder_connector_shuts_down_on_channel_close() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST")).respond_with(jsonrpc_ok()).expect(1).mount(&mock_server).await;

        let (tx, rx) = broadcast::channel::<MeterBundleResponse>(16);
        BuilderConnector::connect(rx, mock_server.uri().parse().unwrap());

        // Send one message, then close the channel.
        tx.send(response_with_results()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(tx);

        // The task should exit gracefully without panic.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
