//! Shared observability event contracts and durable JSONL writers.
//!
//! This crate defines the producer-facing event envelope for Base transaction
//! observability. It is intentionally separate from tracing/stdout logging:
//! producers append these business events to a dedicated JSONL file that a
//! collector sidecar can tail.

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{B256, TxHash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{OpenOptions, create_dir_all},
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::mpsc,
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, warn};

pub mod metrics;

use metrics::Metrics;

/// Current transaction event schema version.
pub const SCHEMA_VERSION: &str = "transaction-event/v1";

/// Default bounded channel capacity for the background writer.
pub const DEFAULT_QUEUE_CAPACITY: usize = 16_384;

/// Default background flush interval.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Producer identity for a transaction event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransactionEventProducer {
    /// Base reth node producers such as txpool tracing.
    #[serde(rename = "base-reth-node")]
    BaseRethNode,
    /// Base builder payload loop.
    #[serde(rename = "base-builder")]
    BaseBuilder,
    /// Ingress RPC/audit clients.
    #[serde(rename = "ingress-rpc")]
    IngressRpc,
    /// base-routing proxyd.
    #[serde(rename = "base-routing/proxyd")]
    BaseRoutingProxyd,
}

impl fmt::Display for TransactionEventProducer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::BaseRethNode => "base-reth-node",
            Self::BaseBuilder => "base-builder",
            Self::IngressRpc => "ingress-rpc",
            Self::BaseRoutingProxyd => "base-routing/proxyd",
        };
        f.write_str(value)
    }
}

/// Versioned transaction event vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum TransactionEventType {
    #[serde(rename = "PROXY_RECEIVED")]
    ProxyReceived,
    #[serde(rename = "PROXY_REJECTED")]
    ProxyRejected,
    #[serde(rename = "PROXY_VALIDATION_ACCEPTED")]
    ProxyValidationAccepted,
    #[serde(rename = "PROXY_VALIDATION_REJECTED")]
    ProxyValidationRejected,
    #[serde(rename = "PROXY_ROUTED_TO_BACKEND")]
    ProxyRoutedToBackend,
    #[serde(rename = "PROXY_BACKEND_SUCCESS")]
    ProxyBackendSuccess,
    #[serde(rename = "PROXY_BACKEND_FAILURE")]
    ProxyBackendFailure,
    #[serde(rename = "PROXY_INGRESS_RPC_ATTEMPT")]
    ProxyIngressRpcAttempt,
    #[serde(rename = "PROXY_INGRESS_RPC_SUCCESS")]
    ProxyIngressRpcSuccess,
    #[serde(rename = "PROXY_INGRESS_RPC_FAILURE")]
    ProxyIngressRpcFailure,
    #[serde(rename = "INGRESS_RECEIVED")]
    IngressReceived,
    #[serde(rename = "SIMULATION_STARTED")]
    SimulationStarted,
    #[serde(rename = "SIMULATION_SUCCEEDED")]
    SimulationSucceeded,
    #[serde(rename = "SIMULATION_FAILED")]
    SimulationFailed,
    #[serde(rename = "INGRESS_TX_FORWARD_ATTEMPT")]
    IngressTxForwardAttempt,
    #[serde(rename = "INGRESS_TX_FORWARD_SUCCESS")]
    IngressTxForwardSuccess,
    #[serde(rename = "INGRESS_TX_FORWARD_FAILURE")]
    IngressTxForwardFailure,
    #[serde(rename = "INGRESS_METERING_SEND_ATTEMPT")]
    IngressMeteringSendAttempt,
    #[serde(rename = "INGRESS_METERING_SEND_SUCCESS")]
    IngressMeteringSendSuccess,
    #[serde(rename = "INGRESS_METERING_SEND_FAILURE")]
    IngressMeteringSendFailure,
    #[serde(rename = "INGRESS_METERING_SEND_DROPPED")]
    IngressMeteringSendDropped,
    #[serde(rename = "TXPOOL_PENDING")]
    Pending,
    #[serde(rename = "TXPOOL_QUEUED")]
    Queued,
    #[serde(rename = "TXPOOL_PENDING_TO_QUEUED")]
    PendingToQueued,
    #[serde(rename = "TXPOOL_QUEUED_TO_PENDING")]
    QueuedToPending,
    #[serde(rename = "TXPOOL_DROPPED")]
    Dropped,
    #[serde(rename = "TXPOOL_REPLACED")]
    Replaced,
    #[serde(rename = "TXPOOL_TRACKING_OVERFLOWED")]
    Overflowed,
    #[serde(rename = "TXPOOL_BLOCK_INCLUDED")]
    Included,
    #[serde(rename = "TXPOOL_FLASHBLOCK_INCLUDED")]
    FlashblockIncluded,
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_ATTEMPT")]
    TxpoolBuilderForwardAttempt,
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_SUCCESS")]
    TxpoolBuilderForwardSuccess,
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_FAILURE")]
    TxpoolBuilderForwardFailure,
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_DROPPED")]
    TxpoolBuilderForwardDropped,
    #[serde(rename = "TXPOOL_VALIDATED_INSERT_ACCEPTED")]
    TxpoolValidatedInsertAccepted,
    #[serde(rename = "TXPOOL_VALIDATED_INSERT_REJECTED")]
    TxpoolValidatedInsertRejected,
    #[serde(rename = "BUILDER_CONSIDERED")]
    BuilderConsidered,
    #[serde(rename = "BUILDER_ACCEPTED")]
    BuilderAccepted,
    #[serde(rename = "BUILDER_REJECTED")]
    BuilderRejected,
    #[serde(rename = "BUILDER_INCLUDED")]
    BuilderIncluded,
    #[serde(rename = "BUILDER_PAYLOAD_FINALIZED")]
    BuilderPayloadFinalized,
}

