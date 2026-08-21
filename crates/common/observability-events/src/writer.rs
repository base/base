use std::{
    fmt,
    fs::{File, OpenOptions, create_dir_all, read_dir, remove_file, rename},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::warn;
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};

use crate::{
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_FILES, DEFAULT_QUEUE_CAPACITY, Metrics, TransactionEvent,
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
    /// Maximum size of one JSONL segment before it is rotated.
    pub max_file_bytes: u64,
    /// Maximum number of JSONL segments to retain, including the active file.
    pub max_files: usize,
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
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            required: false,
            producer,
            network: network.into(),
        }
    }
}

/// Shared buffer for events written by an in-memory [`TransactionEventWriter`].
#[derive(Clone, Debug, Default)]
pub struct TransactionEventRecorder {
    events: Arc<Mutex<Vec<TransactionEvent>>>,
}

impl TransactionEventRecorder {
    /// Creates an empty recorder.
    pub fn new() -> Self {
        Self { events: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Returns a snapshot of recorded events.
    pub fn events(&self) -> Vec<TransactionEvent> {
        self.events.lock().unwrap_or_else(|err| err.into_inner()).clone()
    }

    /// Removes all recorded events.
    pub fn clear(&self) {
        self.events.lock().unwrap_or_else(|err| err.into_inner()).clear();
    }

    fn push(&self, event: TransactionEvent) {
        self.events.lock().unwrap_or_else(|err| err.into_inner()).push(event);
    }
}

/// Non-blocking handle for appending transaction events to JSONL.
#[derive(Clone)]
pub struct TransactionEventWriter {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    backend: WriterBackend,
    network: String,
}

enum WriterBackend {
    Disabled,
    File {
        writer: NonBlocking,
        dropped: ErrorCounter,
        observed_drops: AtomicUsize,
        _guard: WorkerGuard,
    },
    Memory {
        recorder: TransactionEventRecorder,
    },
}

impl fmt::Debug for TransactionEventWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match &self.inner.backend {
            WriterBackend::Disabled => "disabled",
            WriterBackend::File { .. } => "file",
            WriterBackend::Memory { .. } => "memory",
        };
        f.debug_struct("TransactionEventWriter")
            .field("backend", &backend)
            .field("network", &self.inner.network)
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

        let file = SizeRollingFile::new(&config);

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

        Ok(Self::new(
            WriterBackend::File {
                writer,
                dropped,
                observed_drops: AtomicUsize::new(0),
                _guard: guard,
            },
            config.network,
        ))
    }

    /// Creates a disabled writer handle.
    pub fn disabled(config: TransactionEventWriterConfig) -> Self {
        Self::new(WriterBackend::Disabled, config.network)
    }

    /// Creates an in-memory writer that appends events to `recorder`.
    pub fn in_memory(network: impl Into<String>, recorder: TransactionEventRecorder) -> Self {
        Self::new(WriterBackend::Memory { recorder }, network)
    }

    fn new(backend: WriterBackend, network: impl Into<String>) -> Self {
        Self { inner: Arc::new(WriterInner { backend, network: network.into() }) }
    }

    /// Attempts to enqueue one event without blocking the caller.
    pub fn try_write(&self, event: &TransactionEvent) -> Result<(), WriteEventError> {
        match &self.inner.backend {
            WriterBackend::Disabled => {
                Metrics::dropped_events("disabled").increment(1);
                Err(WriteEventError::Disabled)
            }
            WriterBackend::Memory { recorder } => {
                Self::validate_event(event)?;
                recorder.push(event.clone());
                Metrics::submitted_events().increment(1);
                Ok(())
            }
            WriterBackend::File { writer, .. } => {
                Self::validate_event(event)?;
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
        }
    }

    fn validate_event(event: &TransactionEvent) -> Result<(), WriteEventError> {
        event.validate().map_err(|err| {
            Metrics::dropped_events("validation").increment(1);
            WriteEventError::Invalid(err)
        })
    }

    /// Returns the configured network label for this writer.
    pub fn network(&self) -> &str {
        &self.inner.network
    }

    fn observe_dropped_events(&self) -> usize {
        let WriterBackend::File { dropped, observed_drops, .. } = &self.inner.backend else {
            return 0;
        };

        loop {
            let current = dropped.dropped_lines();
            let previous = observed_drops.load(Ordering::Relaxed);
            if current <= previous {
                return 0;
            }

            if observed_drops
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

const MAX_ROTATED_PATH_ATTEMPTS: u32 = 1000;

struct SizeRollingFile {
    file: Option<File>,
    path: PathBuf,
    current_size: u64,
    max_file_bytes: u64,
    max_files: usize,
}

impl SizeRollingFile {
    fn new(config: &TransactionEventWriterConfig) -> io::Result<Self> {
        let file = open_file(&config.file_path)?;
        let current_size = file.metadata()?.len();

        Ok(Self {
            file: Some(file),
            path: config.file_path.clone(),
            current_size,
            max_file_bytes: config.max_file_bytes.max(1),
            max_files: config.max_files.max(1),
        })
    }

    const fn should_rotate(&self, incoming_bytes: usize) -> bool {
        self.current_size > 0
            && self.current_size.saturating_add(incoming_bytes as u64) > self.max_file_bytes
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut file = self.file.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "transaction event file is closed")
        })?;
        if let Err(err) = file.flush() {
            self.file = Some(file);
            return Err(err);
        }
        drop(file);

        let rotated_path = match self.next_rotated_path() {
            Ok(path) => path,
            Err(err) => {
                self.file = open_file(&self.path).ok();
                return Err(err);
            }
        };
        if let Err(err) = rename(&self.path, &rotated_path) {
            self.file = open_file(&self.path).ok();
            return Err(err);
        }

        match open_file(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                self.current_size = 0;
                self.prune_rotated_files();
                Ok(())
            }
            Err(err) => {
                let _ = rename(&rotated_path, &self.path);
                self.file = open_file(&self.path).ok();
                Err(err)
            }
        }
    }

