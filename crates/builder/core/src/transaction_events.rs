//! Builder transaction event emission.

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, TransactionEventWriter, transaction_event,
};
use serde_json::{Map, Value, json};
use tracing::warn;

use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources,
    TxnExecutionError,
};

/// Shared builder transaction event sink handle.
#[derive(Clone, Debug)]
pub struct SharedBuilderTransactionEventSink(TransactionEventWriter);

impl SharedBuilderTransactionEventSink {
    /// Wraps the configured transaction event writer.
    pub const fn new(writer: TransactionEventWriter) -> Self {
        Self(writer)
    }

    /// Returns the wrapped writer.
    pub const fn writer(&self) -> &TransactionEventWriter {
        &self.0
    }
}

impl From<TransactionEventWriter> for SharedBuilderTransactionEventSink {
    fn from(writer: TransactionEventWriter) -> Self {
        Self::new(writer)
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
pub(crate) const fn rejection_reason_code(err: &TxnExecutionError) -> &'static str {
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
    debug_assert_eq!(sink.writer().network(), ctx.network);

    match transaction_event!(
        writer: Some(sink.writer()),
        producer: TransactionEventProducer::BaseBuilder,
        event_type: event_type,
        tx_hash: tx_hash,
        maybe_block_hash: ctx.block_hash,
        block_number: ctx.block_number,
        payload_id: ctx.payload_id,
        id: {
            "flashblock_index" => ctx.flashblock_index.map(|index| index.to_string()).unwrap_or_default(),
            "ordering_position" => ctx.ordering_position.map(|position| position.to_string()).unwrap_or_default(),
        },
        data: base_data,
    ) {
        Ok(()) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Err(err) => {
            BuilderMetrics::builder_transaction_events_dropped(event_type_label, "write")
                .increment(1);
            warn!(
                target: "payload_builder",
                error = %err,
                event_type = %event_type,
                tx_hash = ?tx_hash,
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
    debug_assert_eq!(sink.writer().network(), ctx.network);

    match transaction_event!(
        writer: Some(sink.writer()),
        producer: TransactionEventProducer::BaseBuilder,
        event_type: event_type,
        maybe_block_hash: ctx.block_hash,
        block_number: ctx.block_number,
        payload_id: ctx.payload_id,
        id: {
            "flashblock_index" => ctx.flashblock_index.map(|index| index.to_string()).unwrap_or_default(),
        },
        data: base_data,
    ) {
        Ok(()) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Err(err) => {
            BuilderMetrics::builder_transaction_events_dropped(event_type_label, "write")
                .increment(1);
            warn!(
                target: "payload_builder",
                error = %err,
                event_type = %event_type,
                "failed to enqueue builder payload event"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use base_observability_events::{
        DEFAULT_QUEUE_CAPACITY, TransactionEvent, TransactionEventType,
        TransactionEventWriterConfig,
    };

    use super::*;

    async fn writer(path: PathBuf) -> TransactionEventWriter {
        TransactionEventWriter::from_config(TransactionEventWriterConfig {
            enabled: true,
            file_path: path,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            flush_interval: Duration::from_millis(10),
            required: true,
            producer: TransactionEventProducer::BaseBuilder,
            network: "base-sepolia".to_string(),
        })
        .await
        .unwrap()
    }

    async fn read_events(path: PathBuf, writer: TransactionEventWriter) -> Vec<TransactionEvent> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(writer);
        tokio::time::sleep(Duration::from_millis(20)).await;

        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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

    #[tokio::test]
    async fn emits_builder_decision_event_with_safe_context_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let writer = writer(path.clone()).await;
        let shared = SharedBuilderTransactionEventSink::new(writer.clone());
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

        let events = read_events(path, writer).await;
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

    #[tokio::test]
    async fn emits_builder_payload_event_with_block_join_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let writer = writer(path.clone()).await;
        let shared = SharedBuilderTransactionEventSink::new(writer.clone());
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

        let events = read_events(path, writer).await;
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

    #[tokio::test]
    async fn payload_event_id_includes_flashblock_index_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let writer = writer(path.clone()).await;
        let shared = SharedBuilderTransactionEventSink::new(writer.clone());
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

        let events = read_events(path, writer).await;
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