impl fmt::Display for TransactionEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::ProxyReceived => "PROXY_RECEIVED",
            Self::ProxyRejected => "PROXY_REJECTED",
            Self::ProxyValidationAccepted => "PROXY_VALIDATION_ACCEPTED",
            Self::ProxyValidationRejected => "PROXY_VALIDATION_REJECTED",
            Self::ProxyRoutedToBackend => "PROXY_ROUTED_TO_BACKEND",
            Self::ProxyBackendSuccess => "PROXY_BACKEND_SUCCESS",
            Self::ProxyBackendFailure => "PROXY_BACKEND_FAILURE",
            Self::ProxyIngressRpcAttempt => "PROXY_INGRESS_RPC_ATTEMPT",
            Self::ProxyIngressRpcSuccess => "PROXY_INGRESS_RPC_SUCCESS",
            Self::ProxyIngressRpcFailure => "PROXY_INGRESS_RPC_FAILURE",
            Self::IngressReceived => "INGRESS_RECEIVED",
            Self::SimulationStarted => "SIMULATION_STARTED",
            Self::SimulationSucceeded => "SIMULATION_SUCCEEDED",
            Self::SimulationFailed => "SIMULATION_FAILED",
            Self::IngressTxForwardAttempt => "INGRESS_TX_FORWARD_ATTEMPT",
            Self::IngressTxForwardSuccess => "INGRESS_TX_FORWARD_SUCCESS",
            Self::IngressTxForwardFailure => "INGRESS_TX_FORWARD_FAILURE",
            Self::IngressMeteringSendAttempt => "INGRESS_METERING_SEND_ATTEMPT",
            Self::IngressMeteringSendSuccess => "INGRESS_METERING_SEND_SUCCESS",
            Self::IngressMeteringSendFailure => "INGRESS_METERING_SEND_FAILURE",
            Self::IngressMeteringSendDropped => "INGRESS_METERING_SEND_DROPPED",
            Self::Pending => "TXPOOL_PENDING",
            Self::Queued => "TXPOOL_QUEUED",
            Self::PendingToQueued => "TXPOOL_PENDING_TO_QUEUED",
            Self::QueuedToPending => "TXPOOL_QUEUED_TO_PENDING",
            Self::Dropped => "TXPOOL_DROPPED",
            Self::Replaced => "TXPOOL_REPLACED",
            Self::Overflowed => "TXPOOL_TRACKING_OVERFLOWED",
            Self::Included => "TXPOOL_BLOCK_INCLUDED",
            Self::FlashblockIncluded => "TXPOOL_FLASHBLOCK_INCLUDED",
            Self::TxpoolBuilderForwardAttempt => "TXPOOL_BUILDER_FORWARD_ATTEMPT",
            Self::TxpoolBuilderForwardSuccess => "TXPOOL_BUILDER_FORWARD_SUCCESS",
            Self::TxpoolBuilderForwardFailure => "TXPOOL_BUILDER_FORWARD_FAILURE",
            Self::TxpoolBuilderForwardDropped => "TXPOOL_BUILDER_FORWARD_DROPPED",
            Self::TxpoolValidatedInsertAccepted => "TXPOOL_VALIDATED_INSERT_ACCEPTED",
            Self::TxpoolValidatedInsertRejected => "TXPOOL_VALIDATED_INSERT_REJECTED",
            Self::BuilderConsidered => "BUILDER_CONSIDERED",
            Self::BuilderAccepted => "BUILDER_ACCEPTED",
            Self::BuilderRejected => "BUILDER_REJECTED",
            Self::BuilderIncluded => "BUILDER_INCLUDED",
            Self::BuilderPayloadFinalized => "BUILDER_PAYLOAD_FINALIZED",
        };
        f.write_str(value)
    }
}