    fn next_rotated_path(&self) -> io::Result<PathBuf> {
        let (parent, stem, extension) = file_name_parts(&self.path)?;
        let mut timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();

        for _ in 0..MAX_ROTATED_PATH_ATTEMPTS {
            let candidate = parent.join(format!("{stem}.{timestamp}.{extension}"));
            if !candidate.exists() {
                return Ok(candidate);
            }
            timestamp = timestamp.saturating_add(1);
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique rotated transaction event path",
        ))
    }

    fn prune_rotated_files(&self) {
        let Ok((parent, stem, extension)) = file_name_parts(&self.path) else {
            return;
        };
        let prefix = format!("{stem}.");
        let suffix = format!(".{extension}");
        let Ok(entries) = read_dir(parent) else {
            return;
        };
        let mut rotated_files = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let timestamp =
                    name.strip_prefix(&prefix)?.strip_suffix(&suffix)?.parse::<u128>().ok()?;
                Some((timestamp, name, entry.path()))
            })
            .collect::<Vec<_>>();

        rotated_files
            .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let keep_rotated = self.max_files.saturating_sub(1);
        while rotated_files.len() > keep_rotated {
            let (_, _, path) = rotated_files.remove(0);
            if let Err(err) = remove_file(&path) {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to prune rotated transaction event file"
                );
            }
        }
    }
}

impl Write for SizeRollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.should_rotate(buf.len()) {
            self.rotate()?;
        }

        let file = self.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "transaction event file is closed")
        })?;
        let written = file.write(buf)?;
        self.current_size = self.current_size.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "transaction event file is closed")
            })?
            .flush()
    }
}

fn open_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn file_name_parts(path: &Path) -> io::Result<(&Path, String, String)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|name| name.to_str()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "transaction event path has no valid stem")
    })?;
    let extension = path.extension().and_then(|name| name.to_str()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("transaction event path has no extension: {}", path.display()),
        )
    })?;
    Ok((parent, stem.to_string(), extension.to_string()))
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
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
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

        TransactionEventWriter::new(
            WriterBackend::File {
                writer,
                dropped,
                observed_drops: AtomicUsize::new(0),
                _guard: guard,
            },
            config.network,
        )
    }

    #[test]
    fn in_memory_writer_records_and_clears_events() {
        let recorder = TransactionEventRecorder::new();
        let writer = TransactionEventWriter::in_memory("test", recorder.clone());
        writer.try_write(&sample_event()).unwrap();
        let events = recorder.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, TransactionEventType::Pending);
        assert_eq!(events[0].network.as_deref(), Some("base-mainnet"));
        assert_eq!(writer.network(), "test");

        recorder.clear();
        assert!(recorder.events().is_empty());
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
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
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
    fn size_rolling_file_rotates_and_prunes_old_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let config = TransactionEventWriterConfig {
            enabled: true,
            file_path: path.clone(),
            queue_capacity: 8,
            max_file_bytes: 4,
            max_files: 3,
            required: true,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-mainnet".to_string(),
        };
        let mut writer = SizeRollingFile::new(&config).unwrap();

        for payload in [&b"aaaa"[..], &b"bbbb"[..], &b"cccc"[..], &b"dddd"[..], &b"eeee"[..]] {
            writer.write_all(payload).unwrap();
        }
        writer.flush().unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), 4);
        let rotated_count = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name != "events.jsonl" && name.starts_with("events.") && name.ends_with(".jsonl")
            })
            .count();
        assert_eq!(rotated_count, 2);
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
        let WriterBackend::File { observed_drops, .. } = &writer.inner.backend else {
            panic!("backpressure test requires a file-backed writer");
        };

        for _ in 0..10_000 {
            writer.try_write(&sample_event()).unwrap();
            if observed_drops.load(Ordering::Relaxed) > 0 {
                break;
            }
        }

        let dropped = observed_drops.load(Ordering::Relaxed);
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
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
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
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
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
