//! Builder transaction event emission.

use std::{fmt, sync::Arc};

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    EventIdBuilder, TransactionEvent, TransactionEventProducer, TransactionEventType,
    TransactionEventWriter,
};
use chrono::Utc;
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources,
    TxnExecutionError,
};

/// Non-blocking sink for builder transaction events.
pub trait BuilderTransactionEventSink: Send + Sync {
    /// Attempts to enqueue one event without blocking the payload-building path.
    fn try_write_event(&self, event: &TransactionEvent) -> Result<(), String>;
}

impl BuilderTransactionEventSink for TransactionEventWriter {
    fn try_write_event(&self, event: &TransactionEvent) -> Result<(), String> {
        self.try_write(event).map_err(|err| err.to_string())
    }
}

/// Shared builder transaction event sink handle.
#[derive(Clone)]
pub struct SharedBuilderTransactionEventSink(Arc<dyn BuilderTransactionEventSink>);

impl SharedBuilderTransactionEventSink {
    /// Wraps a concrete sink.
    pub fn new(sink: Arc<dyn BuilderTransactionEventSink>) -> Self {
        Self(sink)
    }

    /// Attempts to write an event through the wrapped sink.
    pub fn try_write_event(&self, event: &TransactionEvent) -> Result<(), String> {
        self.0.try_write_event(event)
    }
}

impl fmt::Debug for SharedBuilderTransactionEventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedBuilderTransactionEventSink(..)")
    }
}

impl From<TransactionEventWriter> for SharedBuilderTransactionEventSink {
    fn from(writer: TransactionEventWriter) -> Self {
        Self::new(Arc::new(writer))
    }
}

/// Stable decision context attached to each builder transaction event.
#[derive(Debug, Clone)]
pub(crate) struct BuilderTransactionEventContext {
    /// Network label.
    pub network: String,
    /// Payload ID join key.
    pub payload_id: String,
    /// Target block number.
    pub block_number: u64,
    /// Final block hash, once known.
    pub block_hash: Option<B256>,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Flashblock index for per-attempt events.
    pub flashblock_index: Option<u64>,
    /// Target flashblock count for the block.
    pub target_flashblock_count: u64,
    /// One-based ordering position for the transaction in this scan.
    pub ordering_position: Option<u64>,
    /// Builder mode.
    pub builder_mode: &'static str,
    /// Source queue name.
    pub source_queue: &'static str,
}

impl BuilderTransactionEventContext {
    /// Base data fields shared by all builder transaction events.
    fn base_data(&self) -> Map<String, Value> {
        let mut data = Map::from_iter([
            ("parent_hash".to_string(), json!(format!("{:#x}", self.parent_hash))),
            ("builder_mode".to_string(), json!(self.builder_mode)),
            ("source_queue".to_string(), json!(self.source_queue)),
            ("target_flashblock_count".to_string(), json!(self.target_flashblock_count)),
        ]);
        if let Some(flashblock_index) = self.flashblock_index {
            data.insert("flashblock_index".to_string(), json!(flashblock_index));
        }
        if let Some(ordering_position) = self.ordering_position {
            data.insert("ordering_position".to_string(), json!(ordering_position));
        }
        data
    }
}