/// Stable JSON event envelope shared by all transaction observability producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionEvent {
    /// Schema version. Must be [`SCHEMA_VERSION`] for this envelope.
    pub schema_version: String,
    /// Producer-generated event identifier used for collector/audit dedupe.
    pub event_id: String,
    /// Wall-clock time when the source observed the event.
    pub event_time: DateTime<Utc>,
    /// Producer identity.
    pub producer: TransactionEventProducer,
    /// Event kind.
    pub event_type: TransactionEventType,
    /// Network label such as `base-mainnet` or `base-sepolia`.
    pub network: Option<String>,
    /// Transaction hash join key.
    pub tx_hash: Option<TxHash>,
    /// Block hash join key.
    pub block_hash: Option<B256>,
    /// Block number join key.
    pub block_number: Option<u64>,
    /// Engine payload ID join key.
    pub payload_id: Option<String>,
    /// Request ID join key.
    pub request_id: Option<String>,
    /// Producer-specific fields. Must not contain raw tx bytes, calldata, full
    /// request bodies, API keys, secrets, or raw forwarding headers.
    pub data: Map<String, Value>,
}

impl TransactionEvent {
    /// Creates a new event with required envelope fields and empty optional data.
    pub fn new(
        event_id: impl Into<String>,
        event_time: DateTime<Utc>,
        producer: TransactionEventProducer,
        event_type: TransactionEventType,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            event_id: event_id.into(),
            event_time,
            producer,
            event_type,
            network: None,
            tx_hash: None,
            block_hash: None,
            block_number: None,
            payload_id: None,
            request_id: None,
            data: Map::new(),
        }
    }

    /// Returns true if the event contains at least one normal transaction,
    /// block, or payload join key.
    pub const fn has_join_key(&self) -> bool {
        self.tx_hash.is_some()
            || self.payload_id.is_some()
            || self.block_hash.is_some()
            || self.block_number.is_some()
    }

    /// Validates envelope fields that are not guaranteed by Rust typing.
    ///
    /// Join keys are intentionally not required here because a few event types
    /// can be emitted before a producer has decoded a hash. Producers should
    /// still include `tx_hash`, `block_hash`/`block_number`, or `payload_id`
    /// whenever one is available.
    pub fn validate(&self) -> Result<(), TransactionEventValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(TransactionEventValidationError::InvalidSchemaVersion(
                self.schema_version.clone(),
            ));
        }
        if self.event_id.trim().is_empty() {
            return Err(TransactionEventValidationError::MissingEventId);
        }
        if let Some(key) = find_forbidden_data_key(&self.data) {
            return Err(TransactionEventValidationError::ForbiddenDataKey(key));
        }
        Ok(())
    }

    /// Sets the network label.
    pub fn with_network(mut self, network: impl Into<String>) -> Self {
        self.network = Some(network.into());
        self
    }

    /// Sets the transaction hash join key.
    pub const fn with_tx_hash(mut self, tx_hash: TxHash) -> Self {
        self.tx_hash = Some(tx_hash);
        self
    }

    /// Sets the block hash join key.
    pub const fn with_block_hash(mut self, block_hash: B256) -> Self {
        self.block_hash = Some(block_hash);
        self
    }

    /// Sets the block number join key.
    pub const fn with_block_number(mut self, block_number: u64) -> Self {
        self.block_number = Some(block_number);
        self
    }

    /// Sets the payload ID join key.
    pub fn with_payload_id(mut self, payload_id: impl Into<String>) -> Self {
        self.payload_id = Some(payload_id.into());
        self
    }

    /// Sets the request ID join key.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Replaces the producer-specific data map.
    pub fn with_data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }
}

