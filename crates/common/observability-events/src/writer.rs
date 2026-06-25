use std::{
    fmt,
    fs::{File, OpenOptions, create_dir_all},
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tracing::warn;
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};

use crate::{
    DEFAULT_QUEUE_CAPACITY, Metrics, TransactionEvent, TransactionEventProducer,
    TransactionEventValidationError,
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
            required: false,
            producer,
            network: network.into(),
        }
    }
}

/// Non-blocking handle for appending transaction events to JSONL.
#[derive(Clone)]
pub struct TransactionEventWriter {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    writer: Option<NonBlocking>,
    dropped: Option<ErrorCounter>,
    observed_drops: AtomicUsize,
    _guard: Option<WorkerGuard>,
    config: TransactionEventWriterConfig,
}

impl fmt::Debug for TransactionEventWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransactionEventWriter")
            .field("enabled", &self.inner.writer.is_some())
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct MetricWriter<W> {
    inner: W,
}

impl<W> MetricWriter<W> {
    const fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W> Write for MetricWriter<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.write(buf) {
            Ok(bytes) => {
                Metrics::bytes_written().increment(bytes as u64);
                Ok(bytes)
            }
            Err(err) => {
                Metrics::write_errors("write").increment(1);
                Err(err)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.inner.flush() {
            Ok(()) => Ok(()),
            Err(err) => {
                Metrics::write_errors("flush").increment(1);
                Err(err)
            }
        }
    }
}

impl TransactionEventWriter {
    /// Opens the configured file and starts the background writer task.
    ///
    /// If the writer is disabled, returns a handle that drops events with a
    /// metric. If initialization fails and `required = false`, returns the same
    /// disabled handle after recording the error. If `required = true`, returns
    /// the initialization error.
    pub fn from_config(config: TransactionEventWriterConfig) -> eyre::Result<Self> {
        if !config.enabled {
            return Ok(Self::disabled(config));
        }

        let file = open_file(&config);

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
        let (writer, guard) = NonBlockingBuilder::default()
            .lossy(true)
            .buffered_lines_limit(queue_capacity)
            .thread_name("transaction-event-writer")
            .finish(MetricWriter::new(file));
        let dropped = writer.error_counter();

        Ok(Self {
            inner: Arc::new(WriterInner {
                writer: Some(writer),
                dropped: Some(dropped),
                observed_drops: AtomicUsize::new(0),
                _guard: Some(guard),
                config,
            }),
        })
    }

    /// Creates a disabled writer handle.
    pub fn disabled(config: TransactionEventWriterConfig) -> Self {
        Self {
            inner: Arc::new(WriterInner {
                writer: None,
                dropped: None,
                observed_drops: AtomicUsize::new(0),
                _guard: None,
                config,
            }),
        }
    }

    /// Attempts to enqueue one event without blocking the caller.
    pub fn try_write(&self, event: &TransactionEvent) -> Result<(), WriteEventError> {
        let Some(writer) = &self.inner.writer else {
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

        let _ = writer.clone().write_all(&line);
        self.observe_dropped_events();
        Metrics::submitted_events().increment(1);
        Ok(())
    }

    /// Returns the configured network label for this writer.
    pub fn network(&self) -> &str {
        &self.inner.config.network
    }

    fn observe_dropped_events(&self) -> usize {
        let Some(dropped) = &self.inner.dropped else {
            return 0;
        };

        loop {
            let current = dropped.dropped_lines();
            let previous = self.inner.observed_drops.load(Ordering::Relaxed);
            if current <= previous {
                return 0;
            }

            if self
                .inner
                .observed_drops
                .compare_exchange_weak(previous, current, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let delta = current - previous;
                Metrics::dropped_events("backpressure").increment(delta as u64);
                return delta;
            }
        }
    }
}

/// Error returned when an event cannot be queued.
#[derive(Debug, thiserror::Error)]
pub enum WriteEventError {
    /// Writer is disabled.
    #[error("transaction event writer is disabled")]
    Disabled,
    /// Serialization failed.
    #[error("failed to serialize transaction event: {0}")]
    Serialize(serde_json::Error),
    /// Event failed contract validation.
    #[error("invalid transaction event: {0}")]
    Invalid(TransactionEventValidationError),
}

fn open_file(config: &TransactionEventWriterConfig) -> io::Result<File> {
    if let Some(parent) = config.file_path.parent() {
        create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(&config.file_path)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, ErrorKind},
        thread,
        time::Duration,
    };

    use alloy_primitives::TxHash;
    use chrono::{DateTime, Utc};
    use serde_json::{Map, Value, json};

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

    fn writer_with_sink<W>(sink: W, queue_capacity: usize) -> TransactionEventWriter
    where
        W: Write + Send + 'static,
    {
        let config = TransactionEventWriterConfig {
            enabled: true,
            file_path: PathBuf::from("test.jsonl"),
            queue_capacity,
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        };
        let (writer, guard) = NonBlockingBuilder::default()
            .lossy(true)
            .buffered_lines_limit(queue_capacity)
            .thread_name("transaction-event-writer-test")
            .finish(MetricWriter::new(sink));
        let dropped = writer.error_counter();

        TransactionEventWriter {
            inner: Arc::new(WriterInner {
                writer: Some(writer),
                dropped: Some(dropped),
                observed_drops: AtomicUsize::new(0),
                _guard: Some(guard),
                config,
            }),
        }
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

    #[test]
    fn writer_appends_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction-events.jsonl");
        let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path.clone(),
            queue_capacity: 8,
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .unwrap();

        writer.try_write(&sample_event()).unwrap();
        drop(writer);

        let contents = fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let value: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
    }

    #[test]
    fn writer_observes_aggregate_backpressure_drops() {
        struct SlowWriter;

        impl Write for SlowWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                thread::sleep(Duration::from_millis(50));
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer = writer_with_sink(SlowWriter, 0);

        for _ in 0..10_000 {
            writer.try_write(&sample_event()).unwrap();
            if writer.inner.observed_drops.load(Ordering::Relaxed) > 0 {
                break;
            }
        }

        let dropped = writer.inner.observed_drops.load(Ordering::Relaxed);
        assert!(dropped > 0, "lossy writer should report aggregate drops under backpressure");
    }

    #[test]
    fn writer_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("transaction-events.jsonl");
        let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path.clone(),
            queue_capacity: 8,
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .unwrap();

        writer.try_write(&sample_event()).unwrap();
        drop(writer);

        assert!(path.exists());
    }

    #[test]
    fn required_writer_fails_closed_on_init_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transaction-events-dir");
        fs::create_dir(&path).unwrap();
        let err = TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path,
            queue_capacity: 8,
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        })
        .unwrap_err();

        assert!(err.to_string().contains("required transaction event writer"));
    }

    #[test]
    fn metric_writer_propagates_runtime_write_failure() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush failed"))
            }
        }

        let mut writer = MetricWriter::new(FailingWriter);

        let err = writer.write_all(b"{}\n").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Other);

        let err = writer.flush().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Other);
    }
}
