//! Transaction event emission helpers and process-global writer access.

use std::sync::atomic::{AtomicBool, Ordering};

use alloy_primitives::{B256, TxHash};
use chrono::Utc;
use serde_json::{Map, Value};
use tokio::sync::OnceCell;
use tracing::debug;

use crate::{
    EventIdBuilder, TransactionEvent, TransactionEventProducer, TransactionEventType,
    TransactionEventWriter, TransactionEventWriterConfig, WriteEventError,
};

static GLOBAL_TRANSACTION_EVENT_WRITER: OnceCell<TransactionEventWriter> = OnceCell::const_new();

/// Result of initializing the process-global transaction event writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalTransactionEventWriterInitStatus {
    /// A writer was initialized and registered for process-global event emission.
    Initialized,
    /// No writer config was provided, or the provided config disabled the journal.
    NotConfigured,
    /// A writer was already initialized earlier in the process.
    AlreadyInitialized,
}

/// Result of attempting to emit a transaction event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEventEmitOutcome {
    /// The event was accepted by the configured writer.
    Emitted,
    /// No process-global transaction event writer is configured.
    NotConfigured,
}

/// Process-global transaction event writer access.
#[derive(Debug)]
pub struct GlobalTransactionEventWriter;

impl GlobalTransactionEventWriter {
    /// Initializes the process-global transaction event writer.
    pub async fn init(
        config: Option<TransactionEventWriterConfig>,
    ) -> eyre::Result<GlobalTransactionEventWriterInitStatus> {
        let Some(config) = config else {
            return Ok(GlobalTransactionEventWriterInitStatus::NotConfigured);
        };
        if !config.enabled {
            return Ok(GlobalTransactionEventWriterInitStatus::NotConfigured);
        }

        if GLOBAL_TRANSACTION_EVENT_WRITER.get().is_some() {
            return Ok(GlobalTransactionEventWriterInitStatus::AlreadyInitialized);
        }

        let initialized = AtomicBool::new(false);
        GLOBAL_TRANSACTION_EVENT_WRITER
            .get_or_try_init(|| async {
                initialized.store(true, Ordering::Relaxed);
                TransactionEventWriter::from_config(config).await
            })
            .await?;

        if initialized.load(Ordering::Relaxed) {
            Ok(GlobalTransactionEventWriterInitStatus::Initialized)
        } else {
            Ok(GlobalTransactionEventWriterInitStatus::AlreadyInitialized)
        }
    }

    /// Returns the process-global transaction event writer, if configured.
    pub fn get() -> Option<&'static TransactionEventWriter> {
        GLOBAL_TRANSACTION_EVENT_WRITER.get()
    }
}

/// Builder for emitting a transaction event through an optional writer.
#[derive(Debug, Clone)]
pub struct TransactionEventBuilder {
    producer: TransactionEventProducer,
    event_type: TransactionEventType,
    event_id: EventIdBuilder,
    tx_hash: Option<TxHash>,
    block_hash: Option<B256>,
    block_number: Option<u64>,
    payload_id: Option<String>,
    request_id: Option<String>,
    data: Map<String, Value>,
}

impl TransactionEventBuilder {
    /// Creates a builder with required event identity fields.
    pub fn new(producer: TransactionEventProducer, event_type: TransactionEventType) -> Self {
        Self {
            producer,
            event_type,
            event_id: EventIdBuilder::new()
                .part("producer", producer)
                .part("event_type", event_type),
            tx_hash: None,
            block_hash: None,
            block_number: None,
            payload_id: None,
            request_id: None,
            data: Map::new(),
        }
    }

    /// Adds a producer-specific event ID component.
    pub fn id_part(mut self, name: &str, value: impl std::fmt::Display) -> Self {
        self.event_id = self.event_id.part(name, value);
        self
    }

    /// Sets the transaction hash join key and includes it in the event ID.
    pub fn tx_hash(mut self, tx_hash: TxHash) -> Self {
        self.event_id = self.event_id.part("tx_hash", tx_hash);
        self.tx_hash = Some(tx_hash);
        self
    }

    /// Sets the transaction hash join key when present.
    pub fn maybe_tx_hash(self, tx_hash: Option<TxHash>) -> Self {
        match tx_hash {
            Some(tx_hash) => self.tx_hash(tx_hash),
            None => self,
        }
    }