/// Validation error for transaction event envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionEventValidationError {
    /// `schema_version` did not match [`SCHEMA_VERSION`].
    InvalidSchemaVersion(String),
    /// `event_id` was empty.
    MissingEventId,
    /// `data` contained a key reserved for unsafe payloads or secrets.
    ForbiddenDataKey(String),
}

impl fmt::Display for TransactionEventValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchemaVersion(version) => {
                write!(f, "invalid transaction event schema_version {version}")
            }
            Self::MissingEventId => f.write_str("transaction event event_id is required"),
            Self::ForbiddenDataKey(key) => {
                write!(f, "transaction event data contains forbidden key {key}")
            }
        }
    }
}

impl std::error::Error for TransactionEventValidationError {}

fn find_forbidden_data_key(data: &Map<String, Value>) -> Option<String> {
    for (key, value) in data {
        if is_forbidden_data_key(key) {
            return Some(key.clone());
        }
        if let Value::Object(child) = value
            && let Some(key) = find_forbidden_data_key(child)
        {
            return Some(key);
        }
    }
    None
}

fn is_forbidden_data_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "raw_transaction"
            | "raw_tx"
            | "raw_transaction_bytes"
            | "raw_tx_bytes"
            | "calldata"
            | "request_body"
            | "body"
            | "authorization"
            | "api_key"
            | "api_keys"
            | "secret"
            | "secrets"
            | "x-forwarded-for"
            | "forwarded_headers"
            | "headers"
    )
}

/// Builder for deterministic event IDs.
#[derive(Debug, Clone)]
pub struct EventIdBuilder {
    hasher: Sha256,
}

impl EventIdBuilder {
    /// Creates an empty event ID builder.
    pub fn new() -> Self {
        Self { hasher: Sha256::new() }
    }

    /// Adds a stable component to the ID hash.
    pub fn part(mut self, name: &str, value: impl fmt::Display) -> Self {
        let value = value.to_string();
        self.hasher.update(name.as_bytes());
        self.hasher.update([0]);
        self.hasher.update(value.len().to_le_bytes());
        self.hasher.update(value.as_bytes());
        self.hasher.update([0xff]);
        self
    }

    /// Finalizes the event ID as a hex-encoded SHA-256 digest.
    pub fn finish(self) -> String {
        format!("0x{}", hex::encode(self.hasher.finalize()))
    }
}

impl Default for EventIdBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for the dedicated transaction event JSONL writer.
#[derive(Debug, Clone)]
pub struct TransactionEventWriterConfig {
    /// Enables the writer. Disabled writers drop submitted events with metrics.
    pub enabled: bool,
    /// File path to append JSONL events to.
    pub file_path: PathBuf,
    /// Bounded queue capacity before producers drop instead of blocking.
    pub queue_capacity: usize,
    /// Periodic flush interval for the background file writer.
    pub flush_interval: Duration,
    /// If true, initialization errors are returned to the caller.
    pub required: bool,
    /// Producer identity expected for events written through this handle.
    pub producer: TransactionEventProducer,
    /// Network label expected for events written through this handle.
    pub network: String,
}

impl TransactionEventWriterConfig {
    /// Creates a disabled config with conservative defaults.
    pub fn disabled(
        producer: TransactionEventProducer,
        network: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            enabled: false,
            file_path: file_path.into(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            required: false,
            producer,
            network: network.into(),
        }
    }
}

/// Non-blocking handle for appending transaction events to JSONL.
#[derive(Debug, Clone)]
pub struct TransactionEventWriter {
    inner: Arc<WriterInner>,
}

#[derive(Debug)]
struct WriterInner {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    _task: Option<Arc<JoinHandle<()>>>,
    config: TransactionEventWriterConfig,
}

