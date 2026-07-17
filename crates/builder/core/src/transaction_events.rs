//! Builder transaction event emission.

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventEmitOutcome, TransactionEventProducer,
    TransactionEventType, transaction_event,
};
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::warn;

use crate::{
    BuilderMetrics, ExecutionInfo, ExecutionMeteringLimitExceeded, ResourceLimits, TxResources,
    TxnExecutionError,
};

/// Stable decision context attached to each builder transaction event.
#[derive(Debug, Clone)]
pub(crate) struct BuilderTransactionEventContext {
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
    /// Returns the typed context fields shared by all builder transaction events.
    fn event_data(&self) -> BuilderEventContextData {
        BuilderEventContextData {
            parent_hash: format!("{:#x}", self.parent_hash),
            builder_mode: self.builder_mode,
            source_queue: self.source_queue,
            target_flashblock_count: self.target_flashblock_count,
            flashblock_index: self.flashblock_index,
            ordering_position: self.ordering_position,
        }
    }
}

#[derive(Debug, Serialize)]
struct BuilderEventContextData {
    parent_hash: String,
    builder_mode: &'static str,
    source_queue: &'static str,
    target_flashblock_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    flashblock_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ordering_position: Option<u64>,
}

#[derive(Debug, Serialize)]
struct BuilderEventData<T> {
    #[serde(flatten)]
    context: BuilderEventContextData,
    #[serde(flatten)]
    event: T,
}

/// Budget and resource fields shared by builder transaction decision events.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderBudgetFields {
    cumulative_gas_used: u64,
    cumulative_da_bytes_used: u64,
    cumulative_uncompressed_bytes: u64,
    block_gas_limit: u64,
    tx_data_limit: Option<u64>,
    block_data_limit: Option<u64>,
    block_da_footprint_limit: Option<u64>,
    tx_execution_time_limit_us: Option<u128>,
    block_uncompressed_size_limit: Option<u64>,
    #[serde(flatten)]
    transaction_resources: Option<BuilderTransactionResources>,
}

#[derive(Debug, Serialize)]
struct BuilderTransactionResources {
    tx_da_size: u64,
    tx_gas_limit: u64,
    tx_execution_time_us: Option<u128>,
    tx_uncompressed_size: u64,
}

impl BuilderBudgetFields {
    /// Builds the budget fields for the current transaction-selection attempt.
    pub(crate) fn new(
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        resources: Option<&TxResources>,
    ) -> Self {
        Self {
            cumulative_gas_used: info.cumulative_gas_used,
            cumulative_da_bytes_used: info.cumulative_da_bytes_used,
            cumulative_uncompressed_bytes: info.cumulative_uncompressed_bytes,
            block_gas_limit: limits.block_gas_limit,
            tx_data_limit: limits.tx_data_limit,
            block_data_limit: limits.block_data_limit,
            block_da_footprint_limit: limits.block_da_footprint_limit,
            tx_execution_time_limit_us: limits.tx_execution_time_limit_us,
            block_uncompressed_size_limit: limits.block_uncompressed_size_limit,
            transaction_resources: resources.map(BuilderTransactionResources::from),
        }
    }
}

impl From<&TxResources> for BuilderTransactionResources {
    fn from(resources: &TxResources) -> Self {
        Self {
            tx_da_size: resources.da_size,
            tx_gas_limit: resources.gas_limit,
            tx_execution_time_us: resources.execution_time_us,
            tx_uncompressed_size: resources.uncompressed_size,
        }
    }
}

/// Fields emitted when the builder considers a transaction.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderConsideredEventData {
    #[serde(flatten)]
    budget: BuilderBudgetFields,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_target_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_age_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metering_wait_duration_ms: Option<u128>,
}

impl BuilderConsideredEventData {
    /// Creates a considered-event payload with no additional decision details.
    pub(crate) fn new(
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        resources: Option<&TxResources>,
    ) -> Self {
        Self {
            budget: BuilderBudgetFields::new(info, limits, resources),
            bundle_target_block: None,
            tx_age_ms: None,
            metering_wait_duration_ms: None,
        }
    }

    /// Adds the target block associated with a bundle transaction.
    pub(crate) const fn with_bundle_target_block(mut self, block_number: u64) -> Self {
        self.bundle_target_block = Some(block_number);
        self
    }

    /// Adds metering wait details to the considered-event payload.
    pub(crate) const fn with_metering_wait(
        mut self,
        tx_age_ms: u128,
        metering_wait_duration_ms: u128,
    ) -> Self {
        self.tx_age_ms = Some(tx_age_ms);
        self.metering_wait_duration_ms = Some(metering_wait_duration_ms);
        self
    }
}

/// Fields emitted when the builder rejects a transaction.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderRejectedEventData {
    #[serde(flatten)]
    budget: BuilderBudgetFields,
    rejection_reason: &'static str,
    rejection_detail: String,
    permanent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_target_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_block: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_age_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metering_wait_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
}