    /// Sets the block hash join key and includes it in the event ID.
    pub fn block_hash(mut self, block_hash: B256) -> Self {
        self.event_id = self.event_id.part("block_hash", block_hash);
        self.block_hash = Some(block_hash);
        self
    }

    /// Sets the block hash join key when present.
    pub fn maybe_block_hash(self, block_hash: Option<B256>) -> Self {
        match block_hash {
            Some(block_hash) => self.block_hash(block_hash),
            None => self,
        }
    }

    /// Sets the block number join key and includes it in the event ID.
    pub fn block_number(mut self, block_number: u64) -> Self {
        self.event_id = self.event_id.part("block_number", block_number);
        self.block_number = Some(block_number);
        self
    }

    /// Sets the block number join key when present.
    pub fn maybe_block_number(self, block_number: Option<u64>) -> Self {
        match block_number {
            Some(block_number) => self.block_number(block_number),
            None => self,
        }
    }

    /// Sets the payload ID join key and includes it in the event ID.
    pub fn payload_id(mut self, payload_id: impl Into<String>) -> Self {
        let payload_id = payload_id.into();
        self.event_id = self.event_id.part("payload_id", &payload_id);
        self.payload_id = Some(payload_id);
        self
    }

    /// Sets the request ID join key and includes it in the event ID.
    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        let request_id = request_id.into();
        self.event_id = self.event_id.part("request_id", &request_id);
        self.request_id = Some(request_id);
        self
    }

    /// Replaces producer-specific event data.
    pub fn data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }

    /// Adds one producer-specific event data field.
    pub fn data_field(mut self, name: impl Into<String>, value: Value) -> Self {
        self.data.insert(name.into(), value);
        self
    }

    /// Builds an event for the given network label without writing it.
    pub fn build_with_network(self, network: &str) -> TransactionEvent {
        let mut event = TransactionEvent::new(
            self.event_id.finish(),
            Utc::now(),
            self.producer,
            self.event_type,
        )
        .with_network(network)
        .with_data(self.data);

        event.tx_hash = self.tx_hash;
        event.block_hash = self.block_hash;
        event.block_number = self.block_number;
        event.payload_id = self.payload_id;
        event.request_id = self.request_id;
        event
    }

    /// Builds an event for the given writer without writing it.
    pub fn build(self, writer: &TransactionEventWriter) -> TransactionEvent {
        self.build_with_network(writer.network())
    }

    /// Emits the event through an explicit writer.
    pub fn emit_to(
        self,
        writer: &TransactionEventWriter,
    ) -> Result<TransactionEventEmitOutcome, WriteEventError> {
        let event = self.build(writer);
        let result = writer.try_write(&event);
        if let Err(err) = &result {
            debug!(error = %err, event_type = %event.event_type, "transaction event not written");
        }
        result.map(|()| TransactionEventEmitOutcome::Emitted)
    }

    /// Emits the event through the process-global writer, if configured.
    pub fn emit_global(self) -> Result<TransactionEventEmitOutcome, WriteEventError> {
        let Some(writer) = GlobalTransactionEventWriter::get() else {
            return Ok(TransactionEventEmitOutcome::NotConfigured);
        };

        self.emit_to(writer)
    }
}