impl TransactionEventWriter {
    /// Opens the configured file and starts the background writer task.
    ///
    /// If the writer is disabled, returns a handle that drops events with a
    /// metric. If initialization fails and `required = false`, returns the same
    /// disabled handle after recording the error. If `required = true`, returns
    /// the initialization error.
    pub async fn from_config(config: TransactionEventWriterConfig) -> eyre::Result<Self> {
        if !config.enabled {
            return Ok(Self::disabled(config));
        }

        let file = async {
            if let Some(parent) = config.file_path.parent() {
                create_dir_all(parent).await?;
            }
            OpenOptions::new().create(true).append(true).open(&config.file_path).await
        }
        .await;

        let file = match file {
            Ok(file) => file,
            Err(err) if config.required => {
                return Err(eyre::eyre!(
                    "failed to initialize required transaction event writer at {}: {err}",
                    config.file_path.display()
                ));
            }
            Err(err) => {
                Metrics::write_errors("write").increment(1);
                warn!(
                    path = %config.file_path.display(),
                    error = %err,
                    "transaction event writer disabled after initialization failure"
                );
                return Ok(Self::disabled(config));
            }
        };

        let queue_capacity = config.queue_capacity.max(1);
        let flush_interval = if config.flush_interval.is_zero() {
            DEFAULT_FLUSH_INTERVAL
        } else {
            config.flush_interval
        };
        let (tx, rx) = mpsc::channel(queue_capacity);
        let queued = Arc::new(AtomicUsize::new(0));
        let task_queued = Arc::clone(&queued);
        let task = tokio::spawn(async move {
            run_writer(BufWriter::new(file), rx, task_queued, flush_interval).await;
        });

        Ok(Self {
            inner: Arc::new(WriterInner {
                tx: Some(tx),
                queued,
                _task: Some(Arc::new(task)),
                config,
            }),
        })
    }

    /// Creates a disabled writer handle.
    pub fn disabled(config: TransactionEventWriterConfig) -> Self {
        Self {
            inner: Arc::new(WriterInner {
                tx: None,
                queued: Arc::new(AtomicUsize::new(0)),
                _task: None,
                config,
            }),
        }
    }

    /// Attempts to enqueue one event without blocking the caller.
    pub fn try_write(&self, event: &TransactionEvent) -> Result<(), WriteEventError> {
        if event.producer != self.inner.config.producer {
            warn!(
                expected = %self.inner.config.producer,
                actual = %event.producer,
                "transaction event producer does not match writer config"
            );
        }
        if event.network.as_deref() != Some(self.inner.config.network.as_str()) {
            warn!(
                expected = %self.inner.config.network,
                actual = ?event.network,
                "transaction event network does not match writer config"
            );
        }

        let Some(tx) = &self.inner.tx else {
            Metrics::dropped_events("disabled").increment(1);
            return Err(WriteEventError::Disabled);
        };

        event.validate().map_err(|err| {
            Metrics::dropped_events("validation").increment(1);
            WriteEventError::Invalid(err)
        })?;

        let mut line = serde_json::to_vec(event).map_err(|err| {
            Metrics::dropped_events("serialization").increment(1);
            WriteEventError::Serialize(err)
        })?;
        line.push(b'\n');

        match tx.try_send(line) {
            Ok(()) => {
                let depth = self.inner.queued.fetch_add(1, Ordering::Relaxed) + 1;
                Metrics::emitted_events().increment(1);
                Metrics::queue_depth().set(depth as f64);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Metrics::dropped_events("backpressure").increment(1);
                Err(WriteEventError::Backpressure)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Metrics::dropped_events("closed").increment(1);
                Err(WriteEventError::Closed)
            }
        }
    }

    /// Returns the approximate number of queued events.
    pub fn queue_depth(&self) -> usize {
        self.inner.queued.load(Ordering::Relaxed)
    }

    /// Returns the configured network label for this writer.
    pub fn network(&self) -> &str {
        &self.inner.config.network
    }
}

/// Error returned when an event cannot be queued.
#[derive(Debug)]
pub enum WriteEventError {
    /// Writer is disabled.
    Disabled,
    /// Bounded queue is full.
    Backpressure,
    /// Background writer task has stopped.
    Closed,
    /// Serialization failed.
    Serialize(serde_json::Error),
    /// Event failed contract validation.
    Invalid(TransactionEventValidationError),
}

