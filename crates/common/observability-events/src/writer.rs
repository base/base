use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    fs::{OpenOptions, create_dir_all},
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::mpsc,
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tracing::{error, warn};

use crate::{
    DEFAULT_FLUSH_INTERVAL, DEFAULT_QUEUE_CAPACITY, Metrics, TransactionEvent,
    TransactionEventProducer, TransactionEventValidationError,
};

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
    tx: Option<mpsc::Sender<QueuedEvent>>,
    queued: Arc<AtomicUsize>,
    _task: Option<JoinHandle<()>>,
    config: TransactionEventWriterConfig,
}

#[derive(Debug)]
struct QueuedEvent {
    event_id: String,
    tx_hash: Option<String>,
    line: Vec<u8>,
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
            inner: Arc::new(WriterInner { tx: Some(tx), queued, _task: Some(task), config }),
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

        let queued_event = QueuedEvent {
            event_id: event.event_id.clone(),
            tx_hash: event.tx_hash.map(|tx_hash| format!("{tx_hash:#x}")),
            line,
        };

        self.inner.queued.fetch_add(1, Ordering::Relaxed);

        match tx.try_send(queued_event) {
            Ok(()) => {
                let depth = self.inner.queued.load(Ordering::Relaxed);
                Metrics::emitted_events().increment(1);
                Metrics::queue_depth().set(depth as f64);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                let depth = decrement_queued(&self.inner.queued);
                Metrics::dropped_events("backpressure").increment(1);
                Metrics::queue_depth().set(depth as f64);
                Err(WriteEventError::Backpressure)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let depth = decrement_queued(&self.inner.queued);
                Metrics::dropped_events("closed").increment(1);
                Metrics::queue_depth().set(depth as f64);
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
#[derive(Debug, thiserror::Error)]
pub enum WriteEventError {
    /// Writer is disabled.
    #[error("transaction event writer is disabled")]
    Disabled,
    /// Bounded queue is full.
    #[error("transaction event writer queue is full")]
    Backpressure,
    /// Background writer task has stopped.
    #[error("transaction event writer task is closed")]
    Closed,
    /// Serialization failed.
    #[error("failed to serialize transaction event: {0}")]
    Serialize(serde_json::Error),
    /// Event failed contract validation.
    #[error("invalid transaction event: {0}")]
    Invalid(TransactionEventValidationError),
}

/// Drains queued events to the JSONL file.
///
/// Runtime write and flush failures are observable through metrics and logs but
/// do not block or fail transaction-serving paths. A write failure permanently
/// drops the affected queued event, and storage failures can leave a partial
/// JSONL line on disk. Collectors should tolerate and skip malformed lines.
/// Callers that require startup-time fail closed behavior should configure
/// [`TransactionEventWriterConfig::required`].
async fn run_writer<W>(
    mut writer: BufWriter<W>,
    mut rx: mpsc::Receiver<QueuedEvent>,
    queued: Arc<AtomicUsize>,
    flush_interval: Duration,
) where
    W: AsyncWrite + Unpin,
{
    let mut ticker = interval(flush_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else {
                    if let Err(err) = writer.flush().await {
                        Metrics::write_errors("flush").increment(1);
                        error!(error = %err, "failed to flush transaction event journal on shutdown");
                    }
                    Metrics::queue_depth().set(0.0);
                    break;
                };

                let depth = decrement_queued(&queued);
                Metrics::queue_depth().set(depth as f64);

                let bytes = event.line.len();
                match writer.write_all(&event.line).await {
                    Ok(()) => Metrics::bytes_written().increment(bytes as u64),
                    Err(err) => {
                        Metrics::write_errors("write").increment(1);
                        error!(
                            error = %err,
                            event_id = %event.event_id,
                            tx_hash = ?event.tx_hash,
                            "failed to write transaction event journal entry; queued event dropped"
                        );
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

fn decrement_queued(queued: &AtomicUsize) -> usize {
    queued
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_sub(1))
        .map(|previous| previous - 1)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use alloy_primitives::TxHash;
    use chrono::{DateTime, Utc};
    use serde_json::{Map, Value, json};
    use tokio::io;

    use super::*;
    use crate::{
        EventIdBuilder, SCHEMA_VERSION, TransactionEventType, TransactionEventValidationError,
    };

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
    fn validation_rejects_forbidden_data_keys_inside_arrays() {
        let mut event = sample_event();
        event.data = Map::from_iter([(
            "responses".to_string(),
            json!([
                {
                    "Authorization": "redacted-but-still-not-allowed"
                }
            ]),
        )]);

        assert!(matches!(
            event.validate(),
            Err(TransactionEventValidationError::ForbiddenDataKey(key)) if key == "Authorization"
        ));
    }

    #[test]
    fn validation_rejects_forbidden_data_keys_exactly() {
        for key in [
            "raw_transaction",
            "raw_tx",
            "calldata",
            "request_body",
            "authorization",
            "api_key",
            "headers",
        ] {
            let mut event = sample_event();
            event.data = Map::from_iter([(key.to_string(), json!("redacted"))]);

            assert!(
                matches!(
                    event.validate(),
                    Err(TransactionEventValidationError::ForbiddenDataKey(rejected)) if rejected == key
                ),
                "expected {key} to be rejected"
            );
        }
    }

    #[test]
    fn validation_allows_transaction_summary_keys() {
        let mut event = sample_event();
        event.data = Map::from_iter([
            ("tx_hash".to_string(), json!("0x1234")),
            ("payload_id".to_string(), json!("0x01")),
            ("meter_bundle_response".to_string(), json!({ "totalGasUsed": 21000 })),
            ("observability_source".to_string(), json!({ "container_name": "execution" })),
        ]);

        event.validate().unwrap();
    }

    #[test]
    fn validation_rejects_excessive_data_depth() {
        let mut value = json!("leaf");
        for _ in 0..=16 {
            value = json!({ "nested": value });
        }

        let mut event = sample_event();
        event.data = Map::from_iter([("nested".to_string(), value)]);

        assert!(matches!(event.validate(), Err(TransactionEventValidationError::DataTooDeep)));
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

        tx.send(QueuedEvent {
            event_id: "event-1".to_string(),
            tx_hash: None,
            line: Vec::from(&b"{}\n"[..]),
        })
        .await
        .unwrap();
        tx.send(QueuedEvent {
            event_id: "event-2".to_string(),
            tx_hash: None,
            line: Vec::from(&b"{}\n"[..]),
        })
        .await
        .unwrap();
        drop(tx);

        task.await.unwrap();
    }
}