impl BuilderRejectedEventData {
    /// Creates a rejected-event payload with an explicit reason and detail.
    pub(crate) fn new(
        rejection_reason: &'static str,
        rejection_detail: impl Into<String>,
        permanent: bool,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        resources: Option<&TxResources>,
    ) -> Self {
        Self {
            budget: BuilderBudgetFields::new(info, limits, resources),
            rejection_reason,
            rejection_detail: rejection_detail.into(),
            permanent,
            bundle_target_block: None,
            current_block: None,
            block_timestamp: None,
            tx_age_ms: None,
            metering_wait_duration_ms: None,
            dry_run: None,
        }
    }

    /// Creates a rejected-event payload from a builder execution error.
    pub(crate) fn from_error(
        err: &TxnExecutionError,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        resources: Option<&TxResources>,
    ) -> Self {
        Self::new(
            rejection_reason_code(err),
            err.to_string(),
            err.is_permanent(),
            info,
            limits,
            resources,
        )
    }

    /// Adds the target block associated with a bundle transaction.
    pub(crate) const fn with_bundle_target_block(mut self, block_number: u64) -> Self {
        self.bundle_target_block = Some(block_number);
        self
    }

    /// Adds the current block associated with a bundle rejection.
    pub(crate) const fn with_current_block(mut self, block_number: u64) -> Self {
        self.current_block = Some(block_number);
        self
    }

    /// Adds the block timestamp associated with a bundle rejection.
    pub(crate) const fn with_block_timestamp(mut self, timestamp: u64) -> Self {
        self.block_timestamp = Some(timestamp);
        self
    }

    /// Adds metering wait details to the rejected-event payload.
    pub(crate) const fn with_metering_wait(
        mut self,
        tx_age_ms: u128,
        metering_wait_duration_ms: u128,
    ) -> Self {
        self.tx_age_ms = Some(tx_age_ms);
        self.metering_wait_duration_ms = Some(metering_wait_duration_ms);
        self
    }

    /// Adds the execution metering mode to the rejected-event payload.
    pub(crate) const fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = Some(dry_run);
        self
    }
}

/// Fields emitted when the builder accepts a transaction.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderAcceptedEventData {
    #[serde(flatten)]
    budget: BuilderBudgetFields,
    execution_outcome: &'static str,
    gas_used: u64,
}

impl BuilderAcceptedEventData {
    /// Creates an accepted-event payload.
    pub(crate) fn new(
        execution_outcome: &'static str,
        gas_used: u64,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
        resources: Option<&TxResources>,
    ) -> Self {
        Self {
            budget: BuilderBudgetFields::new(info, limits, resources),
            execution_outcome,
            gas_used,
        }
    }
}

/// Fields emitted when the builder starts a flashblock.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderFlashblockStartedEventData {
    target_gas: u64,
    gas_used: u64,
    target_da: Option<u64>,
    da_used: u64,
    target_da_footprint: Option<u64>,
}

impl BuilderFlashblockStartedEventData {
    /// Creates a flashblock-started payload.
    pub(crate) const fn new(
        target_gas: u64,
        gas_used: u64,
        target_da: Option<u64>,
        da_used: u64,
        target_da_footprint: Option<u64>,
    ) -> Self {
        Self { target_gas, gas_used, target_da, da_used, target_da_footprint }
    }
}

/// Fields emitted when flashblock construction stops before publication.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderFlashblockStoppedEventData {
    reason: &'static str,
    transaction_count: usize,
    build_duration_ms: f64,
}

impl BuilderFlashblockStoppedEventData {
    /// Creates a flashblock-stopped payload.
    pub(crate) const fn new(
        reason: &'static str,
        transaction_count: usize,
        build_duration_ms: f64,
    ) -> Self {
        Self { reason, transaction_count, build_duration_ms }
    }
}

/// Fields emitted when the builder publishes a flashblock.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderFlashblockPublishedEventData {
    transaction_count: usize,
    byte_size: usize,
    build_duration_ms: f64,
    gas_used: u64,
    block_hash: String,
}

impl BuilderFlashblockPublishedEventData {
    /// Creates a flashblock-published payload.
    pub(crate) fn new(
        transaction_count: usize,
        byte_size: usize,
        build_duration_ms: f64,
        gas_used: u64,
        block_hash: B256,
    ) -> Self {
        Self {
            transaction_count,
            byte_size,
            build_duration_ms,
            gas_used,
            block_hash: format!("{block_hash:#x}"),
        }
    }
}

/// Fields emitted when the builder finalizes a payload.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderPayloadFinalizedEventData {
    transaction_count: usize,
    gas_used: u64,
    gas_limit: u64,
    timestamp: u64,
    inclusion_signal: &'static str,
}

impl BuilderPayloadFinalizedEventData {
    /// Creates a payload-finalized event payload.
    pub(crate) const fn new(
        transaction_count: usize,
        gas_used: u64,
        gas_limit: u64,
        timestamp: u64,
        inclusion_signal: &'static str,
    ) -> Self {
        Self { transaction_count, gas_used, gas_limit, timestamp, inclusion_signal }
    }
}