/// Adds current block/flashblock budget state to an event data map.
pub(crate) fn add_budget_fields(
    data: &mut Map<String, Value>,
    info: &ExecutionInfo,
    limits: &ResourceLimits,
    resources: Option<&TxResources>,
) {
    data.extend([
        ("cumulative_gas_used".to_string(), json!(info.cumulative_gas_used)),
        ("cumulative_da_bytes_used".to_string(), json!(info.cumulative_da_bytes_used)),
        ("flashblock_execution_time_us".to_string(), json!(info.flashblock_execution_time_us)),
        ("cumulative_state_root_gas".to_string(), json!(info.cumulative_state_root_gas)),
        ("cumulative_uncompressed_bytes".to_string(), json!(info.cumulative_uncompressed_bytes)),
        ("block_gas_limit".to_string(), json!(limits.block_gas_limit)),
        ("tx_data_limit".to_string(), json!(limits.tx_data_limit)),
        ("block_data_limit".to_string(), json!(limits.block_data_limit)),
        ("block_da_footprint_limit".to_string(), json!(limits.block_da_footprint_limit)),
        ("tx_execution_time_limit_us".to_string(), json!(limits.tx_execution_time_limit_us)),
        (
            "flashblock_execution_time_limit_us".to_string(),
            json!(limits.flashblock_execution_time_limit_us),
        ),
        ("block_state_root_gas_limit".to_string(), json!(limits.block_state_root_gas_limit)),
        ("block_uncompressed_size_limit".to_string(), json!(limits.block_uncompressed_size_limit)),
    ]);

    if let Some(resources) = resources {
        data.extend([
            ("tx_da_size".to_string(), json!(resources.da_size)),
            ("tx_gas_limit".to_string(), json!(resources.gas_limit)),
            ("tx_execution_time_us".to_string(), json!(resources.execution_time_us)),
            ("tx_state_root_gas".to_string(), json!(resources.state_root_gas)),
            ("tx_uncompressed_size".to_string(), json!(resources.uncompressed_size)),
        ]);
    }
}

/// Returns a stable rejection reason code for builder transaction events.
pub(crate) fn rejection_reason_code(err: &TxnExecutionError) -> &'static str {
    match err {
        TxnExecutionError::TransactionDASizeExceeded(_, _) => "tx_da_size_exceeded",
        TxnExecutionError::BlockDASizeExceeded { .. } => "block_da_size_exceeded",
        TxnExecutionError::DAFootprintLimitExceeded { .. } => "da_footprint_limit_exceeded",
        TxnExecutionError::TransactionGasLimitExceeded { .. } => "transaction_gas_limit_exceeded",
        TxnExecutionError::BlockUncompressedSizeExceeded { .. } => {
            "block_uncompressed_size_exceeded"
        }
        TxnExecutionError::ExecutionMeteringLimitExceeded(inner) => match inner {
            ExecutionMeteringLimitExceeded::TransactionExecutionTime(_, _) => {
                "tx_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::FlashblockExecutionTime(_, _, _) => {
                "flashblock_execution_time_exceeded"
            }
            ExecutionMeteringLimitExceeded::BlockStateRootGas(_, _, _) => {
                "block_state_root_gas_exceeded"
            }
        },
        TxnExecutionError::SequencerTransaction => "sequencer_transaction",
        TxnExecutionError::NonceTooLow => "nonce_too_low",
        TxnExecutionError::InternalError(_) => "internal_error",
        TxnExecutionError::EvmError => "evm_error",
        TxnExecutionError::MaxGasUsageExceeded => "max_gas_usage_exceeded",
        TxnExecutionError::MeteringDataPending => "metering_data_pending",
    }
}

/// Emits one builder transaction event if a sink is configured.
pub(crate) fn emit_builder_transaction_event(
    sink: Option<&SharedBuilderTransactionEventSink>,
    ctx: BuilderTransactionEventContext,
    event_type: TransactionEventType,
    tx_hash: TxHash,
    mut data: Map<String, Value>,
) {
    let Some(sink) = sink else {
        return;
    };

    let event_type_label = event_type.to_string();
    let mut base_data = ctx.base_data();
    base_data.append(&mut data);

    let mut event_id = EventIdBuilder::new()
        .part("producer", TransactionEventProducer::BaseBuilder)
        .part("event_type", event_type)
        .part("payload_id", &ctx.payload_id)
        .part("block_number", ctx.block_number)
        .part("tx_hash", tx_hash);
    if let Some(block_hash) = ctx.block_hash {
        event_id = event_id.part("block_hash", block_hash);
    }
    if let Some(flashblock_index) = ctx.flashblock_index {
        event_id = event_id.part("flashblock_index", flashblock_index);
    }
    if let Some(ordering_position) = ctx.ordering_position {
        event_id = event_id.part("ordering_position", ordering_position);
    }

    let mut event = TransactionEvent::new(
        event_id.finish(),
        Utc::now(),
        TransactionEventProducer::BaseBuilder,
        event_type,
    )
    .with_network(ctx.network)
    .with_tx_hash(tx_hash)
    .with_block_number(ctx.block_number)
    .with_payload_id(ctx.payload_id)
    .with_data(base_data);

    if let Some(block_hash) = ctx.block_hash {
        event = event.with_block_hash(block_hash);
    }

    match sink.try_write_event(&event) {
        Ok(()) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Err(err) => {
            BuilderMetrics::builder_transaction_events_dropped(event_type_label, "write")
                .increment(1);
            warn!(
                target: "payload_builder",
                error = %err,
                event_type = %event.event_type,
                tx_hash = ?event.tx_hash,
                "failed to enqueue builder transaction event"
            );
        }
    }
}

