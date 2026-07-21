use std::fmt;

use alloy_primitives::{B256, TxHash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current transaction event schema version.
pub const SCHEMA_VERSION: &str = "transaction-event/v1";

/// Default bounded channel capacity for the background writer.
pub const DEFAULT_QUEUE_CAPACITY: usize = 16_384;

const MAX_DATA_VALIDATION_DEPTH: usize = 16;

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
pub enum TransactionEventType {
    /// Proxyd accepted a transaction request from a client.
    #[serde(rename = "PROXY_RECEIVED")]
    ProxyReceived,
    /// Proxyd rejected a transaction request before backend routing.
    #[serde(rename = "PROXY_REJECTED")]
    ProxyRejected,
    /// Proxyd validation accepted a transaction request.
    #[serde(rename = "PROXY_VALIDATION_ACCEPTED")]
    ProxyValidationAccepted,
    /// Proxyd validation rejected a transaction request.
    #[serde(rename = "PROXY_VALIDATION_REJECTED")]
    ProxyValidationRejected,
    /// Proxyd routed a transaction request to a configured backend.
    #[serde(rename = "PROXY_ROUTED_TO_BACKEND")]
    ProxyRoutedToBackend,
    /// A proxyd backend returned a successful response.
    #[serde(rename = "PROXY_BACKEND_SUCCESS")]
    ProxyBackendSuccess,
    /// A proxyd backend returned an error or failed before responding.
    #[serde(rename = "PROXY_BACKEND_FAILURE")]
    ProxyBackendFailure,
    /// Proxyd attempted to forward a transaction to ingress-rpc.
    #[serde(rename = "PROXY_INGRESS_RPC_ATTEMPT")]
    ProxyIngressRpcAttempt,
    /// Proxyd successfully forwarded a transaction to ingress-rpc.
    #[serde(rename = "PROXY_INGRESS_RPC_SUCCESS")]
    ProxyIngressRpcSuccess,
    /// Proxyd failed to forward a transaction to ingress-rpc.
    #[serde(rename = "PROXY_INGRESS_RPC_FAILURE")]
    ProxyIngressRpcFailure,
    /// Ingress-rpc received a transaction request.
    #[serde(rename = "INGRESS_RECEIVED")]
    IngressReceived,
    /// Transaction simulation started.
    #[serde(rename = "SIMULATION_STARTED")]
    SimulationStarted,
    /// Transaction simulation completed successfully.
    #[serde(rename = "SIMULATION_SUCCEEDED")]
    SimulationSucceeded,
    /// Transaction simulation failed, timed out, or rejected the transaction.
    #[serde(rename = "SIMULATION_FAILED")]
    SimulationFailed,
    /// Ingress-rpc attempted to send metering data for a transaction.
    #[serde(rename = "INGRESS_METERING_SEND_ATTEMPT")]
    IngressMeteringSendAttempt,
    /// Ingress-rpc successfully sent metering data for a transaction.
    #[serde(rename = "INGRESS_METERING_SEND_SUCCESS")]
    IngressMeteringSendSuccess,
    /// Ingress-rpc failed to send metering data for a transaction.
    #[serde(rename = "INGRESS_METERING_SEND_FAILURE")]
    IngressMeteringSendFailure,
    /// Ingress-rpc dropped a queued metering send for a transaction.
    #[serde(rename = "INGRESS_METERING_SEND_DROPPED")]
    IngressMeteringSendDropped,
    /// The txpool observed a transaction in the pending subpool.
    #[serde(rename = "TXPOOL_PENDING")]
    Pending,
    /// The txpool observed a transaction in the queued subpool.
    #[serde(rename = "TXPOOL_QUEUED")]
    Queued,
    /// A txpool transaction moved from pending to queued.
    #[serde(rename = "TXPOOL_PENDING_TO_QUEUED")]
    PendingToQueued,
    /// A txpool transaction moved from queued to pending.
    #[serde(rename = "TXPOOL_QUEUED_TO_PENDING")]
    QueuedToPending,
    /// A txpool transaction was dropped.
    #[serde(rename = "TXPOOL_DROPPED")]
    Dropped,
    /// A txpool transaction was replaced by another transaction.
    #[serde(rename = "TXPOOL_REPLACED")]
    Replaced,
    /// Txpool tracking overflowed before all events could be retained.
    #[serde(rename = "TXPOOL_TRACKING_OVERFLOWED")]
    Overflowed,
    /// Txpool attempted to forward a transaction to the builder.
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_ATTEMPT")]
    TxpoolBuilderForwardAttempt,
    /// Txpool successfully forwarded a transaction to the builder.
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_SUCCESS")]
    TxpoolBuilderForwardSuccess,
    /// Txpool failed to forward a transaction to the builder.
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_FAILURE")]
    TxpoolBuilderForwardFailure,
    /// Txpool dropped a queued builder forward for a transaction.
    #[serde(rename = "TXPOOL_BUILDER_FORWARD_DROPPED")]
    TxpoolBuilderForwardDropped,
    /// Txpool consumed a transaction from the best-txs iterator for builder forwarding.
    #[serde(rename = "TXPOOL_BUILDER_CONSUMED")]
    TxpoolBuilderConsumed,
    /// A validated txpool insert accepted a transaction.
    #[serde(rename = "TXPOOL_VALIDATED_INSERT_ACCEPTED")]
    TxpoolValidatedInsertAccepted,
    /// A validated txpool insert rejected a transaction.
    #[serde(rename = "TXPOOL_VALIDATED_INSERT_REJECTED")]
    TxpoolValidatedInsertRejected,
    /// The builder considered a transaction for payload inclusion.
    #[serde(rename = "BUILDER_CONSIDERED")]
    BuilderConsidered,
    /// The builder accepted a transaction for payload inclusion.
    #[serde(rename = "BUILDER_ACCEPTED")]
    BuilderAccepted,
    /// The builder rejected a transaction during payload construction.
    #[serde(rename = "BUILDER_REJECTED")]
    BuilderRejected,
    /// The builder included a transaction in a finalized payload.
    #[serde(rename = "BUILDER_INCLUDED")]
    BuilderIncluded,
    /// The builder finalized a payload.
    #[serde(rename = "BUILDER_PAYLOAD_FINALIZED")]
    BuilderPayloadFinalized,
    /// The builder started building a flashblock.
    #[serde(rename = "BUILDER_FLASHBLOCK_STARTED")]
    BuilderFlashblockStarted,
    /// The builder published a flashblock.
    #[serde(rename = "BUILDER_FLASHBLOCK_PUBLISHED")]
    BuilderFlashblockPublished,
    /// The builder stopped flashblock construction before publishing.
    #[serde(rename = "BUILDER_FLASHBLOCK_BUILD_STOPPED")]
    BuilderFlashblockBuildStopped,
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
            Self::TxpoolBuilderForwardAttempt => "TXPOOL_BUILDER_FORWARD_ATTEMPT",
            Self::TxpoolBuilderForwardSuccess => "TXPOOL_BUILDER_FORWARD_SUCCESS",
            Self::TxpoolBuilderForwardFailure => "TXPOOL_BUILDER_FORWARD_FAILURE",
            Self::TxpoolBuilderForwardDropped => "TXPOOL_BUILDER_FORWARD_DROPPED",
            Self::TxpoolBuilderConsumed => "TXPOOL_BUILDER_CONSUMED",
            Self::TxpoolValidatedInsertAccepted => "TXPOOL_VALIDATED_INSERT_ACCEPTED",
            Self::TxpoolValidatedInsertRejected => "TXPOOL_VALIDATED_INSERT_REJECTED",
            Self::BuilderConsidered => "BUILDER_CONSIDERED",
            Self::BuilderAccepted => "BUILDER_ACCEPTED",
            Self::BuilderRejected => "BUILDER_REJECTED",
            Self::BuilderIncluded => "BUILDER_INCLUDED",
            Self::BuilderPayloadFinalized => "BUILDER_PAYLOAD_FINALIZED",
            Self::BuilderFlashblockStarted => "BUILDER_FLASHBLOCK_STARTED",
            Self::BuilderFlashblockPublished => "BUILDER_FLASHBLOCK_PUBLISHED",
            Self::BuilderFlashblockBuildStopped => "BUILDER_FLASHBLOCK_BUILD_STOPPED",
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
        if let Some(reason) = find_forbidden_data_key(&self.data, 0) {
            return Err(match reason {
                ForbiddenDataReason::Key(key) => {
                    TransactionEventValidationError::ForbiddenDataKey(key)
                }
                ForbiddenDataReason::TooDeep => TransactionEventValidationError::DataTooDeep,
            });
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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransactionEventValidationError {
    /// `schema_version` did not match [`SCHEMA_VERSION`].
    #[error("invalid transaction event schema_version {0}")]
    InvalidSchemaVersion(String),
    /// `event_id` was empty.
    #[error("transaction event event_id is required")]
    MissingEventId,
    /// `data` contained a key reserved for unsafe payloads or secrets.
    #[error("transaction event data contains forbidden key {0}")]
    ForbiddenDataKey(String),
    /// `data` nesting exceeded the validation depth limit.
    #[error("transaction event data exceeds maximum validation depth")]
    DataTooDeep,
}

enum ForbiddenDataReason {
    Key(String),
    TooDeep,
}

fn find_forbidden_data_key(data: &Map<String, Value>, depth: usize) -> Option<ForbiddenDataReason> {
    if depth > MAX_DATA_VALIDATION_DEPTH {
        return Some(ForbiddenDataReason::TooDeep);
    }
    for (key, value) in data {
        if is_forbidden_data_key(key) {
            return Some(ForbiddenDataReason::Key(key.clone()));
        }
        if let Some(reason) = find_forbidden_data_value(value, depth + 1) {
            return Some(reason);
        }
    }
    None
}

fn find_forbidden_data_value(value: &Value, depth: usize) -> Option<ForbiddenDataReason> {
    if depth > MAX_DATA_VALIDATION_DEPTH {
        return Some(ForbiddenDataReason::TooDeep);
    }
    match value {
        Value::Object(child) => find_forbidden_data_key(child, depth),
        Value::Array(items) => {
            for item in items {
                if let Some(reason) = find_forbidden_data_value(item, depth + 1) {
                    return Some(reason);
                }
            }
            None
        }
        _ => None,
    }
}

fn is_forbidden_data_key(key: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "raw_transaction",
        "raw_tx",
        "raw_transaction_bytes",
        "raw_tx_bytes",
        "calldata",
        "request_body",
        "body",
        "authorization",
        "api_key",
        "api_keys",
        "secret",
        "secrets",
        "x-forwarded-for",
        "forwarded_headers",
        "headers",
    ];
    FORBIDDEN.iter().any(|forbidden| key.eq_ignore_ascii_case(forbidden))
}