/// Fields emitted when the builder includes a transaction in a finalized payload.
#[derive(Debug, Serialize)]
pub(crate) struct BuilderIncludedEventData {
    inclusion_signal: &'static str,
}

impl BuilderIncludedEventData {
    /// Creates a transaction-included event payload.
    pub(crate) const fn new(inclusion_signal: &'static str) -> Self {
        Self { inclusion_signal }
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
///
/// `data` is lazy so disabled writers skip hot-path payload construction.
pub(crate) fn emit_builder_transaction_event<D, F>(
    ctx: BuilderTransactionEventContext,
    event_type: TransactionEventType,
    tx_hash: TxHash,
    data: F,
) where
    D: Serialize,
    F: FnOnce() -> D,
{
    if GlobalTransactionEventWriter::get().is_none() {
        return;
    }

    let event_type_label = event_type.to_string();
    let data =
        serialize_builder_event_data(BuilderEventData { context: ctx.event_data(), event: data() });

    match transaction_event!(
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
        data: data,
    ) {
        Ok(TransactionEventEmitOutcome::Emitted) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Ok(TransactionEventEmitOutcome::NotConfigured) => {}
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
///
/// `data` is lazy so disabled writers skip hot-path payload construction.
pub(crate) fn emit_builder_payload_event<D, F>(
    ctx: BuilderTransactionEventContext,
    event_type: TransactionEventType,
    data: F,
) where
    D: Serialize,
    F: FnOnce() -> D,
{
    if GlobalTransactionEventWriter::get().is_none() {
        return;
    }

    let event_type_label = event_type.to_string();
    let data =
        serialize_builder_event_data(BuilderEventData { context: ctx.event_data(), event: data() });

    match transaction_event!(
        producer: TransactionEventProducer::BaseBuilder,
        event_type: event_type,
        maybe_block_hash: ctx.block_hash,
        block_number: ctx.block_number,
        payload_id: ctx.payload_id,
        id: {
            "flashblock_index" => ctx.flashblock_index.map(|index| index.to_string()).unwrap_or_default(),
        },
        data: data,
    ) {
        Ok(TransactionEventEmitOutcome::Emitted) => {
            BuilderMetrics::builder_transaction_events_emitted(event_type_label).increment(1);
        }
        Ok(TransactionEventEmitOutcome::NotConfigured) => {}
        Err(err) => {
            BuilderMetrics::builder_transaction_events_dropped(event_type_label, "write")
                .increment(1);
            warn!(
                target: "payload_builder",
                error = %err,
                event_type = %event_type,
                "failed to enqueue builder transaction event"
            );
        }
    }
}

fn serialize_builder_event_data<T: Serialize>(data: BuilderEventData<T>) -> Map<String, Value> {
    serde_json::to_value(data)
        .expect("builder event data must serialize")
        .as_object()
        .expect("builder event data must serialize as an object")
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> BuilderTransactionEventContext {
        BuilderTransactionEventContext {
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
    fn builds_safe_builder_decision_context_fields() {
        let ctx = context();
        let data = serialize_builder_event_data(BuilderEventData {
            context: ctx.event_data(),
            event: BuilderBudgetFields::new(
                &ExecutionInfo {
                    cumulative_gas_used: 21_000,
                    cumulative_da_bytes_used: 120,
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
                    uncompressed_size: 110,
                }),
            ),
        });

        assert_eq!(data["builder_mode"], "flashblocks");
        assert_eq!(data["flashblock_index"], 2);
        assert_eq!(data["ordering_position"], 3);
        assert_eq!(data["tx_da_size"], 120);
        let serialized = serde_json::to_string(&data).unwrap();
        assert!(!serialized.contains("calldata"));
        assert!(!serialized.contains("raw_tx"));
    }

    #[test]
    fn builds_builder_payload_context_fields() {
        let mut ctx = context();
        let block_hash = B256::repeat_byte(0xbb);
        ctx.block_hash = Some(block_hash);
        ctx.flashblock_index = None;
        ctx.ordering_position = None;
        let data = serialize_builder_event_data(BuilderEventData {
            context: ctx.event_data(),
            event: BuilderPayloadFinalizedEventData::new(
                0,
                0,
                30_000_000,
                ctx.block_number,
                "builder_finalized_payload",
            ),
        });

        assert_eq!(ctx.block_hash, Some(block_hash));
        assert_eq!(ctx.block_number, 10);
        assert_eq!(ctx.payload_id, "0x0102030405060708");
        assert_eq!(data["parent_hash"], format!("{:#x}", B256::repeat_byte(0xaa)));
        assert_eq!(data["transaction_count"], 0);
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
                ExecutionMeteringLimitExceeded::TransactionExecutionTime(1, 2),
            )),
            "tx_execution_time_exceeded"
        );
    }
}