/// Emits one builder payload event if a sink is configured.
pub(crate) fn emit_builder_payload_event(
    sink: Option<&SharedBuilderTransactionEventSink>,
    ctx: BuilderTransactionEventContext,
    event_type: TransactionEventType,
    mut data: Map<String, Value>,
) {
    let Some(sink) = sink else {
        return;
    };

    let event_type_label = event_type.to_string();
    let mut base_data = ctx.base_data();
    base_data.append(&mut data);

    let mut event_id = EventIdBuilder::new()
        .part("producer", TransactionEventProducer::BaseBuilder)
        .part("event_type", event_type)
        .part("payload_id", &ctx.payload_id)
        .part("block_number", ctx.block_number);
    if let Some(block_hash) = ctx.block_hash {
        event_id = event_id.part("block_hash", block_hash);
    }
    if let Some(flashblock_index) = ctx.flashblock_index {
        event_id = event_id.part("flashblock_index", flashblock_index);
    }

    let mut event = TransactionEvent::new(
        event_id.finish(),
        Utc::now(),
        TransactionEventProducer::BaseBuilder,
        event_type,
    )
    .with_network(ctx.network)
    .with_block_number(ctx.block_number)
    .with_payload_id(ctx.payload_id)
    .with_data(base_data);

    if let Some(block_hash) = ctx.block_hash {
        event = event.with_block_hash(block_hash);
    }

    match sink.try_write_event(&event) {
        Ok(()) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Err(err) => {
            BuilderMetrics::builder_transaction_events_dropped(event_type_label, "write")
                .increment(1);
            warn!(
                target: "payload_builder",
                error = %err,
                event_type = %event.event_type,
                payload_id = ?event.payload_id,
                "failed to enqueue builder payload event"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use base_observability_events::TransactionEventType;

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<TransactionEvent>>,
    }

    impl BuilderTransactionEventSink for RecordingSink {
        fn try_write_event(&self, event: &TransactionEvent) -> Result<(), String> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    fn context() -> BuilderTransactionEventContext {
        BuilderTransactionEventContext {
            network: "base-sepolia".to_string(),
            payload_id: "0x0102030405060708".to_string(),
            block_number: 10,
            block_hash: None,
            parent_hash: B256::repeat_byte(0xaa),
            flashblock_index: Some(2),
            target_flashblock_count: 8,
            ordering_position: Some(3),
            builder_mode: "flashblocks",
            source_queue: "txpool_best",
        }
    }

    #[test]
    fn emits_builder_decision_event_with_safe_context_fields() {
        let sink = Arc::new(RecordingSink::default());
        let shared = SharedBuilderTransactionEventSink::new(sink.clone());
        let tx_hash = TxHash::repeat_byte(0x11);
        let mut data = Map::new();
        add_budget_fields(
            &mut data,
            &ExecutionInfo {
                cumulative_gas_used: 21_000,
                cumulative_da_bytes_used: 120,
                flashblock_execution_time_us: 100,
                cumulative_state_root_gas: 22_000,
                cumulative_uncompressed_bytes: 110,
                ..Default::default()
            },
            &ResourceLimits {
                block_gas_limit: 30_000_000,
                block_data_limit: Some(1_000),
                ..Default::default()
            },
            Some(&TxResources {
                da_size: 120,
                gas_limit: 21_000,
                execution_time_us: Some(100),
                state_root_gas: Some(22_000),
                uncompressed_size: 110,
            }),
        );

        emit_builder_transaction_event(
            Some(&shared),
            context(),
            TransactionEventType::BuilderConsidered,
            tx_hash,
            data,
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.producer, TransactionEventProducer::BaseBuilder);
        assert_eq!(event.event_type, TransactionEventType::BuilderConsidered);
        assert_eq!(event.network.as_deref(), Some("base-sepolia"));
        assert_eq!(event.tx_hash, Some(tx_hash));
        assert_eq!(event.block_number, Some(10));
        assert_eq!(event.payload_id.as_deref(), Some("0x0102030405060708"));
        assert_eq!(event.data["builder_mode"], "flashblocks");
        assert_eq!(event.data["flashblock_index"], 2);
        assert_eq!(event.data["ordering_position"], 3);
        assert_eq!(event.data["tx_da_size"], 120);
        assert!(event.validate().is_ok());
        let serialized = serde_json::to_string(event).unwrap();
        assert!(!serialized.contains("calldata"));
        assert!(!serialized.contains("raw_tx"));
    }

    #[test]
    fn emits_builder_payload_event_with_block_join_fields() {
        let sink = Arc::new(RecordingSink::default());
        let shared = SharedBuilderTransactionEventSink::new(sink.clone());
        let mut ctx = context();
        let block_hash = B256::repeat_byte(0xbb);
        ctx.block_hash = Some(block_hash);
        ctx.flashblock_index = None;
        ctx.ordering_position = None;

        emit_builder_payload_event(
            Some(&shared),
            ctx,
            TransactionEventType::BuilderPayloadFinalized,
            Map::from_iter([("transaction_count".to_string(), json!(0))]),
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_type, TransactionEventType::BuilderPayloadFinalized);
        assert_eq!(event.block_hash, Some(block_hash));
        assert_eq!(event.block_number, Some(10));
        assert_eq!(event.payload_id.as_deref(), Some("0x0102030405060708"));
        assert_eq!(event.data["parent_hash"], format!("{:#x}", B256::repeat_byte(0xaa)));
        assert_eq!(event.data["transaction_count"], 0);
        assert!(event.tx_hash.is_none());
        assert!(event.validate().is_ok());
    }

    #[test]
    fn payload_event_id_includes_flashblock_index_when_present() {
        let sink = Arc::new(RecordingSink::default());
        let shared = SharedBuilderTransactionEventSink::new(sink.clone());
        let mut first = context();
        first.flashblock_index = Some(1);
        let mut second = context();
        second.flashblock_index = Some(2);

        emit_builder_payload_event(
            Some(&shared),
            first,
            TransactionEventType::BuilderFlashblockStarted,
            Map::new(),
        );
        emit_builder_payload_event(
            Some(&shared),
            second,
            TransactionEventType::BuilderFlashblockStarted,
            Map::new(),
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_ne!(events[0].event_id, events[1].event_id);
        assert_eq!(events[0].data["flashblock_index"], 1);
        assert_eq!(events[1].data["flashblock_index"], 2);
    }

    #[test]
    fn rejection_reason_codes_are_stable() {
        assert_eq!(
            rejection_reason_code(&TxnExecutionError::TransactionGasLimitExceeded {
                cumulative_gas_used: 1,
                tx_gas_limit: 2,
                block_gas_limit: 3,
            }),
            "transaction_gas_limit_exceeded"
        );
        assert_eq!(
            rejection_reason_code(&TxnExecutionError::ExecutionMeteringLimitExceeded(
                ExecutionMeteringLimitExceeded::FlashblockExecutionTime(1, 2, 3),
            )),
            "flashblock_execution_time_exceeded"
        );
    }
}