/// Emits a transaction event through the process-global writer.
#[macro_export]
macro_rules! transaction_event {
    (
        producer: $producer:expr,
        event_type: $event_type:expr
        $(, tx_hash: $tx_hash:expr)?
        $(, maybe_tx_hash: $maybe_tx_hash:expr)?
        $(, block_hash: $block_hash:expr)?
        $(, maybe_block_hash: $maybe_block_hash:expr)?
        $(, block_number: $block_number:expr)?
        $(, maybe_block_number: $maybe_block_number:expr)?
        $(, payload_id: $payload_id:expr)?
        $(, request_id: $request_id:expr)?
        $(, id: { $( $id_name:expr => $id_value:expr ),* $(,)? })?
        $(, data: { $( $data_name:expr => $data_value:expr ),* $(,)? })?
        $(,)?
    ) => {{
        let builder = $crate::TransactionEventBuilder::new($producer, $event_type)
            $(.tx_hash($tx_hash))?
            $(.maybe_tx_hash($maybe_tx_hash))?
            $(.block_hash($block_hash))?
            $(.maybe_block_hash($maybe_block_hash))?
            $(.block_number($block_number))?
            $(.maybe_block_number($maybe_block_number))?
            $(.payload_id($payload_id))?
            $(.request_id($request_id))?
            $($(.id_part($id_name, $id_value))*)?
            $($(.data_field($data_name, $crate::__private::json!($data_value)))*)?;
        builder.emit_global()
    }};
    (
        producer: $producer:expr,
        event_type: $event_type:expr
        $(, tx_hash: $tx_hash:expr)?
        $(, maybe_tx_hash: $maybe_tx_hash:expr)?
        $(, block_hash: $block_hash:expr)?
        $(, maybe_block_hash: $maybe_block_hash:expr)?
        $(, block_number: $block_number:expr)?
        $(, maybe_block_number: $maybe_block_number:expr)?
        $(, payload_id: $payload_id:expr)?
        $(, request_id: $request_id:expr)?
        $(, id: { $( $id_name:expr => $id_value:expr ),* $(,)? })?
        , data: $data:expr
        $(,)?
    ) => {{
        let builder = $crate::TransactionEventBuilder::new($producer, $event_type)
            $(.tx_hash($tx_hash))?
            $(.maybe_tx_hash($maybe_tx_hash))?
            $(.block_hash($block_hash))?
            $(.maybe_block_hash($maybe_block_hash))?
            $(.block_number($block_number))?
            $(.maybe_block_number($maybe_block_number))?
            $(.payload_id($payload_id))?
            $(.request_id($request_id))?
            $($(.id_part($id_name, $id_value))*)?
            .data($data);
        builder.emit_global()
    }};
    (
        writer: $writer:expr,
        producer: $producer:expr,
        event_type: $event_type:expr
        $(, tx_hash: $tx_hash:expr)?
        $(, maybe_tx_hash: $maybe_tx_hash:expr)?
        $(, block_hash: $block_hash:expr)?
        $(, maybe_block_hash: $maybe_block_hash:expr)?
        $(, block_number: $block_number:expr)?
        $(, maybe_block_number: $maybe_block_number:expr)?
        $(, payload_id: $payload_id:expr)?
        $(, request_id: $request_id:expr)?
        $(, id: { $( $id_name:expr => $id_value:expr ),* $(,)? })?
        $(, data: { $( $data_name:expr => $data_value:expr ),* $(,)? })?
        $(,)?
    ) => {{
        let builder = $crate::TransactionEventBuilder::new($producer, $event_type)
            $(.tx_hash($tx_hash))?
            $(.maybe_tx_hash($maybe_tx_hash))?
            $(.block_hash($block_hash))?
            $(.maybe_block_hash($maybe_block_hash))?
            $(.block_number($block_number))?
            $(.maybe_block_number($maybe_block_number))?
            $(.payload_id($payload_id))?
            $(.request_id($request_id))?
            $($(.id_part($id_name, $id_value))*)?
            $($(.data_field($data_name, $crate::__private::json!($data_value)))*)?;
        match $writer {
            Some(writer) => builder.emit_to(writer),
            None => Ok($crate::TransactionEventEmitOutcome::NotConfigured),
        }
    }};
    (
        writer: $writer:expr,
        producer: $producer:expr,
        event_type: $event_type:expr
        $(, tx_hash: $tx_hash:expr)?
        $(, maybe_tx_hash: $maybe_tx_hash:expr)?
        $(, block_hash: $block_hash:expr)?
        $(, maybe_block_hash: $maybe_block_hash:expr)?
        $(, block_number: $block_number:expr)?
        $(, maybe_block_number: $maybe_block_number:expr)?
        $(, payload_id: $payload_id:expr)?
        $(, request_id: $request_id:expr)?
        $(, id: { $( $id_name:expr => $id_value:expr ),* $(,)? })?
        , data: $data:expr
        $(,)?
    ) => {{
        let builder = $crate::TransactionEventBuilder::new($producer, $event_type)
            $(.tx_hash($tx_hash))?
            $(.maybe_tx_hash($maybe_tx_hash))?
            $(.block_hash($block_hash))?
            $(.maybe_block_hash($maybe_block_hash))?
            $(.block_number($block_number))?
            $(.maybe_block_number($maybe_block_number))?
            $(.payload_id($payload_id))?
            $(.request_id($request_id))?
            $($(.id_part($id_name, $id_value))*)?
            .data($data);
        match $writer {
            Some(writer) => builder.emit_to(writer),
            None => Ok($crate::TransactionEventEmitOutcome::NotConfigured),
        }
    }};
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use alloy_primitives::{B256, TxHash};
    use serde_json::{Map, json};

    use crate::{
        DEFAULT_QUEUE_CAPACITY, TransactionEventBuilder, TransactionEventEmitOutcome,
        TransactionEventProducer, TransactionEventType, TransactionEventWriter,
        TransactionEventWriterConfig,
    };

    fn disabled_writer() -> TransactionEventWriter {
        TransactionEventWriter::disabled(TransactionEventWriterConfig {
            enabled: false,
            file_path: PathBuf::from("/tmp/transaction-events.jsonl"),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            flush_interval: Duration::from_secs(1),
            required: false,
            producer: TransactionEventProducer::BaseRethNode,
            network: "base-devnet".to_string(),
        })
    }

    #[test]
    fn builder_populates_event_envelope_from_writer() {
        let writer = disabled_writer();
        let tx_hash = TxHash::repeat_byte(0x11);
        let block_hash = B256::repeat_byte(0x22);

        let event = TransactionEventBuilder::new(
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::Pending,
        )
        .tx_hash(tx_hash)
        .block_hash(block_hash)
        .block_number(42)
        .payload_id("0xabc")
        .request_id("request-1")
        .id_part("event_index", 7)
        .data_field("event_index", json!(7))
        .build(&writer);

        assert_eq!(event.producer, TransactionEventProducer::BaseRethNode);
        assert_eq!(event.event_type, TransactionEventType::Pending);
        assert_eq!(event.network.as_deref(), Some("base-devnet"));
        assert_eq!(event.tx_hash, Some(tx_hash));
        assert_eq!(event.block_hash, Some(block_hash));
        assert_eq!(event.block_number, Some(42));
        assert_eq!(event.payload_id.as_deref(), Some("0xabc"));
        assert_eq!(event.request_id.as_deref(), Some("request-1"));
        assert_eq!(event.data["event_index"], json!(7));
        assert!(event.event_id.starts_with("0x"));
    }

    #[test]
    fn builder_event_id_changes_when_id_part_changes() {
        let writer = disabled_writer();
        let first = TransactionEventBuilder::new(
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::Pending,
        )
        .tx_hash(TxHash::repeat_byte(0x11))
        .id_part("event_index", 1)
        .build(&writer);
        let second = TransactionEventBuilder::new(
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::Pending,
        )
        .tx_hash(TxHash::repeat_byte(0x11))
        .id_part("event_index", 2)
        .build(&writer);

        assert_ne!(first.event_id, second.event_id);
    }

    #[test]
    fn builder_none_writer_is_noop() {
        let result = TransactionEventBuilder::new(
            TransactionEventProducer::BaseRethNode,
            TransactionEventType::Pending,
        )
        .emit_global();

        assert_eq!(result.unwrap(), TransactionEventEmitOutcome::NotConfigured);
    }

    #[test]
    fn macro_emits_inline_data_fields() {
        let writer = disabled_writer();

        let result = transaction_event!(
            writer: Some(&writer),
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::Pending,
            tx_hash: TxHash::repeat_byte(0x11),
            id: {
                "event_index" => 1,
            },
            data: {
                "source" => "txpool",
                "event_index" => 1,
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn macro_accepts_prebuilt_data_map() {
        let writer = disabled_writer();
        let data = Map::from_iter([("source".to_string(), json!("txpool"))]);

        let result = transaction_event!(
            writer: Some(&writer),
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::Pending,
            maybe_tx_hash: Some(TxHash::repeat_byte(0x11)),
            data: data,
        );

        assert!(result.is_err());
    }

    #[test]
    fn macro_noops_when_global_writer_is_not_configured() {
        let result = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::Pending,
            tx_hash: TxHash::repeat_byte(0x11),
            data: {
                "source" => "txpool",
            },
        );

        assert_eq!(result.unwrap(), TransactionEventEmitOutcome::NotConfigured);
    }
}
