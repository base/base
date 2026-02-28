#![doc = include_str!("../README.md")]

/// Health check HTTP server.
mod health;
pub use health::bind_health_server;

/// Prometheus metrics for the ingress RPC service.
mod metrics;
pub use metrics::{Metrics, record_histogram};

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
use tokio::sync::broadcast;
use tracing::{error, warn};
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
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the RPC server to.
    pub address: IpAddr,
    /// Port to bind the RPC server to.
    pub port: u16,
    /// URL of the mempool service to proxy transactions to.
    pub mempool_url: Url,
    /// Method to submit transactions to the mempool.
    pub tx_submission_method: TxSubmissionMethod,
    /// Kafka properties file path for ingress events.
    pub ingress_kafka_properties: String,
    /// Kafka topic for queuing transactions before the DB Writer.
    pub ingress_topic: String,
    /// Kafka properties file path for audit events.
    pub audit_kafka_properties: String,
    /// Kafka topic for audit events.
    pub audit_topic: String,
    /// Default lifetime for sent transactions in seconds.
    pub send_transaction_default_lifetime_seconds: u64,
    /// URL of the simulation RPC service for bundle metering.
    pub simulation_rpc: Url,
    /// Block time in milliseconds.
    pub block_time_milliseconds: u64,
    /// Timeout for bundle metering in milliseconds.
    pub meter_bundle_timeout_ms: u64,
    /// URLs of the builder RPC service for setting metering information.
    pub builder_rpcs: Vec<Url>,
    /// Maximum number of `MeterBundleResponse`s to buffer in memory.
    pub max_buffered_meter_bundle_responses: usize,
    /// Address to bind the health check server to.
    pub health_check_addr: SocketAddr,
    /// Chain ID.
    pub chain_id: u64,
    /// URL of third-party RPC endpoint to forward raw transactions to.
    pub raw_tx_forward_rpc: Option<Url>,
    /// TTL for bundle cache in seconds.
    pub bundle_cache_ttl: u64,
    /// Enable sending to builder.
    pub send_to_builder: bool,
}

/// Connects ingress metering data to builder RPCs.
#[derive(Debug)]
pub struct BuilderConnector;

impl BuilderConnector {
    /// Spawns a background task that forwards metering data to the builder RPC.
    pub fn connect(metering_rx: broadcast::Receiver<MeterBundleResponse>, builder_rpc: Url) {
        let builder: RootProvider<Base> = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(builder_rpc);

        tokio::spawn(async move {
            let mut event_rx = metering_rx;
            while let Ok(event) = event_rx.recv().await {
                if event.results.is_empty() {
                    warn!(message = "received metering information with no transactions", hash=%event.bundle_hash);
                    continue;
                }

                let tx_hash = event.results[0].tx_hash;
                if let Err(e) = builder
                    .client()
                    .request::<(TxHash, MeterBundleResponse), ()>(
                        "base_setMeteringInformation",
                        (tx_hash, event),
                    )
                    .await
                {
                    error!(error = %e, "Failed to set metering information for tx hash: {tx_hash}");
                }
            }
        });
    }
}