impl fmt::Display for WriteEventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("transaction event writer is disabled"),
            Self::Backpressure => f.write_str("transaction event writer queue is full"),
            Self::Closed => f.write_str("transaction event writer task is closed"),
            Self::Serialize(err) => write!(f, "failed to serialize transaction event: {err}"),
            Self::Invalid(err) => write!(f, "invalid transaction event: {err}"),
        }
    }
}

impl std::error::Error for WriteEventError {}

async fn run_writer<W>(
    mut writer: BufWriter<W>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    flush_interval: Duration,
) where
    W: AsyncWrite + Unpin,
{
    let mut ticker = interval(flush_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_line = rx.recv() => {
                let Some(line) = maybe_line else {
                    if let Err(err) = writer.flush().await {
                        Metrics::write_errors("flush").increment(1);
                        error!(error = %err, "failed to flush transaction event journal on shutdown");
                    }
                    Metrics::queue_depth().set(0.0);
                    break;
                };

                queued.fetch_sub(1, Ordering::Relaxed);
                Metrics::queue_depth().set(queued.load(Ordering::Relaxed) as f64);

                let bytes = line.len();
                match writer.write_all(&line).await {
                    Ok(()) => Metrics::bytes_written().increment(bytes as u64),
                    Err(err) => {
                        Metrics::write_errors("write").increment(1);
                        error!(error = %err, "failed to write transaction event journal entry");
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(err) = writer.flush().await {
                    Metrics::write_errors("flush").increment(1);
                    error!(error = %err, "failed to flush transaction event journal");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::json;
    use tokio::io;

    use super::*;

    fn sample_event() -> TransactionEvent {
        let tx_hash = TxHash::repeat_byte(0x11);
        let event_id = EventIdBuilder::new()
            .part("producer", TransactionEventProducer::BaseRethNode)
            .part("event_type", TransactionEventType::Pending)
            .part("tx_hash", tx_hash)
            .finish();

        TransactionEvent::new(
            event_id,
            DateTime::parse_from_rfc3339("2026-06-02T00:00:00.000000000Z")
                .unwrap()
                .with_timezone(&Utc),
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::Pending,
        )
        .with_network("base-mainnet")
        .with_tx_hash(tx_hash)
        .with_data(Map::from_iter([("pool".to_string(), json!("pending"))]))
    }

    #[test]
    fn serializes_stable_envelope_shape() {
        let event = sample_event();
        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["event_time"], "2026-06-02T00:00:00Z");
        assert_eq!(value["producer"], "base-reth-node");
        assert_eq!(value["event_type"], "TXPOOL_PENDING");
        assert_eq!(value["network"], "base-mainnet");
        assert_eq!(value["tx_hash"], format!("{:#x}", TxHash::repeat_byte(0x11)));
        assert_eq!(value["data"]["pool"], "pending");
        assert!(event.has_join_key());
    }

    #[test]
    fn deserializes_contract_from_json() {
        let event: TransactionEvent = serde_json::from_value(json!({
            "schema_version": "transaction-event/v1",
            "event_id": "0xabc",
            "event_time": "2026-06-02T00:00:00.000000000Z",
            "producer": "base-builder",
            "event_type": "BUILDER_INCLUDED",
            "network": "base-mainnet",
            "tx_hash": null,
            "block_hash": "0x3333333333333333333333333333333333333333333333333333333333333333",
            "block_number": 123,
            "payload_id": "0x12345678abcdef00",
            "request_id": null,
            "data": {
                "position": 7
            }
        }))
        .unwrap();

        assert_eq!(event.schema_version, SCHEMA_VERSION);
        assert_eq!(event.producer, TransactionEventProducer::BaseBuilder);
        assert_eq!(event.event_type, TransactionEventType::BuilderIncluded);
        assert_eq!(event.payload_id.as_deref(), Some("0x12345678abcdef00"));
        assert!(event.has_join_key());
    }

    #[test]
    fn validation_rejects_bad_schema_and_empty_event_id() {
        let mut event = sample_event();
        event.schema_version = "transaction-event/v0".to_string();
        assert!(matches!(
            event.validate(),
            Err(TransactionEventValidationError::InvalidSchemaVersion(_))
        ));

        let mut event = sample_event();
        event.event_id = " ".to_string();
        assert!(matches!(event.validate(), Err(TransactionEventValidationError::MissingEventId)));
    }

    #[test]
    fn validation_rejects_forbidden_data_keys() {
        let mut event = sample_event();
        event.data = Map::from_iter([(
            "nested".to_string(),
            json!({
                "authorization": "redacted-but-still-not-allowed"
            }),
        )]);

        assert!(matches!(
            event.validate(),
            Err(TransactionEventValidationError::ForbiddenDataKey(key)) if key == "authorization"
        ));
    }

    #[test]
    fn deterministic_event_id_is_stable_and_ordered() {
        let id_a = EventIdBuilder::new()
            .part("producer", "base-reth-node")
            .part("event_type", "TXPOOL_PENDING")
            .part("tx_hash", TxHash::repeat_byte(0x22))
            .finish();
        let id_b = EventIdBuilder::new()
            .part("producer", "base-reth-node")
            .part("event_type", "TXPOOL_PENDING")
            .part("tx_hash", TxHash::repeat_byte(0x22))
            .finish();
        let id_c = EventIdBuilder::new()
            .part("event_type", "TXPOOL_PENDING")
            .part("producer", "base-reth-node")
            .part("tx_hash", TxHash::repeat_byte(0x22))
            .finish();

        assert_eq!(id_a, id_b);
        assert_ne!(id_a, id_c);
        assert!(id_a.starts_with("0x"));
    }

    #[test]
    fn example_event_does_not_leak_raw_payloads() {
        let event = sample_event();
        let line = serde_json::to_string(&event).unwrap();

        for forbidden in
            ["raw_transaction", "raw_tx", "calldata", "authorization", "x-forwarded-for"]
        {
            assert!(
                !line.to_ascii_lowercase().contains(forbidden),
                "example leaked forbidden field {forbidden}: {line}"
            );
        }
    }

    #[tokio::test]
    async fn writer_appends_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction-events.jsonl");
        let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path.clone(),
            queue_capacity: 8,
            flush_interval: Duration::from_millis(10),
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .await
        .unwrap();

        writer.try_write(&sample_event()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(writer);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let contents = fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let value: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn writer_drops_on_backpressure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction-events.jsonl");
        let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path,
            queue_capacity: 1,
            flush_interval: Duration::from_secs(30),
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .await
        .unwrap();

        writer.try_write(&sample_event()).unwrap();
        let mut saw_backpressure = false;
        for _ in 0..10_000 {
            if matches!(writer.try_write(&sample_event()), Err(WriteEventError::Backpressure)) {
                saw_backpressure = true;
                break;
            }
        }

        assert!(saw_backpressure, "bounded writer should eventually reject without blocking");
    }

    #[tokio::test]
    async fn writer_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("transaction-events.jsonl");
        let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path.clone(),
            queue_capacity: 8,
            flush_interval: Duration::from_millis(10),
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .await
        .unwrap();

        writer.try_write(&sample_event()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(path.exists());
    }

    #[tokio::test]
    async fn required_writer_fails_closed_on_init_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction-events-dir");
        fs::create_dir(&path).unwrap();
        let err = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path,
            queue_capacity: 8,
            flush_interval: Duration::from_millis(10),
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .await
        .unwrap_err();

        assert!(err.to_string().contains("required transaction event writer"));
    }

    #[tokio::test]
    async fn runtime_write_failure_does_not_close_writer() {
        struct FailingWriter;

        impl AsyncWrite for FailingWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<io::Result<usize>> {
                std::task::Poll::Ready(Err(io::Error::other("disk full")))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let (tx, rx) = mpsc::channel(2);
        let queued = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_writer(
            BufWriter::new(FailingWriter),
            rx,
            Arc::clone(&queued),
            Duration::from_millis(10),
        ));

        tx.send(Vec::from(&b"{}\n"[..])).await.unwrap();
        tx.send(Vec::from(&b"{}\n"[..])).await.unwrap();
        drop(tx);

        task.await.unwrap();
    }
}
