//! Feature-private Blink accounting, cutoff, candidate, and durability records.

#[cfg(feature = "test-utils")]
use std::str::FromStr;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, U256, hex, keccak256};
#[cfg(feature = "test-utils")]
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::BaseTxEnvelope;
use reth_provider::AccountReader;
use revm::precompile::{Crypto, DefaultCrypto};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use thiserror::Error;
use tokio_tungstenite::tungstenite::error::{
    Error as WebSocketError, ProtocolError as WebSocketProtocolError,
};

use crate::{
    A1Outcome, A1Status, AuditedWriteKey, BackrunPlan, CancellationProbe, ExactProtocol,
    MAX_ACCOUNTS, MAX_STORAGE_SLOTS, MaterializedState, MeasurementBindingDeriverV1,
    MeasurementBindingInputV1, MeasurementEncoder, MeasurementExecutionHopV1, PortError,
    PreparedPoolQuote, PreparedPoolState, ProcessedFrame, ShadowOutcome, SlotSubmit,
    SnapshotHandle, SnapshotNonceWitnessV1, TraderSnapshotPort, TransactionVisitor, VisitControl,
};
#[cfg(feature = "test-utils")]
use crate::{BackrunHop, BackrunPlanDigest, MaterializedWrite};

const BLINK_CUTOFF_DRAIN_DEADLINE_V1: Duration = Duration::from_secs(30);
/// Fixed maximum number of simultaneously retained Blink generations.
pub const BLINK_LEDGER_CAPACITY: usize = 4_096;
/// Maximum retained victim bytes in one future-query-free candidate.
pub const EDGE_MAX_VICTIM_RAW_BYTES: usize = 131_072;
/// Maximum exact predecessor transactions retained in the ordered cutoff digest.
pub(crate) const EDGE_MAX_ORDERED_TRANSACTIONS: usize = 65_536;

/// Exact terminal disposition of one admitted Blink generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BlinkGenerationTerminalV1 {
    /// The generation completed frame processing.
    Processed,
    /// A replacement removed the queued generation before frame capture.
    ReplacedBeforeFrame,
    /// Shutdown removed a queued generation before frame capture.
    CancelledBeforeFrame,
    /// The active generation cooperatively terminalized as cancelled.
    Cancelled,
    /// Processing reached an internal fail-closed terminal.
    InternalFailure,
}

/// Exact preterminal disposition of a selected Candidate DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SelectedDtoTerminalV1 {
    /// The selected DTO committed with the frame's Selected terminal.
    Committed,
    /// Cancellation won after DTO construction but before commit.
    CancelledBeforeTerminal,
    /// A non-cancellation terminal prevented commit.
    PreterminalFailure,
}

/// Named fail-closed reason for the frozen Blink runtime branch inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BlinkRejectReasonV3 {
    /// WebSocket connection was already closed.
    AlreadyClosed,
    /// WebSocket connection closed normally.
    ConnectionClosed,
    /// A retryable I/O kind occurred.
    RetryableIo,
    /// A non-retryable I/O kind occurred.
    OtherIo,
    /// TLS failed.
    Tls,
    /// Tungstenite capacity failed.
    Capacity,
    /// Reset arrived without a closing handshake.
    ProtocolResetWithoutClosingHandshake,
    /// A different protocol error occurred.
    Protocol,
    /// The write buffer was full.
    WriteBufferFull,
    /// UTF-8 validation failed.
    Utf8,
    /// An attack-attempt classification occurred.
    AttackAttempt,
    /// URL parsing failed.
    Url,
    /// HTTP formatting failed.
    HttpFormat,
    /// HTTP switching-protocols response was observed.
    Http101,
    /// HTTP request timeout was observed.
    Http408,
    /// HTTP rate limiting was observed.
    Http429,
    /// HTTP server failure was observed.
    Http5xx,
    /// A different HTTP status was observed.
    HttpOther,
    /// A text wire frame exceeded the fixed bound.
    WireTextOversize,
    /// A binary wire frame was rejected.
    WireBinary,
    /// A ping control frame was observed.
    WireControlPing,
    /// A pong control frame was observed.
    WireControlPong,
    /// A close frame was observed.
    WireClose,
    /// An unexpected raw frame was rejected.
    WireUnexpectedFrame,
    /// The wire stream ended.
    WireEnd,
    /// WebSocket request construction failed.
    RequestBuild,
    /// Connect or acknowledgment operation timed out.
    OperationTimeout,
    /// Subscription acknowledgment failed validation.
    AckMalformed,
    /// Subscription acknowledgment text exceeded the wire bound.
    AckTextOversize,
    /// Subscription acknowledgment was binary.
    AckBinary,
    /// Subscription acknowledgment was a close frame.
    AckClose,
    /// Subscription acknowledgment was a control frame.
    AckControl,
    /// Subscription acknowledgment had an unexpected wire shape.
    AckUnexpectedWire,
    /// Notification decode selected the existing application-drop terminal.
    NotificationApplicationDrop,
    /// Notification decode selected the existing protocol-disable terminal.
    NotificationProtocolDisabled,
    /// Notification decode selected the existing internal-failure terminal.
    NotificationInternalFailure,
    /// JSON syntax was invalid.
    JsonSyntax,
    /// The JSON root had the wrong type.
    RootWrongType,
    /// JSON-RPC version was missing, malformed, or wrong.
    JsonRpcMismatch,
    /// Notification method was missing, malformed, or wrong.
    MethodMismatch,
    /// Notification parameters were missing or malformed.
    ParamsInvalid,
    /// Subscription identity was missing, malformed, or wrong.
    SubscriptionMismatch,
    /// Timestamp exceeded the safe JSON integer range.
    TimestampUnsafe,
    /// Publish time exceeded the safe JSON integer range.
    PublishTimeUnsafe,
    /// Block number quantity was invalid.
    BlockNumberInvalid,
    /// Flashblock index quantity was invalid.
    FlashblockIndexInvalid,
    /// Chain identifier quantity was invalid.
    ChainIdInvalid,
    /// Transaction type quantity was invalid or overflowed.
    TransactionTypeInvalid,
    /// Transaction hash format or decoding was invalid.
    TxHashInvalid,
    /// Sender format or decoding was invalid.
    SenderInvalid,
    /// Raw transaction lacked the 0x prefix.
    RawMissingPrefix,
    /// Raw transaction was empty.
    RawEmpty,
    /// Raw transaction contained an odd number of hex digits.
    RawOddLength,
    /// Raw transaction exceeded the fixed bound.
    RawOversize,
    /// Raw transaction contained non-hex input.
    RawNonHex,
    /// Raw transaction decoding failed.
    RawDecode,
    /// Runtime admission was closed.
    SlotClosed,
    /// Checked generation allocation overflowed.
    GenerationOverflow,
    /// A generation received more than one terminal.
    DuplicateGenerationTerminal,
    /// The ledger reached its fixed capacity.
    LedgerCapacityOverflow,
    /// An internal ledger mutex was poisoned.
    LedgerLockPoisoned,
}

impl BlinkRejectReasonV3 {
    /// Stable JSON string used in authority preimages without relying on Rust `Debug`.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::AlreadyClosed => "AlreadyClosed",
            Self::ConnectionClosed => "ConnectionClosed",
            Self::RetryableIo => "RetryableIo",
            Self::OtherIo => "OtherIo",
            Self::Tls => "Tls",
            Self::Capacity => "Capacity",
            Self::ProtocolResetWithoutClosingHandshake => "ProtocolResetWithoutClosingHandshake",
            Self::Protocol => "Protocol",
            Self::WriteBufferFull => "WriteBufferFull",
            Self::Utf8 => "Utf8",
            Self::AttackAttempt => "AttackAttempt",
            Self::Url => "Url",
            Self::HttpFormat => "HttpFormat",
            Self::Http101 => "Http101",
            Self::Http408 => "Http408",
            Self::Http429 => "Http429",
            Self::Http5xx => "Http5xx",
            Self::HttpOther => "HttpOther",
            Self::WireTextOversize => "WireTextOversize",
            Self::WireBinary => "WireBinary",
            Self::WireControlPing => "WireControlPing",
            Self::WireControlPong => "WireControlPong",
            Self::WireClose => "WireClose",
            Self::WireUnexpectedFrame => "WireUnexpectedFrame",
            Self::WireEnd => "WireEnd",
            Self::RequestBuild => "RequestBuild",
            Self::OperationTimeout => "OperationTimeout",
            Self::AckMalformed => "AckMalformed",
            Self::AckTextOversize => "AckTextOversize",
            Self::AckBinary => "AckBinary",
            Self::AckClose => "AckClose",
            Self::AckControl => "AckControl",
            Self::AckUnexpectedWire => "AckUnexpectedWire",
            Self::NotificationApplicationDrop => "NotificationApplicationDrop",
            Self::NotificationProtocolDisabled => "NotificationProtocolDisabled",
            Self::NotificationInternalFailure => "NotificationInternalFailure",
            Self::JsonSyntax => "JsonSyntax",
            Self::RootWrongType => "RootWrongType",
            Self::JsonRpcMismatch => "JsonRpcMismatch",
            Self::MethodMismatch => "MethodMismatch",
            Self::ParamsInvalid => "ParamsInvalid",
            Self::SubscriptionMismatch => "SubscriptionMismatch",
            Self::TimestampUnsafe => "TimestampUnsafe",
            Self::PublishTimeUnsafe => "PublishTimeUnsafe",
            Self::BlockNumberInvalid => "BlockNumberInvalid",
            Self::FlashblockIndexInvalid => "FlashblockIndexInvalid",
            Self::ChainIdInvalid => "ChainIdInvalid",
            Self::TransactionTypeInvalid => "TransactionTypeInvalid",
            Self::TxHashInvalid => "TxHashInvalid",
            Self::SenderInvalid => "SenderInvalid",
            Self::RawMissingPrefix => "RawMissingPrefix",
            Self::RawEmpty => "RawEmpty",
            Self::RawOddLength => "RawOddLength",
            Self::RawOversize => "RawOversize",
            Self::RawNonHex => "RawNonHex",
            Self::RawDecode => "RawDecode",
            Self::SlotClosed => "SlotClosed",
            Self::GenerationOverflow => "GenerationOverflow",
            Self::DuplicateGenerationTerminal => "DuplicateGenerationTerminal",
            Self::LedgerCapacityOverflow => "LedgerCapacityOverflow",
            Self::LedgerLockPoisoned => "LedgerLockPoisoned",
        }
    }
}

/// Exact production disposition paired with one named reject-schema variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlinkRejectDispositionV3 {
    /// Named schema reason for the actual source branch.
    pub reason: BlinkRejectReasonV3,
    /// Runtime status selected by the existing ingress branch.
    pub status: A1Status,
    /// Existing coarse counter outcome, when the branch emits one.
    pub outcome: Option<A1Outcome>,
    /// Whether the existing branch retries.
    pub retry: bool,
    /// Whether the branch is classified as an internal failure.
    pub internal: bool,
    /// Whether the existing branch cancels the root runtime.
    pub cancel_root: bool,
}

/// Exhaustive classifier for the pinned Tungstenite and HTTP branch inventory.
#[derive(Debug, Default, Clone, Copy)]
pub struct BlinkRejectClassifierV3;

impl BlinkRejectClassifierV3 {
    /// Classifies one Tungstenite error without changing ingress control flow.
    pub fn classify(error: &WebSocketError) -> BlinkRejectDispositionV3 {
        match error {
            WebSocketError::ConnectionClosed => Self::disposition(
                BlinkRejectReasonV3::ConnectionClosed,
                A1Status::Retrying,
                Some(A1Outcome::DisconnectObserved),
                true,
                false,
                false,
            ),
            WebSocketError::AlreadyClosed => Self::disposition(
                BlinkRejectReasonV3::AlreadyClosed,
                A1Status::DisabledPermanent,
                Some(A1Outcome::InternalFailure),
                false,
                true,
                true,
            ),
            WebSocketError::Io(error) if Self::retryable_io(error.kind()) => Self::disposition(
                BlinkRejectReasonV3::RetryableIo,
                A1Status::Retrying,
                Some(A1Outcome::TransportFailure),
                true,
                false,
                false,
            ),
            WebSocketError::Io(_) => Self::disposition(
                BlinkRejectReasonV3::OtherIo,
                A1Status::DisabledPermanent,
                Some(A1Outcome::InternalFailure),
                false,
                true,
                true,
            ),
            WebSocketError::Tls(_) => Self::protocol(BlinkRejectReasonV3::Tls),
            WebSocketError::Capacity(_) => Self::protocol(BlinkRejectReasonV3::Capacity),
            WebSocketError::Protocol(WebSocketProtocolError::ResetWithoutClosingHandshake) => {
                Self::disposition(
                    BlinkRejectReasonV3::ProtocolResetWithoutClosingHandshake,
                    A1Status::Retrying,
                    Some(A1Outcome::DisconnectObserved),
                    true,
                    false,
                    false,
                )
            }
            WebSocketError::Protocol(_) => Self::protocol(BlinkRejectReasonV3::Protocol),
            WebSocketError::WriteBufferFull(_) => Self::disposition(
                BlinkRejectReasonV3::WriteBufferFull,
                A1Status::DisabledPermanent,
                Some(A1Outcome::InternalFailure),
                false,
                true,
                true,
            ),
            WebSocketError::Utf8(_) => Self::protocol(BlinkRejectReasonV3::Utf8),
            WebSocketError::AttackAttempt => Self::protocol(BlinkRejectReasonV3::AttackAttempt),
            WebSocketError::Url(_) => Self::protocol(BlinkRejectReasonV3::Url),
            WebSocketError::HttpFormat(_) => Self::protocol(BlinkRejectReasonV3::HttpFormat),
            WebSocketError::Http(response) => {
                Self::classify_http_status(response.status().as_u16())
            }
        }
    }

    /// Classifies the exact HTTP status partition.
    pub const fn classify_http_status(status: u16) -> BlinkRejectDispositionV3 {
        match status {
            101 => Self::disposition(
                BlinkRejectReasonV3::Http101,
                A1Status::AwaitingAck,
                None,
                false,
                false,
                false,
            ),
            408 => Self::disposition(
                BlinkRejectReasonV3::Http408,
                A1Status::Retrying,
                Some(A1Outcome::TransportFailure),
                true,
                false,
                false,
            ),
            429 => Self::disposition(
                BlinkRejectReasonV3::Http429,
                A1Status::Retrying,
                Some(A1Outcome::TransportFailure),
                true,
                false,
                false,
            ),
            500..=599 => Self::disposition(
                BlinkRejectReasonV3::Http5xx,
                A1Status::Retrying,
                Some(A1Outcome::TransportFailure),
                true,
                false,
                false,
            ),
            _ => Self::disposition(
                BlinkRejectReasonV3::HttpOther,
                A1Status::DisabledPermanent,
                Some(A1Outcome::ProtocolDisabled),
                false,
                false,
                false,
            ),
        }
    }

    /// Returns whether one I/O error kind belongs to the exact retryable set.
    pub const fn retryable_io(kind: io::ErrorKind) -> bool {
        matches!(
            kind,
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::TimedOut
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::NotConnected
                | io::ErrorKind::NetworkDown
                | io::ErrorKind::NetworkUnreachable
                | io::ErrorKind::HostUnreachable
        )
    }

    const fn protocol(reason: BlinkRejectReasonV3) -> BlinkRejectDispositionV3 {
        Self::disposition(
            reason,
            A1Status::DisabledPermanent,
            Some(A1Outcome::ProtocolDisabled),
            false,
            false,
            false,
        )
    }

    const fn disposition(
        reason: BlinkRejectReasonV3,
        status: A1Status,
        outcome: Option<A1Outcome>,
        retry: bool,
        internal: bool,
        cancel_root: bool,
    ) -> BlinkRejectDispositionV3 {
        BlinkRejectDispositionV3 { reason, status, outcome, retry, internal, cancel_root }
    }
}

/// Compiled bidirectional inventory of actual Blink ingress, runtime, and ledger branches.
pub const BLINK_REJECT_BRANCH_INVENTORY_V3: &[(&str, BlinkRejectReasonV3)] = &[
    ("transport-already-closed", BlinkRejectReasonV3::AlreadyClosed),
    ("transport-connection-closed", BlinkRejectReasonV3::ConnectionClosed),
    ("transport-io-retryable", BlinkRejectReasonV3::RetryableIo),
    ("transport-io-other", BlinkRejectReasonV3::OtherIo),
    ("transport-tls", BlinkRejectReasonV3::Tls),
    ("transport-capacity", BlinkRejectReasonV3::Capacity),
    ("transport-protocol-reset", BlinkRejectReasonV3::ProtocolResetWithoutClosingHandshake),
    ("transport-protocol-other", BlinkRejectReasonV3::Protocol),
    ("transport-write-buffer-full", BlinkRejectReasonV3::WriteBufferFull),
    ("transport-utf8", BlinkRejectReasonV3::Utf8),
    ("transport-attack-attempt", BlinkRejectReasonV3::AttackAttempt),
    ("transport-url", BlinkRejectReasonV3::Url),
    ("transport-http-format", BlinkRejectReasonV3::HttpFormat),
    ("http-101", BlinkRejectReasonV3::Http101),
    ("http-408", BlinkRejectReasonV3::Http408),
    ("http-429", BlinkRejectReasonV3::Http429),
    ("http-5xx", BlinkRejectReasonV3::Http5xx),
    ("http-other", BlinkRejectReasonV3::HttpOther),
    ("wire-text-oversize", BlinkRejectReasonV3::WireTextOversize),
    ("wire-binary", BlinkRejectReasonV3::WireBinary),
    ("wire-ping", BlinkRejectReasonV3::WireControlPing),
    ("wire-pong", BlinkRejectReasonV3::WireControlPong),
    ("wire-close", BlinkRejectReasonV3::WireClose),
    ("wire-frame", BlinkRejectReasonV3::WireUnexpectedFrame),
    ("wire-end", BlinkRejectReasonV3::WireEnd),
    ("request-build", BlinkRejectReasonV3::RequestBuild),
    ("connect-timeout", BlinkRejectReasonV3::OperationTimeout),
    ("ack-timeout", BlinkRejectReasonV3::OperationTimeout),
    ("ack-malformed", BlinkRejectReasonV3::AckMalformed),
    ("ack-text-oversize", BlinkRejectReasonV3::AckTextOversize),
    ("ack-binary", BlinkRejectReasonV3::AckBinary),
    ("ack-close", BlinkRejectReasonV3::AckClose),
    ("ack-control", BlinkRejectReasonV3::AckControl),
    ("ack-unexpected-wire", BlinkRejectReasonV3::AckUnexpectedWire),
    ("notification-application-drop", BlinkRejectReasonV3::NotificationApplicationDrop),
    ("notification-protocol-disabled", BlinkRejectReasonV3::NotificationProtocolDisabled),
    ("notification-internal-failure", BlinkRejectReasonV3::NotificationInternalFailure),
    ("decode-json-syntax", BlinkRejectReasonV3::JsonSyntax),
    ("decode-root-wrong-type", BlinkRejectReasonV3::RootWrongType),
    ("decode-jsonrpc-mismatch", BlinkRejectReasonV3::JsonRpcMismatch),
    ("decode-method-mismatch", BlinkRejectReasonV3::MethodMismatch),
    ("decode-params-invalid", BlinkRejectReasonV3::ParamsInvalid),
    ("decode-result-invalid", BlinkRejectReasonV3::ParamsInvalid),
    ("decode-subscription-mismatch", BlinkRejectReasonV3::SubscriptionMismatch),
    ("decode-timestamp-unsafe", BlinkRejectReasonV3::TimestampUnsafe),
    ("decode-publish-time-unsafe", BlinkRejectReasonV3::PublishTimeUnsafe),
    ("decode-block-number-invalid", BlinkRejectReasonV3::BlockNumberInvalid),
    ("decode-flashblock-index-invalid", BlinkRejectReasonV3::FlashblockIndexInvalid),
    ("decode-chain-id-invalid", BlinkRejectReasonV3::ChainIdInvalid),
    ("decode-transaction-type-invalid", BlinkRejectReasonV3::TransactionTypeInvalid),
    ("decode-transaction-hash-invalid", BlinkRejectReasonV3::TxHashInvalid),
    ("decode-transaction-hash-malformed", BlinkRejectReasonV3::TxHashInvalid),
    ("decode-sender-invalid", BlinkRejectReasonV3::SenderInvalid),
    ("decode-sender-malformed", BlinkRejectReasonV3::SenderInvalid),
    ("decode-raw-missing-prefix", BlinkRejectReasonV3::RawMissingPrefix),
    ("decode-raw-prefix-invalid", BlinkRejectReasonV3::RawMissingPrefix),
    ("decode-raw-empty", BlinkRejectReasonV3::RawEmpty),
    ("decode-raw-odd-length", BlinkRejectReasonV3::RawOddLength),
    ("decode-raw-oversize", BlinkRejectReasonV3::RawOversize),
    ("decode-raw-non-hex", BlinkRejectReasonV3::RawNonHex),
    ("decode-raw-decode", BlinkRejectReasonV3::RawDecode),
    ("runtime-lifecycle-closed", BlinkRejectReasonV3::SlotClosed),
    ("runtime-submit-closed", BlinkRejectReasonV3::SlotClosed),
    ("runtime-generation-overflow", BlinkRejectReasonV3::GenerationOverflow),
    ("ledger-duplicate-generation-terminal", BlinkRejectReasonV3::DuplicateGenerationTerminal),
    ("ledger-capacity-overflow", BlinkRejectReasonV3::LedgerCapacityOverflow),
    ("ledger-lock-poisoned", BlinkRejectReasonV3::LedgerLockPoisoned),
];

impl BlinkRejectClassifierV3 {
    /// Returns the stable inventory branch for a reason whose emit site is one-to-one.
    pub fn branch_id(reason: BlinkRejectReasonV3) -> Option<&'static str> {
        let mut matching =
            BLINK_REJECT_BRANCH_INVENTORY_V3.iter().filter_map(|(branch_id, inventory_reason)| {
                (*inventory_reason == reason).then_some(*branch_id)
            });
        let branch_id = matching.next()?;
        matching.next().is_none().then_some(branch_id)
    }
}

impl BlinkRejectClassifierV3 {
    /// SHA-256 of the canonical compiled branch-to-reason inventory.
    pub fn branch_inventory_sha256() -> [u8; 32] {
        let mut inventory = String::new();
        for (branch, reason) in BLINK_REJECT_BRANCH_INVENTORY_V3 {
            inventory.push_str(branch);
            inventory.push('=');
            inventory.push_str(reason.wire_name());
            inventory.push('\n');
        }
        DefaultCrypto.sha256(inventory.as_bytes())
    }

    /// SHA-256 of the exact compiled Blink ingress source slice.
    pub fn source_slice_sha256() -> [u8; 32] {
        DefaultCrypto.sha256(include_bytes!("blink_ingress.rs"))
    }

    /// SHA-256 of the exact compiled runtime source slice containing actual emit sites.
    pub fn runtime_source_sha256() -> [u8; 32] {
        DefaultCrypto.sha256(include_bytes!("runtime.rs"))
    }

    /// SHA-256 of this exact compiled ledger and classifier source slice.
    pub fn ledger_source_sha256() -> [u8; 32] {
        DefaultCrypto.sha256(include_bytes!("edge_measurement.rs"))
    }

    /// Exact domain-separated `RejectSchemaV3` provenance digest.
    pub fn reject_schema_digest() -> B256 {
        let mut preimage = Vec::with_capacity(29 + 128);
        preimage.extend_from_slice(b"edge-blink-reject-schema/v3\0");
        preimage.extend_from_slice(&Self::branch_inventory_sha256());
        preimage.extend_from_slice(&Self::source_slice_sha256());
        preimage.extend_from_slice(&Self::runtime_source_sha256());
        preimage.extend_from_slice(&Self::ledger_source_sha256());
        B256::from(DefaultCrypto.sha256(&preimage))
    }
}

/// One immutable snapshot of the Blink conservation ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlinkLedgerSnapshotV1 {
    /// All decoded victims that reached runtime admission.
    pub victim_ingress_observed: u64,
    /// Victims accepted for a checked generation assignment.
    pub victim_ingress_accepted: u64,
    /// New generations placed into an empty slot.
    pub slot_accepted: u64,
    /// New generations that atomically replaced an older queued generation.
    pub slot_replaced: u64,
    /// Victims rejected after runtime closure.
    pub slot_closed: u64,
    /// Victims rejected because generation allocation overflowed.
    pub generation_overflow: u64,
    /// Generations admitted by accepted or replacement submission.
    pub admitted_generations: u64,
    /// Generations terminalized after frame claim.
    pub processed_terminal: u64,
    /// Generations terminalized by replacement before frame capture.
    pub replaced_before_frame: u64,
    /// Queued generations terminalized by shutdown before frame capture.
    pub cancelled_before_frame: u64,
    /// Selected DTOs built before the frame terminal was known.
    pub selected_dto_built_preterminal: u64,
    /// Selected DTOs committed with a Selected terminal.
    pub selected_dto_committed: u64,
    /// Selected DTOs cancelled before terminal commit.
    pub selected_dto_cancelled_before_terminal: u64,
    /// Selected DTOs ending in another preterminal failure.
    pub selected_dto_preterminal_failure: u64,
    /// Admitted generations that still lack a terminal.
    pub generation_pending: u64,
    /// Selected DTOs that still lack a terminal.
    pub selected_pending: u64,
    /// Whether any structural accounting violation occurred.
    pub poisoned: bool,
}

/// Errors from bounded Blink measurement accounting.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMeasurementError {
    /// The fixed generation ledger capacity was exhausted.
    #[error("Blink generation ledger capacity overflow")]
    LedgerCapacityOverflow,
    /// Checked arithmetic overflowed.
    #[error("Blink measurement counter overflow")]
    CounterOverflow,
    /// A generation was missing, duplicated, or terminalized twice.
    #[error("Blink generation ledger conflict")]
    GenerationConflict,
    /// A selected DTO was missing or terminalized twice.
    #[error("Blink selected DTO ledger conflict")]
    SelectedDtoConflict,
    /// The cutoff had already been latched with different data.
    #[error("producer epoch cutoff conflict")]
    CutoffConflict,
    /// A lock was poisoned.
    #[error("Blink measurement ledger lock poisoned")]
    LockPoisoned,
    /// A final seal contained a nonzero pending set or prior poison.
    #[error("Blink measurement final is not closed")]
    FinalNotClosed,
}

/// Bounded producer queue or installation failure.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum EdgeProducerError {
    /// Owner configuration was invalid.
    #[error("edge measurement owner configuration is invalid")]
    InvalidConfig,
    /// A process-local owner had already been installed.
    #[error("edge measurement owner is already installed")]
    AlreadyInstalled,
    /// A checked record sequence overflowed.
    #[error("edge measurement record sequence overflow")]
    SequenceOverflow,
    /// A bounded nonblocking queue was full.
    #[error("edge measurement queue is full")]
    QueueFull,
    /// A bounded nonblocking queue was closed.
    #[error("edge measurement queue is closed")]
    QueueClosed,
    /// Exact later-slice evidence for this selected generation was unavailable.
    #[error("selected candidate evidence is unavailable")]
    MissingCandidateEvidence,
    /// Supplied evidence did not match the live same-frame selection.
    #[error("selected candidate evidence does not match the live frame")]
    CandidateEvidenceMismatch,
    /// Pure evidence binding rejected the exact local inputs.
    #[error("measurement evidence binding failed")]
    MeasurementBindingDerivation,
    /// Measurement accounting failed.
    #[error("edge measurement ledger failed")]
    Ledger,
    /// A staged candidate count could not be represented in the stable accounting revision.
    #[error("edge measurement staged candidate count overflow")]
    StagedCountOverflow,
    /// Pre-cutoff generation authority did not drain before the finite deadline.
    #[error("Blink generation authority cutoff drain deadline exceeded")]
    CutoffDrainDeadline,
    /// Cutoff had not been latched.
    #[error("edge measurement cutoff is missing")]
    CutoffMissing,
}

/// Validated installation values supplied by the later CLI coordinator.
#[derive(Debug, Clone)]
pub struct EdgeMeasurementOwnerConfigV1 {
    /// Nonzero producer epoch.
    pub producer_epoch: u64,
    /// Compatibility path for the immutable output root.
    pub output_root: PathBuf,
    /// Preflighted descriptor pinning the exact output-root inode.
    pub output_root_handle: Arc<File>,
    /// Producer manifest digest.
    pub producer_digest: B256,
    /// Reject-schema digest.
    pub reject_schema_digest: B256,
    /// Canonical preregistration digest.
    pub prereg_digest: B256,
    /// Owner policy digest.
    pub policy_digest: B256,
    /// Owner configuration digest.
    pub config_digest: B256,
    /// Owner approval receipt digest.
    pub owner_approval_receipt_digest: B256,
    /// Bounded reject/coverage queue capacity.
    pub record_queue_capacity: usize,
    /// Bounded candidate queue capacity.
    pub candidate_queue_capacity: usize,
    /// Measurement-only sender whose parent/pending nonce is captured.
    pub measurement_sender: Address,
    /// Pinned executor runtime bytecode hash.
    pub executor_runtime_hash: B256,
    /// Uniswap V2 adapter and runtime bytecode hash.
    pub v2_adapter: Address,
    /// Uniswap V2 adapter runtime bytecode hash.
    pub v2_adapter_runtime_hash: B256,
    /// Uniswap V3 adapter and runtime bytecode hash.
    pub v3_adapter: Address,
    /// Uniswap V3 adapter runtime bytecode hash.
    pub v3_adapter_runtime_hash: B256,
    /// Aerodrome adapter and runtime bytecode hash.
    pub aerodrome_adapter: Address,
    /// Aerodrome adapter runtime bytecode hash.
    pub aerodrome_adapter_runtime_hash: B256,
    /// Approved G0 deployment identity digest.
    pub g0_code_identity_digest: B256,
    /// Canonical raw reject branch inventory digest.
    pub raw_reject_inventory_sha256: B256,
    /// SHA-256 of the compiled reject-classifier source.
    pub raw_reject_source_sha256: B256,
    /// SHA-256 of the compiled measurement binding source.
    pub measurement_binding_source_sha256: B256,
}

impl EdgeMeasurementOwnerConfigV1 {
    /// Validates the once-only owner configuration without reading environment or disk.
    pub fn validate(&self) -> Result<(), EdgeProducerError> {
        if self.producer_epoch == 0
            || self.output_root.as_os_str().is_empty()
            || !self.output_root_handle.metadata().is_ok_and(|metadata| metadata.is_dir())
            || self.producer_digest.is_zero()
            || self.reject_schema_digest != BlinkRejectClassifierV3::reject_schema_digest()
            || self.prereg_digest.is_zero()
            || self.policy_digest.is_zero()
            || self.config_digest.is_zero()
            || self.owner_approval_receipt_digest.is_zero()
            || self.record_queue_capacity == 0
            || self.candidate_queue_capacity == 0
            || self.measurement_sender.is_zero()
            || self.executor_runtime_hash.is_zero()
            || self.v2_adapter.is_zero()
            || self.v2_adapter_runtime_hash.is_zero()
            || self.v3_adapter.is_zero()
            || self.v3_adapter_runtime_hash.is_zero()
            || self.aerodrome_adapter.is_zero()
            || self.aerodrome_adapter_runtime_hash.is_zero()
            || self.g0_code_identity_digest.is_zero()
            || self.raw_reject_inventory_sha256 != Self::raw_reject_inventory_sha256()
            || self.raw_reject_source_sha256
                != B256::new(DefaultCrypto.sha256(include_bytes!("edge_measurement.rs")))
            || self.measurement_binding_source_sha256
                != B256::new(DefaultCrypto.sha256(include_bytes!("measurement_tx.rs")))
        {
            return Err(EdgeProducerError::InvalidConfig);
        }
        Ok(())
    }

    /// Returns the canonical digest of the raw classifier inventory, independently of its
    /// composite `RejectSchema` digest.
    pub fn raw_reject_inventory_sha256() -> B256 {
        let mut canonical = String::new();
        for (branch, reason) in BLINK_REJECT_BRANCH_INVENTORY_V3 {
            canonical.push_str(branch);
            canonical.push('\0');
            canonical.push_str(reason.wire_name());
            canonical.push('\n');
        }
        B256::new(DefaultCrypto.sha256(canonical.as_bytes()))
    }
}

/// One named actual Blink branch record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlinkRejectRecordV3 {
    /// Canonical schema marker.
    pub schema: &'static str,
    /// Producer epoch.
    pub producer_epoch: String,
    /// Checked record sequence.
    pub sequence: String,
    /// Stable terminal state for the chained reject ledger.
    pub state: &'static str,
    /// Stable identity of the actual source emit branch.
    pub branch_id: &'static str,
    /// Actual named branch reason.
    pub reason: BlinkRejectReasonV3,
    /// Previous record SHA-256.
    pub previous_record_hash: String,
    /// Record SHA-256.
    pub record_hash: String,
}

/// Named pre-enqueue terminal for a selected live plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CandidatePreEnqueueDropReasonV3 {
    /// Same-frame evidence required to stage the draft was unavailable.
    MissingRequiredEvidence,
    /// Same-frame evidence did not match the selected plan.
    EvidenceMismatch,
    /// Non-envelope binding rejected nonce, victim, fee, plan, or execution-hop evidence.
    MeasurementDerivationRejected,
    /// The selected generation was cancelled after its draft was staged.
    CancelledAfterDraft,
    /// The selected generation became stale after its draft was staged.
    StaleAfterDraft,
    /// The selected generation failed after its draft was staged.
    FailedAfterDraft,
    /// The cutoff drain deadline abandoned a staged draft.
    CutoffDrainDeadline,
    /// The bounded candidate queue was full.
    CandidateQueueFull,
    /// The bounded candidate queue was closed.
    CandidateQueueClosed,
}

impl CandidatePreEnqueueDropReasonV3 {
    const COUNT: usize = 9;

    const fn counter_index(self) -> usize {
        match self {
            Self::MissingRequiredEvidence => 0,
            Self::EvidenceMismatch => 1,
            Self::MeasurementDerivationRejected => 2,
            Self::CancelledAfterDraft => 3,
            Self::StaleAfterDraft => 4,
            Self::FailedAfterDraft => 5,
            Self::CutoffDrainDeadline => 6,
            Self::CandidateQueueFull => 7,
            Self::CandidateQueueClosed => 8,
        }
    }
}

/// Exact cumulative counts of selected candidates that did not enter the candidate queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CandidatePreEnqueueDropCountersV1 {
    /// Same-frame staging failures caused by missing required evidence.
    pub missing_required_evidence: u64,
    /// Same-frame staging failures caused by evidence mismatch.
    pub evidence_mismatch: u64,
    /// Measurement evidence binding rejections.
    pub measurement_derivation_rejected: u64,
    /// Staged candidates cancelled before enqueue.
    pub cancelled_after_draft: u64,
    /// Staged candidates that became stale before enqueue.
    pub stale_after_draft: u64,
    /// Staged candidates that failed before enqueue.
    pub failed_after_draft: u64,
    /// Staged candidates abandoned at the cutoff drain deadline.
    pub cutoff_drain_deadline: u64,
    /// Candidate enqueue attempts rejected because the bounded queue was full.
    pub candidate_queue_full: u64,
    /// Candidate enqueue attempts rejected because the bounded queue was closed.
    pub candidate_queue_closed: u64,
}

/// Exact cumulative counts of Blink reject records lost at the bounded record queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BlinkRejectQueueLossCountersV1 {
    /// Reject records dropped because the bounded record queue was full.
    pub queue_full: u64,
    /// Reject records dropped because the bounded record queue was closed.
    pub queue_closed: u64,
}

/// Exact cumulative counts of candidate-drop records lost at the bounded record queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CandidateDropQueueLossCountersV1 {
    /// Candidate-drop records lost because the bounded record queue was full.
    pub queue_full: u64,
    /// Candidate-drop records lost because the bounded record queue was closed.
    pub queue_closed: u64,
}

/// Borrowed, same-frame evidence required to stage one candidate without future queries.
#[derive(Debug)]
pub struct EdgeCandidateStageInputV3<'a> {
    /// Independent Blink runtime generation; zero is the first valid generation.
    pub generation: u64,
    /// Authoritative snapshot port used only during staging.
    pub port: &'a dyn TraderSnapshotPort,
    /// Captured pending snapshot.
    pub snapshot: &'a SnapshotHandle,
    /// Processed post-victim frame.
    pub processed: &'a ProcessedFrame,
    /// Complete bounded prepared universe.
    pub prepared: &'a [PreparedPoolState],
    /// Selected canonical measurement plan.
    pub plan: &'a BackrunPlan,
    /// Exact raw victim transaction.
    pub victim_raw: &'a Bytes,
    /// Shared authority/deadline probe.
    pub probe: &'a CancellationProbe,
}

#[derive(Debug)]
struct OrderedTransactionReceiptVisitor {
    next_position: usize,
    canonical: Vec<u8>,
    victim_hash: B256,
}

impl OrderedTransactionReceiptVisitor {
    fn new(
        block_number: u64,
        cutoff_position: usize,
        victim_hash: B256,
    ) -> Result<Self, EdgeProducerError> {
        let cutoff_position = u64::try_from(cutoff_position)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"edge-ordered-transaction-cutoff/v1\0");
        canonical.extend_from_slice(&block_number.to_be_bytes());
        canonical.extend_from_slice(&cutoff_position.to_be_bytes());
        Ok(Self { next_position: 0, canonical, victim_hash })
    }

    fn visit_hash(&mut self, position: usize, transaction_hash: B256) -> Result<(), PortError> {
        if position != self.next_position || transaction_hash == self.victim_hash {
            return Err(PortError::Incoherent);
        }
        let position = u64::try_from(position).map_err(|_| PortError::LimitExceeded)?;
        self.canonical.extend_from_slice(&position.to_be_bytes());
        self.canonical.extend_from_slice(transaction_hash.as_slice());
        self.next_position = self.next_position.checked_add(1).ok_or(PortError::LimitExceeded)?;
        Ok(())
    }
}

impl TransactionVisitor for OrderedTransactionReceiptVisitor {
    fn visit(
        &mut self,
        position: usize,
        transaction: &BaseTxEnvelope,
    ) -> Result<VisitControl, PortError> {
        self.visit_hash(position, B256::new(*transaction.tx_hash()))?;
        Ok(VisitControl::Continue)
    }
}

/// Complete future-query-free pre-cutoff draft staged by Blink generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeCandidateEvidenceV3 {
    generation: u64,
    source_generation: u64,
    coverage_generation: u64,
    pending_snapshot_sequence: u64,
    payload_first_record_sequence: u64,
    payload_first_record_hash: [u8; 32],
    structural_terminal_hash: [u8; 32],
    connection_sequence_at_capture: u64,
    connection_record_hash_at_capture: [u8; 32],
    registry_terminal_record_hash: [u8; 32],
    parent_hash: B256,
    state_root: B256,
    ordered_transaction_count: u64,
    victim_absent_before_position: bool,
    ordered_transaction_cutoff_position: u64,
    ordered_transaction_digest: B256,
    victim_raw: Bytes,
    selected_plan: BackrunPlan,
    prepared_route: [PreparedPoolState; 2],
    materialized_state: MaterializedState,
    prepared_state_digest: B256,
    code_witness_digest: B256,
    slot_witness_digest: B256,
    economics_evidence_digest: B256,
    execution_hops: [MeasurementExecutionHopV1; 2],
    deployment_identities: [(Address, B256); 4],
    backrun_measurement_binding: crate::BackrunMeasurementBindingV1,
}

/// Queue item from the live selected-plan producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeProducerRecordV1 {
    /// One actual Blink reject branch.
    BlinkReject(BlinkRejectRecordV3),
    /// One selected-plan pre-enqueue drop.
    CandidateDrop {
        /// Blink generation.
        generation: u64,
        /// Named terminal.
        reason: CandidatePreEnqueueDropReasonV3,
    },
}

/// Checked candidate bounds without an empty-epoch/sequence-zero ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckedCandidateBoundsV1 {
    /// Number of candidate sequences allocated in this producer epoch.
    pub count: u64,
    /// Last allocated candidate sequence, absent when `count` is zero.
    pub last_sequence: Option<u64>,
}
/// Bounded producer queue depths captured with an accounting snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EdgeQueueAccountingSnapshotV1 {
    /// Producer records awaiting the canonical writer.
    pub pending_records: u64,
    /// Candidate details awaiting the canonical writer.
    pub pending_candidates: u64,
    /// Candidate drafts staged before terminal resolution.
    pub staged_count: u64,
}

/// Stable raw producer accounting snapshot across startup, periodic, and terminal phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRawAccountingSnapshotV1 {
    /// Stable revision vector used to reject mixed accounting snapshots.
    pub revision: [u64; 36],
    /// Raw Blink conservation counters.
    pub blink: BlinkLedgerSnapshotV1,
    /// Bounded producer queue depths.
    pub queues: EdgeQueueAccountingSnapshotV1,
    /// Checked candidate count and optional inclusive last sequence.
    pub candidate_bounds: CheckedCandidateBoundsV1,
    /// Candidate drops recorded before enqueue.
    pub candidate_drops: CandidatePreEnqueueDropCountersV1,
    /// Blink reject records lost at the bounded queue.
    pub blink_reject_loss: BlinkRejectQueueLossCountersV1,
    /// Candidate drop records lost at the bounded queue.
    pub candidate_drop_loss: CandidateDropQueueLossCountersV1,
    /// Cutoff witness when cutoff has been latched.
    pub cutoff: Option<ProducerEpochCutoffV1>,
}
#[cfg(any(test, feature = "test-utils"))]
/// Strongly typed read-only audit view of the validated owner configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeMeasurementOwnerConfigAuditSnapshotV1 {
    /// Producer epoch owned by this measurement instance.
    pub producer_epoch: u64,
    /// Canonical output root.
    pub output_root: PathBuf,
    /// Stable identity of the validated output-root handle.
    pub output_root_handle_identity: usize,
    /// Producer implementation digest.
    pub producer_digest: B256,
    /// Reject schema digest.
    pub reject_schema_digest: B256,
    /// Preregistration contract digest.
    pub prereg_digest: B256,
    /// Runtime policy digest.
    pub policy_digest: B256,
    /// Complete configuration digest.
    pub config_digest: B256,
    /// Owner approval receipt digest.
    pub owner_approval_receipt_digest: B256,
    /// Bounded producer-record queue capacity.
    pub record_queue_capacity: usize,
    /// Bounded candidate-detail queue capacity.
    pub candidate_queue_capacity: usize,
    /// Measurement transaction sender.
    pub measurement_sender: Address,
    /// Executor runtime code hash.
    pub executor_runtime_hash: B256,
    /// V2 adapter address.
    pub v2_adapter: Address,
    /// V2 adapter runtime code hash.
    pub v2_adapter_runtime_hash: B256,
    /// V3 adapter address.
    pub v3_adapter: Address,
    /// V3 adapter runtime code hash.
    pub v3_adapter_runtime_hash: B256,
    /// Aerodrome adapter address.
    pub aerodrome_adapter: Address,
    /// Aerodrome adapter runtime code hash.
    pub aerodrome_adapter_runtime_hash: B256,
    /// G0 code identity digest.
    pub g0_code_identity_digest: B256,
    /// Raw reject inventory source digest.
    pub raw_reject_inventory_sha256: B256,
    /// Raw reject implementation source digest.
    pub raw_reject_source_sha256: B256,
    /// Measurement binding implementation source digest.
    pub measurement_binding_source_sha256: B256,
}

#[cfg(any(test, feature = "test-utils"))]
/// Strongly typed read-only audit view of every Blink ledger counter, flag, and map entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlinkMeasurementLedgerAuthorityAuditSnapshotV1 {
    /// Victims observed at runtime ingress.
    pub observed: u64,
    /// Victims accepted for slot submission.
    pub accepted: u64,
    /// Slot submissions accepted.
    pub slot_accepted: u64,
    /// Slot submissions replaced.
    pub slot_replaced: u64,
    /// Slot submissions rejected after closure.
    pub slot_closed: u64,
    /// Checked generation allocation overflows.
    pub generation_overflow: u64,
    /// Generations admitted to processing.
    pub admitted_generations: u64,
    /// Generations reaching processed terminal state.
    pub processed_terminal: u64,
    /// Generations replaced before frame processing.
    pub replaced_before_frame: u64,
    /// Generations cancelled before frame processing.
    pub cancelled_before_frame: u64,
    /// Selected DTOs built before terminal resolution.
    pub selected_dto_built_preterminal: u64,
    /// Selected DTOs committed.
    pub selected_dto_committed: u64,
    /// Selected DTOs cancelled before terminal resolution.
    pub selected_dto_cancelled_before_terminal: u64,
    /// Selected DTOs ending in a preterminal failure.
    pub selected_dto_preterminal_failure: u64,
    /// Whether ledger admission is closed.
    pub admission_closed: bool,
    /// Whether the ledger is poisoned.
    pub poisoned: bool,
    /// Generation terminal authority keyed by generation.
    pub generations: BTreeMap<u64, Option<BlinkGenerationTerminalV1>>,
    /// Selected DTO terminal authority keyed by generation.
    pub selected: BTreeMap<u64, Option<SelectedDtoTerminalV1>>,
    /// Whether the generation map mutex is poisoned.
    pub generations_mutex_poisoned: bool,
    /// Whether the selected DTO map mutex is poisoned.
    pub selected_mutex_poisoned: bool,
}

#[cfg(any(test, feature = "test-utils"))]
/// Complete strongly typed read-only audit snapshot of Blink owner authority.
///
/// Equality includes configuration identity, complete ledger authority, cutoff witness, every live
/// FIFO payload, generation/staging collections, sequences, losses, pending counts, hash-chain
/// cursor, poison/install/cutoff flags, and all authority lock-poison observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeMeasurementOwnerAuthorityAuditSnapshotV1 {
    /// Validated owner configuration.
    pub config: EdgeMeasurementOwnerConfigAuditSnapshotV1,
    /// Complete Blink conservation ledger authority.
    pub ledger: BlinkMeasurementLedgerAuthorityAuditSnapshotV1,
    /// Latched producer cutoff witness.
    pub cutoff: Option<ProducerEpochCutoffV1>,
    /// Live generation authority.
    pub generation_authority: BTreeSet<u64>,
    /// Producer records awaiting canonical persistence.
    pub record_queue: Vec<EdgeProducerRecordV1>,
    /// Candidate details awaiting canonical persistence.
    pub candidate_queue: Vec<EdgeCandidateDetailV1>,
    /// Candidate evidence staged before terminal resolution.
    pub staged: BTreeMap<u64, EdgeCandidateEvidenceV3>,
    /// Candidate sequences whose staging failed.
    pub staging_failures: BTreeSet<u64>,
    /// Next reject-record sequence.
    pub reject_sequence: u64,
    /// Next candidate sequence.
    pub candidate_sequence: u64,
    /// Candidate drops recorded before enqueue.
    pub candidate_drop_counts: CandidatePreEnqueueDropCountersV1,
    /// Blink reject records lost at the bounded queue.
    pub blink_reject_queue_loss: BlinkRejectQueueLossCountersV1,
    /// Candidate drop records lost at the bounded queue.
    pub candidate_drop_queue_loss: CandidateDropQueueLossCountersV1,
    /// Producer records pending canonical persistence.
    pub pending_records: u64,
    /// Candidate details pending canonical persistence.
    pub pending_candidates: u64,
    /// Previous canonical Blink reject record hash.
    pub reject_previous_hash: [u8; 32],
    /// Whether owner authority is poisoned.
    pub poisoned: bool,
    /// Whether the global owner is installed.
    pub installed: bool,
    /// Whether producer cutoff is latched.
    pub cutoff_latched: bool,
    /// Whether the admission mutex is poisoned.
    pub admission_mutex_poisoned: bool,
    /// Whether the generation-authority mutex is poisoned.
    pub generation_authority_mutex_poisoned: bool,
    /// Whether the producer-record receiver mutex is poisoned.
    pub record_receiver_mutex_poisoned: bool,
    /// Whether the candidate receiver mutex is poisoned.
    pub candidate_receiver_mutex_poisoned: bool,
    /// Whether the staged-candidate mutex is poisoned.
    pub staged_mutex_poisoned: bool,
    /// Whether the staging-failures mutex is poisoned.
    pub staging_failures_mutex_poisoned: bool,
    /// Whether the reject hash-chain mutex is poisoned.
    pub reject_previous_hash_mutex_poisoned: bool,
    /// Whether the cutoff mutex is poisoned.
    pub cutoff_mutex_poisoned: bool,
}

/// Cutoff-closed raw producer accounting snapshot with a mandatory witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeCutoffDrainedSnapshotV1 {
    /// Stable revision vector used to reject mixed accounting snapshots.
    pub revision: [u64; 36],
    /// Raw Blink conservation counters.
    pub blink: BlinkLedgerSnapshotV1,
    /// Drained bounded producer queue depths.
    pub queues: EdgeQueueAccountingSnapshotV1,
    /// Checked candidate count and optional inclusive last sequence.
    pub candidate_bounds: CheckedCandidateBoundsV1,
    /// Candidate drops recorded before enqueue.
    pub candidate_drops: CandidatePreEnqueueDropCountersV1,
    /// Blink reject records lost at the bounded queue.
    pub blink_reject_loss: BlinkRejectQueueLossCountersV1,
    /// Candidate drop records lost at the bounded queue.
    pub candidate_drop_loss: CandidateDropQueueLossCountersV1,
    /// Mandatory cutoff witness.
    pub cutoff: ProducerEpochCutoffV1,
}

/// Once-only optional edge-only producer owner.
///
/// Production authority lock order is admission → record receiver → candidate receiver →
/// generation authority → staged candidates → staging failures → reject previous hash → ledger
/// generations → ledger selected DTOs → cutoff latch. Authority audits hold this complete chain
/// while logically draining and restoring both FIFOs, so no producer or consumer can observe the
/// transient drain.
#[derive(Debug)]
pub struct EdgeMeasurementOwnerV1 {
    config: EdgeMeasurementOwnerConfigV1,
    ledger: BlinkMeasurementLedgerV1,
    cutoff: ProducerEpochCutoffLatchV1,
    admission: Mutex<()>,
    generation_authority: Mutex<BTreeSet<u64>>,
    generation_authority_drained: Condvar,
    record_sender: SyncSender<EdgeProducerRecordV1>,
    record_receiver: Mutex<Receiver<EdgeProducerRecordV1>>,
    candidate_sender: SyncSender<EdgeCandidateDetailV1>,
    candidate_receiver: Mutex<Receiver<EdgeCandidateDetailV1>>,
    staged: Mutex<BTreeMap<u64, EdgeCandidateEvidenceV3>>,
    staging_failures: Mutex<BTreeSet<u64>>,
    reject_sequence: AtomicU64,
    candidate_sequence: AtomicU64,
    candidate_drop_counts: [AtomicU64; CandidatePreEnqueueDropReasonV3::COUNT],
    blink_reject_queue_full: AtomicU64,
    blink_reject_queue_closed: AtomicU64,
    candidate_drop_queue_full: AtomicU64,
    candidate_drop_queue_closed: AtomicU64,
    pending_records: AtomicU64,
    pending_candidates: AtomicU64,
    reject_previous_hash: Mutex<[u8; 32]>,
    poisoned: AtomicBool,
    installed: AtomicBool,
    cutoff_latched: AtomicBool,
}

impl EdgeMeasurementOwnerV1 {
    /// Constructs a validated, bounded owner. It remains inert until installed in runtime config.
    pub fn new(config: EdgeMeasurementOwnerConfigV1) -> Result<Arc<Self>, EdgeProducerError> {
        config.validate()?;
        let (record_sender, record_receiver) = sync_channel(config.record_queue_capacity);
        let (candidate_sender, candidate_receiver) = sync_channel(config.candidate_queue_capacity);
        Ok(Arc::new(Self {
            config,
            ledger: BlinkMeasurementLedgerV1::default(),
            cutoff: ProducerEpochCutoffLatchV1::default(),
            admission: Mutex::new(()),
            generation_authority: Mutex::new(BTreeSet::new()),
            generation_authority_drained: Condvar::new(),
            record_sender,
            record_receiver: Mutex::new(record_receiver),
            candidate_sender,
            candidate_receiver: Mutex::new(candidate_receiver),
            staged: Mutex::new(BTreeMap::new()),
            staging_failures: Mutex::new(BTreeSet::new()),
            reject_sequence: AtomicU64::new(0),
            candidate_sequence: AtomicU64::new(0),
            candidate_drop_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            blink_reject_queue_full: AtomicU64::new(0),
            blink_reject_queue_closed: AtomicU64::new(0),
            candidate_drop_queue_full: AtomicU64::new(0),
            candidate_drop_queue_closed: AtomicU64::new(0),
            pending_records: AtomicU64::new(0),
            pending_candidates: AtomicU64::new(0),
            reject_previous_hash: Mutex::new([0; 32]),
            poisoned: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            cutoff_latched: AtomicBool::new(false),
        }))
    }

    /// Installs the sole process-local owner handle exactly once.
    pub fn install(self: &Arc<Self>) -> Result<(), EdgeProducerError> {
        self.installed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| EdgeProducerError::AlreadyInstalled)
    }

    /// Returns whether this authority epoch still admits producer events.
    pub fn is_accepting(&self) -> bool {
        !self.cutoff_latched.load(Ordering::Acquire)
    }

    /// Linearizes one complete production admission transaction with cutoff preparation.
    ///
    /// The callback always runs so post-cutoff production remains latest-wins. `authoritative` is
    /// false after cutoff or lock poison, and no measurement mutation may be made in that case.
    pub fn with_blink_admission<T>(&self, operation: impl FnOnce(&Self, bool) -> T) -> T {
        match self.admission.lock() {
            Ok(_admission) => operation(self, self.is_accepting()),
            Err(_) => {
                self.poison(EdgeProducerError::Ledger);
                operation(self, false)
            }
        }
    }

    /// Returns the validated producer epoch.
    pub const fn producer_epoch(&self) -> u64 {
        self.config.producer_epoch
    }

    /// Returns the exact bounded conservation ledger.
    pub const fn ledger(&self) -> &BlinkMeasurementLedgerV1 {
        &self.ledger
    }

    /// Current checked producer cursors used by the CLI cutoff coordinator.
    ///
    /// The candidate cursor remains the checked count for compatibility. New cutoff coordination
    /// should use [`Self::checked_candidate_bounds`] to distinguish an empty epoch from sequence 0.
    pub fn cutoff_cursors(&self) -> Result<(u64, u64), EdgeProducerError> {
        let blink = self
            .ledger
            .snapshot()
            .map_err(|_| self.poison(EdgeProducerError::Ledger))?
            .admitted_generations;
        Ok((blink, self.checked_candidate_bounds().count))
    }

    /// Returns the checked candidate count and its optional inclusive last sequence.
    pub fn checked_candidate_bounds(&self) -> CheckedCandidateBoundsV1 {
        let count = self.candidate_sequence.load(Ordering::Acquire);
        CheckedCandidateBoundsV1 { count, last_sequence: count.checked_sub(1) }
    }
    #[cfg(any(test, feature = "test-utils"))]
    /// Returns a complete strongly typed clone-and-equality audit snapshot of owner authority.
    ///
    /// Lock order is admission → record receiver → candidate receiver → generation authority →
    /// staged candidates → staging failures → reject previous hash → ledger generations → ledger
    /// selected DTOs → cutoff latch. Both queues are drained and restored in exact FIFO order while
    /// all producer and consumer locks are held. Any restore mismatch panics while holding the lock
    /// chain, poisoning authority rather than exposing a partial snapshot.
    pub fn authority_audit_snapshot(&self) -> EdgeMeasurementOwnerAuthorityAuditSnapshotV1 {
        let admission_mutex_poisoned = self.admission.is_poisoned();
        let _admission = self.admission.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let record_receiver_mutex_poisoned = self.record_receiver.is_poisoned();
        let record_receiver =
            self.record_receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate_receiver_mutex_poisoned = self.candidate_receiver.is_poisoned();
        let candidate_receiver =
            self.candidate_receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation_authority_mutex_poisoned = self.generation_authority.is_poisoned();
        let generation_authority =
            self.generation_authority.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let staged_mutex_poisoned = self.staged.is_poisoned();
        let staged = self.staged.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let staging_failures_mutex_poisoned = self.staging_failures.is_poisoned();
        let staging_failures =
            self.staging_failures.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let reject_previous_hash_mutex_poisoned = self.reject_previous_hash.is_poisoned();
        let reject_previous_hash =
            self.reject_previous_hash.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let generations_mutex_poisoned = self.ledger.generations.is_poisoned();
        let generations =
            self.ledger.generations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let selected_mutex_poisoned = self.ledger.selected.is_poisoned();
        let selected = self.ledger.selected.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let cutoff_mutex_poisoned = self.cutoff.value.is_poisoned();
        let cutoff = self.cutoff.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut record_queue = Vec::new();
        loop {
            match record_receiver.try_recv() {
                Ok(record) => record_queue.push(record),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    panic!("Blink record authority queue disconnected during audit")
                }
            }
        }
        let mut candidate_queue = Vec::new();
        loop {
            match candidate_receiver.try_recv() {
                Ok(candidate) => candidate_queue.push(candidate),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    panic!("Blink candidate authority queue disconnected during audit")
                }
            }
        }
        for record in &record_queue {
            match self.record_sender.try_send(record.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    panic!("Blink record authority queue restore capacity mismatch")
                }
                Err(TrySendError::Disconnected(_)) => {
                    panic!("Blink record authority queue disconnected during restore")
                }
            }
        }
        for candidate in &candidate_queue {
            match self.candidate_sender.try_send(candidate.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    panic!("Blink candidate authority queue restore capacity mismatch")
                }
                Err(TrySendError::Disconnected(_)) => {
                    panic!("Blink candidate authority queue disconnected during restore")
                }
            }
        }

        EdgeMeasurementOwnerAuthorityAuditSnapshotV1 {
            config: EdgeMeasurementOwnerConfigAuditSnapshotV1 {
                producer_epoch: self.config.producer_epoch,
                output_root: self.config.output_root.clone(),
                output_root_handle_identity: Arc::as_ptr(&self.config.output_root_handle) as usize,
                producer_digest: self.config.producer_digest,
                reject_schema_digest: self.config.reject_schema_digest,
                prereg_digest: self.config.prereg_digest,
                policy_digest: self.config.policy_digest,
                config_digest: self.config.config_digest,
                owner_approval_receipt_digest: self.config.owner_approval_receipt_digest,
                record_queue_capacity: self.config.record_queue_capacity,
                candidate_queue_capacity: self.config.candidate_queue_capacity,
                measurement_sender: self.config.measurement_sender,
                executor_runtime_hash: self.config.executor_runtime_hash,
                v2_adapter: self.config.v2_adapter,
                v2_adapter_runtime_hash: self.config.v2_adapter_runtime_hash,
                v3_adapter: self.config.v3_adapter,
                v3_adapter_runtime_hash: self.config.v3_adapter_runtime_hash,
                aerodrome_adapter: self.config.aerodrome_adapter,
                aerodrome_adapter_runtime_hash: self.config.aerodrome_adapter_runtime_hash,
                g0_code_identity_digest: self.config.g0_code_identity_digest,
                raw_reject_inventory_sha256: self.config.raw_reject_inventory_sha256,
                raw_reject_source_sha256: self.config.raw_reject_source_sha256,
                measurement_binding_source_sha256: self.config.measurement_binding_source_sha256,
            },
            ledger: BlinkMeasurementLedgerAuthorityAuditSnapshotV1 {
                observed: self.ledger.observed.load(Ordering::Acquire),
                accepted: self.ledger.accepted.load(Ordering::Acquire),
                slot_accepted: self.ledger.slot_accepted.load(Ordering::Acquire),
                slot_replaced: self.ledger.slot_replaced.load(Ordering::Acquire),
                slot_closed: self.ledger.slot_closed.load(Ordering::Acquire),
                generation_overflow: self.ledger.generation_overflow.load(Ordering::Acquire),
                admitted_generations: self.ledger.admitted_generations.load(Ordering::Acquire),
                processed_terminal: self.ledger.processed_terminal.load(Ordering::Acquire),
                replaced_before_frame: self.ledger.replaced_before_frame.load(Ordering::Acquire),
                cancelled_before_frame: self.ledger.cancelled_before_frame.load(Ordering::Acquire),
                selected_dto_built_preterminal: self
                    .ledger
                    .selected_dto_built_preterminal
                    .load(Ordering::Acquire),
                selected_dto_committed: self.ledger.selected_dto_committed.load(Ordering::Acquire),
                selected_dto_cancelled_before_terminal: self
                    .ledger
                    .selected_dto_cancelled_before_terminal
                    .load(Ordering::Acquire),
                selected_dto_preterminal_failure: self
                    .ledger
                    .selected_dto_preterminal_failure
                    .load(Ordering::Acquire),
                admission_closed: self.ledger.admission_closed.load(Ordering::Acquire),
                poisoned: self.ledger.poisoned.load(Ordering::Acquire),
                generations: generations.clone(),
                selected: selected.clone(),
                generations_mutex_poisoned,
                selected_mutex_poisoned,
            },
            cutoff: cutoff.clone(),
            generation_authority: generation_authority.clone(),
            record_queue,
            candidate_queue,
            staged: staged.clone(),
            staging_failures: staging_failures.clone(),
            reject_sequence: self.reject_sequence.load(Ordering::Acquire),
            candidate_sequence: self.candidate_sequence.load(Ordering::Acquire),
            candidate_drop_counts: self.candidate_pre_enqueue_drop_counters(),
            blink_reject_queue_loss: self.blink_reject_queue_loss_counters(),
            candidate_drop_queue_loss: self.candidate_drop_queue_loss_counters(),
            pending_records: self.pending_records.load(Ordering::Acquire),
            pending_candidates: self.pending_candidates.load(Ordering::Acquire),
            reject_previous_hash: *reject_previous_hash,
            poisoned: self.poisoned.load(Ordering::Acquire),
            installed: self.installed.load(Ordering::Acquire),
            cutoff_latched: self.cutoff_latched.load(Ordering::Acquire),
            admission_mutex_poisoned,
            generation_authority_mutex_poisoned,
            record_receiver_mutex_poisoned,
            candidate_receiver_mutex_poisoned,
            staged_mutex_poisoned,
            staging_failures_mutex_poisoned,
            reject_previous_hash_mutex_poisoned,
            cutoff_mutex_poisoned,
        }
    }

    /// Atomically snapshots all raw Blink, queue, candidate, drop, and cutoff-witness authority.
    pub fn raw_accounting_snapshot(
        &self,
    ) -> Result<EdgeRawAccountingSnapshotV1, EdgeProducerError> {
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let blink = self.ledger.snapshot().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let staged_count =
            self.staged.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?.len();
        let staged_count = u64::try_from(staged_count)
            .map_err(|_| self.poison(EdgeProducerError::StagedCountOverflow))?;
        let queues = EdgeQueueAccountingSnapshotV1 {
            pending_records: self.pending_records.load(Ordering::Acquire),
            pending_candidates: self.pending_candidates.load(Ordering::Acquire),
            staged_count,
        };
        let candidate_bounds = self.checked_candidate_bounds();
        let candidate_drops = self.candidate_pre_enqueue_drop_counters();
        let blink_reject_loss = self.blink_reject_queue_loss_counters();
        let candidate_drop_loss = self.candidate_drop_queue_loss_counters();
        let cutoff = self.cutoff.get().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let revision = [
            blink.victim_ingress_observed,
            blink.victim_ingress_accepted,
            blink.slot_accepted,
            blink.slot_replaced,
            blink.slot_closed,
            blink.generation_overflow,
            blink.admitted_generations,
            blink.processed_terminal,
            blink.replaced_before_frame,
            blink.cancelled_before_frame,
            blink.selected_dto_built_preterminal,
            blink.selected_dto_committed,
            blink.selected_dto_cancelled_before_terminal,
            blink.selected_dto_preterminal_failure,
            blink.generation_pending,
            blink.selected_pending,
            u64::from(blink.poisoned),
            queues.pending_records,
            queues.pending_candidates,
            queues.staged_count,
            candidate_bounds.count,
            candidate_drops.cancelled_after_draft,
            candidate_drops.candidate_queue_closed,
            candidate_drops.candidate_queue_full,
            candidate_drops.cutoff_drain_deadline,
            candidate_drops.evidence_mismatch,
            candidate_drops.failed_after_draft,
            candidate_drops.measurement_derivation_rejected,
            candidate_drops.missing_required_evidence,
            candidate_drops.stale_after_draft,
            blink_reject_loss.queue_closed,
            blink_reject_loss.queue_full,
            candidate_drop_loss.queue_closed,
            candidate_drop_loss.queue_full,
            u64::from(self.poisoned.load(Ordering::Acquire)),
            u64::from(self.cutoff_latched.load(Ordering::Acquire)),
        ];
        Ok(EdgeRawAccountingSnapshotV1 {
            revision,
            blink,
            queues,
            candidate_bounds,
            candidate_drops,
            blink_reject_loss,
            candidate_drop_loss,
            cutoff,
        })
    }

    /// Returns the same raw snapshot only after cutoff, queues, and witnesses are closed.
    pub fn cutoff_drained_snapshot(
        &self,
    ) -> Result<EdgeCutoffDrainedSnapshotV1, EdgeProducerError> {
        let EdgeRawAccountingSnapshotV1 {
            revision,
            blink,
            queues,
            candidate_bounds,
            candidate_drops,
            blink_reject_loss,
            candidate_drop_loss,
            cutoff,
        } = self.raw_accounting_snapshot()?;
        let cutoff = cutoff.ok_or(EdgeProducerError::CutoffMissing)?;
        let last_candidate_matches = candidate_bounds
            .last_sequence
            .map_or(candidate_bounds.count == 0 && cutoff.last_candidate_sequence == 0, |last| {
                last == cutoff.last_candidate_sequence
            });
        if self.is_accepting()
            || queues != EdgeQueueAccountingSnapshotV1::default()
            || !last_candidate_matches
            || !cutoff.has_valid_record_hash()
        {
            return Err(EdgeProducerError::Ledger);
        }
        BlinkMeasurementLedgerV1::verify_snapshot(&blink)
            .map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        Ok(EdgeCutoffDrainedSnapshotV1 {
            revision,
            blink,
            queues,
            candidate_bounds,
            candidate_drops,
            blink_reject_loss,
            candidate_drop_loss,
            cutoff,
        })
    }

    /// Returns an immutable exact cumulative snapshot of every candidate pre-enqueue drop reason.
    pub fn candidate_pre_enqueue_drop_counters(&self) -> CandidatePreEnqueueDropCountersV1 {
        let count = |reason: CandidatePreEnqueueDropReasonV3| {
            self.candidate_drop_counts[reason.counter_index()].load(Ordering::Acquire)
        };
        CandidatePreEnqueueDropCountersV1 {
            missing_required_evidence: count(
                CandidatePreEnqueueDropReasonV3::MissingRequiredEvidence,
            ),
            evidence_mismatch: count(CandidatePreEnqueueDropReasonV3::EvidenceMismatch),
            measurement_derivation_rejected: count(
                CandidatePreEnqueueDropReasonV3::MeasurementDerivationRejected,
            ),
            cancelled_after_draft: count(CandidatePreEnqueueDropReasonV3::CancelledAfterDraft),
            stale_after_draft: count(CandidatePreEnqueueDropReasonV3::StaleAfterDraft),
            failed_after_draft: count(CandidatePreEnqueueDropReasonV3::FailedAfterDraft),
            cutoff_drain_deadline: count(CandidatePreEnqueueDropReasonV3::CutoffDrainDeadline),
            candidate_queue_full: count(CandidatePreEnqueueDropReasonV3::CandidateQueueFull),
            candidate_queue_closed: count(CandidatePreEnqueueDropReasonV3::CandidateQueueClosed),
        }
    }

    /// Returns an immutable exact cumulative snapshot of lost Blink reject records.
    pub fn blink_reject_queue_loss_counters(&self) -> BlinkRejectQueueLossCountersV1 {
        BlinkRejectQueueLossCountersV1 {
            queue_full: self.blink_reject_queue_full.load(Ordering::Acquire),
            queue_closed: self.blink_reject_queue_closed.load(Ordering::Acquire),
        }
    }

    /// Returns an immutable exact cumulative snapshot of lost candidate-drop records.
    pub fn candidate_drop_queue_loss_counters(&self) -> CandidateDropQueueLossCountersV1 {
        CandidateDropQueueLossCountersV1 {
            queue_full: self.candidate_drop_queue_full.load(Ordering::Acquire),
            queue_closed: self.candidate_drop_queue_closed.load(Ordering::Acquire),
        }
    }

    /// Latches measurement poison after a structurally inconsistent ledger result.
    pub fn observe_ledger_result(&self, result: Result<(), EdgeMeasurementError>) {
        let Ok(_admission) = self.admission.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        self.observe_ledger_result_admitted(result);
    }

    /// Handles a ledger result while the caller holds the owner admission boundary.
    pub fn observe_ledger_result_admitted(&self, result: Result<(), EdgeMeasurementError>) {
        if let Err(error) = result {
            let reason = match error {
                EdgeMeasurementError::LedgerCapacityOverflow => {
                    Some(BlinkRejectReasonV3::LedgerCapacityOverflow)
                }
                EdgeMeasurementError::LockPoisoned => Some(BlinkRejectReasonV3::LedgerLockPoisoned),
                _ => None,
            };
            if let Some(reason) = reason
                && self.is_accepting()
                && let Some(branch_id) = BlinkRejectClassifierV3::branch_id(reason)
            {
                let result = self.try_emit_blink_reject(branch_id, reason);
                self.handle_blink_reject_emit_result(result);
            }
            if error != EdgeMeasurementError::LedgerCapacityOverflow {
                self.poison(EdgeProducerError::Ledger);
            }
        }
    }

    /// Records one successful runtime submission and preserves its epoch authority through drain.
    pub fn record_submission_admitted(&self, generation: u64, submission: SlotSubmit) {
        let result = self.ledger.record_submission(generation, submission);
        if result.is_ok() && matches!(submission, SlotSubmit::Accepted | SlotSubmit::Replaced) {
            let authority_result = self.generation_authority.lock().map(|mut authority| {
                if submission == SlotSubmit::Replaced
                    && let Some(replaced) = authority.range(..generation).next_back().copied()
                {
                    authority.remove(&replaced);
                }
                authority.insert(generation)
            });
            if !matches!(authority_result, Ok(true)) {
                self.poison(EdgeProducerError::Ledger);
                return;
            }
        }
        self.observe_ledger_result_admitted(result);
    }

    /// Terminalizes queued shutdown generations and releases their epoch authority tokens.
    pub fn terminalize_shutdown_pending_admitted(&self, active_generation: Option<u64>) {
        let result = self.ledger.terminalize_shutdown_pending(active_generation);
        if result.is_ok() {
            let authority_result = self.generation_authority.lock().map(|mut authority| {
                authority.retain(|generation| Some(*generation) == active_generation);
                authority.is_empty()
            });
            match authority_result {
                Ok(true) => self.generation_authority_drained.notify_all(),
                Ok(false) => {}
                Err(_) => {
                    self.poison(EdgeProducerError::Ledger);
                    return;
                }
            }
        }
        self.observe_ledger_result_admitted(result);
    }

    fn generation_is_authoritative(&self, generation: u64) -> Result<bool, EdgeProducerError> {
        self.generation_authority
            .lock()
            .map(|authority| authority.contains(&generation))
            .map_err(|_| self.poison(EdgeProducerError::Ledger))
    }

    fn release_generation_authority(&self, generation: u64) {
        let Ok(mut authority) = self.generation_authority.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        if !authority.remove(&generation) {
            self.poison(EdgeProducerError::Ledger);
            return;
        }
        if authority.is_empty() {
            self.generation_authority_drained.notify_all();
        }
    }

    /// Stages one complete generation-keyed same-frame draft without publishing a candidate.
    #[cfg(feature = "edge-measurement")]
    pub fn stage_selected_candidate(
        &self,
        input: EdgeCandidateStageInputV3<'_>,
    ) -> Result<(), EdgeProducerError> {
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        if !self.generation_is_authoritative(input.generation)? {
            if !self.is_accepting() {
                return Ok(());
            }
            self.record_candidate_drop(
                input.generation,
                CandidatePreEnqueueDropReasonV3::MissingRequiredEvidence,
            );
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let generation = input.generation;
        let result = self.try_stage_selected_candidate(input);
        if let Err(error) = result {
            let reason = match error {
                EdgeProducerError::MissingCandidateEvidence => {
                    CandidatePreEnqueueDropReasonV3::MissingRequiredEvidence
                }
                EdgeProducerError::MeasurementBindingDerivation => {
                    CandidatePreEnqueueDropReasonV3::MeasurementDerivationRejected
                }
                _ => CandidatePreEnqueueDropReasonV3::EvidenceMismatch,
            };
            let failure_recorded = self
                .staging_failures
                .lock()
                .map(|mut failures| failures.insert(generation))
                .unwrap_or(false);
            if !failure_recorded {
                self.poison(EdgeProducerError::Ledger);
                return Err(EdgeProducerError::Ledger);
            }
            self.record_candidate_drop(generation, reason);
        }
        result
    }

    #[cfg(feature = "edge-measurement")]
    fn try_stage_selected_candidate(
        &self,
        input: EdgeCandidateStageInputV3<'_>,
    ) -> Result<(), EdgeProducerError> {
        MeasurementEncoder::validate(input.plan)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let generation = input.generation;
        let draft = self.build_selected_draft(input)?;
        let mut staged = self.staged.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        if staged.len() >= BLINK_LEDGER_CAPACITY || staged.contains_key(&generation) {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        staged.insert(generation, draft);
        self.observe_ledger_result_admitted(
            self.ledger.record_selected_preterminal_authorized(generation),
        );
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(EdgeProducerError::Ledger);
        }
        Ok(())
    }

    /// Emits a named fail-closed terminal when same-frame draft staging fails.
    pub fn emit_candidate_drop(&self, generation: u64, reason: CandidatePreEnqueueDropReasonV3) {
        let Ok(_admission) = self.admission.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        if !self.is_accepting() {
            return;
        }
        self.record_candidate_drop(generation, reason);
    }

    /// Emits exactly one named record for an actual Blink branch.
    pub fn emit_blink_reject(&self, branch_id: &'static str, reason: BlinkRejectReasonV3) {
        let Ok(_admission) = self.admission.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        self.emit_blink_reject_admitted(branch_id, reason);
    }

    /// Emits one reject while the caller holds the owner admission boundary.
    pub fn emit_blink_reject_admitted(&self, branch_id: &'static str, reason: BlinkRejectReasonV3) {
        if !self.is_accepting() {
            return;
        }
        let result = self.try_emit_blink_reject(branch_id, reason);
        self.handle_blink_reject_emit_result(result);
    }

    fn handle_blink_reject_emit_result(&self, result: Result<(), EdgeProducerError>) {
        let counter = match result {
            Ok(()) => return,
            Err(EdgeProducerError::QueueFull) => &self.blink_reject_queue_full,
            Err(EdgeProducerError::QueueClosed) => &self.blink_reject_queue_closed,
            Err(error) => {
                self.poison(error);
                return;
            }
        };
        if counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
            .is_err()
        {
            self.poison(EdgeProducerError::SequenceOverflow);
        }
    }

    fn handle_candidate_drop_emit_result(&self, result: Result<(), EdgeProducerError>) {
        let counter = match result {
            Ok(()) => return,
            Err(EdgeProducerError::QueueFull) => &self.candidate_drop_queue_full,
            Err(EdgeProducerError::QueueClosed) => &self.candidate_drop_queue_closed,
            Err(error) => {
                self.poison(error);
                return;
            }
        };
        if counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
            .is_err()
        {
            self.poison(EdgeProducerError::SequenceOverflow);
        }
    }

    fn authority_record_hash(
        domain: &str,
        canonical_record: &str,
    ) -> Result<[u8; 32], EdgeProducerError> {
        let mut bytes = Vec::new();
        for field in [
            b"base-edge-authority-record-v1\0".as_slice(),
            domain.as_bytes(),
            canonical_record.as_bytes(),
        ] {
            let length =
                u32::try_from(field.len()).map_err(|_| EdgeProducerError::SequenceOverflow)?;
            bytes.extend_from_slice(&length.to_be_bytes());
            bytes.extend_from_slice(field);
        }
        Ok(DefaultCrypto.sha256(&bytes))
    }

    fn try_emit_blink_reject(
        &self,
        branch_id: &'static str,
        reason: BlinkRejectReasonV3,
    ) -> Result<(), EdgeProducerError> {
        if !BLINK_REJECT_BRANCH_INVENTORY_V3.contains(&(branch_id, reason)) {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let mut previous =
            self.reject_previous_hash.lock().map_err(|_| EdgeProducerError::Ledger)?;
        let sequence = self.reject_sequence.load(Ordering::Acquire);
        let next_sequence = sequence.checked_add(1).ok_or(EdgeProducerError::SequenceOverflow)?;
        let canonical = format!(
            "{{\"branchId\":\"{}\",\"previousRecordHash\":\"{}\",\"producerEpoch\":\"{}\",\"reason\":\"{}\",\"schema\":\"edge-blink-reject/v3\",\"sequence\":\"{}\",\"state\":\"Rejected\"}}",
            branch_id,
            hex::encode(*previous),
            self.config.producer_epoch,
            reason.wire_name(),
            sequence,
        );
        let record_hash = Self::authority_record_hash("edge-blink-reject/v3", &canonical)?;
        let record = BlinkRejectRecordV3 {
            schema: "edge-blink-reject/v3",
            producer_epoch: self.config.producer_epoch.to_string(),
            sequence: sequence.to_string(),
            state: "Rejected",
            branch_id,
            reason,
            previous_record_hash: hex::encode(*previous),
            record_hash: hex::encode(record_hash),
        };
        self.try_send_record(EdgeProducerRecordV1::BlinkReject(record))?;
        self.reject_sequence.store(next_sequence, Ordering::Release);
        *previous = record_hash;
        Ok(())
    }

    fn record_candidate_drop(&self, generation: u64, reason: CandidatePreEnqueueDropReasonV3) {
        if self.candidate_drop_counts[reason.counter_index()]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
            .is_err()
        {
            self.poison(EdgeProducerError::SequenceOverflow);
            return;
        }
        let result =
            self.try_send_record(EdgeProducerRecordV1::CandidateDrop { generation, reason });
        self.handle_candidate_drop_emit_result(result);
    }

    fn try_send_record(&self, record: EdgeProducerRecordV1) -> Result<(), EdgeProducerError> {
        self.pending_records
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| pending.checked_add(1))
            .map_err(|_| EdgeProducerError::SequenceOverflow)?;
        if let Err(error) = self.record_sender.try_send(record) {
            self.pending_records.fetch_sub(1, Ordering::AcqRel);
            return Err(match error {
                TrySendError::Full(_) => EdgeProducerError::QueueFull,
                TrySendError::Disconnected(_) => EdgeProducerError::QueueClosed,
            });
        }
        Ok(())
    }
    fn try_send_candidate<T>(
        sender: &SyncSender<T>,
        candidate: T,
    ) -> Result<(), CandidatePreEnqueueDropReasonV3> {
        sender.try_send(candidate).map_err(|error| match error {
            TrySendError::Full(_) => CandidatePreEnqueueDropReasonV3::CandidateQueueFull,
            TrySendError::Disconnected(_) => CandidatePreEnqueueDropReasonV3::CandidateQueueClosed,
        })
    }

    #[cfg(feature = "edge-measurement")]
    fn build_selected_draft(
        &self,
        input: EdgeCandidateStageInputV3<'_>,
    ) -> Result<EdgeCandidateEvidenceV3, EdgeProducerError> {
        let EdgeCandidateStageInputV3 {
            generation,
            port,
            snapshot,
            processed,
            prepared,
            plan,
            victim_raw,
            probe,
        } = input;
        let source =
            snapshot.edge_evidence().map_err(|_| EdgeProducerError::MissingCandidateEvidence)?;
        let context = processed.measurement_context();
        let ordered_transaction_cutoff_position = snapshot.latest_block_transaction_count();
        if ordered_transaction_cutoff_position > EDGE_MAX_ORDERED_TRANSACTIONS {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let ordered_transaction_count = ordered_transaction_cutoff_position
            .checked_add(1)
            .and_then(|count| u64::try_from(count).ok())
            .ok_or(EdgeProducerError::CandidateEvidenceMismatch)?;
        let mut ordered_visitor = OrderedTransactionReceiptVisitor::new(
            plan.block_number,
            ordered_transaction_cutoff_position,
            plan.victim,
        )?;
        let ordered_summary = snapshot
            .visit_transactions_for_block(
                plan.block_number,
                0,
                ordered_transaction_cutoff_position,
                &mut ordered_visitor,
            )
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?;
        if !ordered_summary.complete
            || usize::try_from(ordered_summary.visited).ok()
                != Some(ordered_transaction_cutoff_position)
            || ordered_visitor.next_position != ordered_transaction_cutoff_position
        {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let ordered_transaction_digest =
            B256::new(DefaultCrypto.sha256(&ordered_visitor.canonical));
        if source.payload_first_record_hash.is_zero()
            || source.structural_terminal_hash.is_zero()
            || source.connection_record_hash.is_zero()
            || source.registry_terminal_record_hash.is_zero()
            || snapshot.parent_hash() != plan.parent_hash
            || snapshot.latest_block_number() != plan.block_number
            || context.parent_hash != plan.parent_hash
            || context.block_number != plan.block_number
            || context.predecessor_index != plan.predecessor_index
            || context.payload_id != plan.payload_id
            || context.victim != plan.victim
            || keccak256(victim_raw) != plan.victim
            || snapshot.has_transaction_hash(plan.victim)
            || snapshot.transaction_position(plan.block_number, plan.victim).is_some()
            || victim_raw.len() > EDGE_MAX_VICTIM_RAW_BYTES
            || !probe.checkpoint(Instant::now(), port.is_current_authoritative(snapshot))
        {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let route = plan.route.each_ref().map(|hop| {
            prepared
                .iter()
                .find(|pool| {
                    pool.pool == hop.pool
                        && pool.protocol == hop.protocol
                        && pool.fee_pips == hop.fee_pips
                        && ((pool.token0 == hop.token_in && pool.token1 == hop.token_out)
                            || (pool.token1 == hop.token_in && pool.token0 == hop.token_out))
                })
                .cloned()
        });
        let [Some(first), Some(second)] = route else {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        };
        let prepared_route = [first, second];
        let first_out = prepared_route[0]
            .quote_exact_in(plan.route[0].token_in, plan.amount_in, probe)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let second_out = prepared_route[1]
            .quote_exact_in(plan.route[1].token_in, first_out, probe)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        if first_out.is_zero() || second_out.is_zero() || second_out != plan.amount_out {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }

        let provider = port
            .state_at_hash(plan.parent_hash)
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?;
        let sender = provider
            .basic_account(&self.config.measurement_sender)
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?
            .ok_or(EdgeProducerError::MissingCandidateEvidence)?;
        let parent_header = port
            .sealed_header_at_hash(plan.parent_hash)
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?;
        if parent_header.hash() != plan.parent_hash
            || parent_header.number != snapshot.canonical_block_number()
            || snapshot.canonical_block_number().checked_add(1) != Some(plan.block_number)
        {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let identities = [
            (crate::MEASUREMENT_EXECUTOR, self.config.executor_runtime_hash),
            (self.config.v2_adapter, self.config.v2_adapter_runtime_hash),
            (self.config.v3_adapter, self.config.v3_adapter_runtime_hash),
            (self.config.aerodrome_adapter, self.config.aerodrome_adapter_runtime_hash),
        ];
        for (address, expected) in identities {
            let actual = provider
                .basic_account(&address)
                .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?
                .and_then(|account| account.bytecode_hash);
            if actual != Some(expected) {
                return Err(EdgeProducerError::CandidateEvidenceMismatch);
            }
        }
        let code_witness_digest = Self::deployment_identity_digest(&identities);
        if code_witness_digest != self.config.g0_code_identity_digest {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }

        let overlay = snapshot
            .pending_account_nonce(self.config.measurement_sender)
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?;
        let nonce = overlay.map_or_else(
            || {
                SnapshotNonceWitnessV1::committed(
                    plan.parent_hash,
                    snapshot.canonical_block_number(),
                    sender.nonce,
                )
            },
            |overlay| {
                SnapshotNonceWitnessV1::pending(
                    plan.parent_hash,
                    snapshot.canonical_block_number(),
                    sender.nonce,
                    overlay.original_nonce(),
                    overlay.current_nonce(),
                )
            },
        );
        let execution_hops = std::array::from_fn(|index| {
            let protocol = plan.route[index].protocol;
            let (adapter, funding_target) = match protocol {
                ExactProtocol::UniswapV2 => (self.config.v2_adapter, plan.route[index].pool),
                ExactProtocol::UniswapV3 => (self.config.v3_adapter, self.config.v3_adapter),
                ExactProtocol::AerodromeVolatile | ExactProtocol::AerodromeStable => {
                    (self.config.aerodrome_adapter, plan.route[index].pool)
                }
            };
            MeasurementExecutionHopV1 {
                adapter,
                min_amount_out: if index == 0 { first_out } else { second_out },
                funding_target,
            }
        });
        let pending_header = snapshot.latest_header();
        if pending_header.number != plan.block_number {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let snapshot_base_fee_per_gas = pending_header
            .base_fee_per_gas
            .map(u128::from)
            .filter(|fee| *fee != 0)
            .ok_or(EdgeProducerError::CandidateEvidenceMismatch)?;
        let binding = MeasurementBindingDeriverV1::bind(MeasurementBindingInputV1 {
            nonce,
            snapshot_base_fee_per_gas,
            plan: plan.clone(),
            execution_hops,
            victim_raw: victim_raw.clone(),
        })
        .map_err(|_| EdgeProducerError::MeasurementBindingDerivation)?;

        let materialized_state = processed.materialized_state().clone();
        let prepared_state_digest = Self::prepared_route_digest(&prepared_route)?;
        let slot_witness_digest = Self::materialized_state_digest(&materialized_state)?;
        let economics_evidence_digest = Self::economics_digest(plan, &execution_hops)?;
        Ok(EdgeCandidateEvidenceV3 {
            generation,
            source_generation: source.source_generation,
            coverage_generation: source.coverage_sequence,
            pending_snapshot_sequence: source.pending_snapshot_sequence,
            payload_first_record_sequence: source.payload_first_record_sequence,
            payload_first_record_hash: source.payload_first_record_hash.0,
            structural_terminal_hash: source.structural_terminal_hash.0,
            connection_sequence_at_capture: source.connection_sequence,
            connection_record_hash_at_capture: source.connection_record_hash.0,
            registry_terminal_record_hash: source.registry_terminal_record_hash.0,
            parent_hash: plan.parent_hash,
            state_root: parent_header.state_root,
            ordered_transaction_count,
            victim_absent_before_position: true,
            ordered_transaction_cutoff_position: u64::try_from(ordered_transaction_cutoff_position)
                .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?,
            ordered_transaction_digest,
            victim_raw: victim_raw.clone(),
            selected_plan: plan.clone(),
            prepared_route,
            materialized_state,
            prepared_state_digest,
            code_witness_digest,
            slot_witness_digest,
            economics_evidence_digest,
            execution_hops,
            deployment_identities: identities,
            backrun_measurement_binding: binding,
        })
    }

    /// SHA-256 layout: domain, `u32be` count, then repeated 20-byte address and 32-byte hash.
    fn deployment_identity_digest(identities: &[(Address, B256); 4]) -> B256 {
        let mut bytes = Vec::with_capacity(34 + 4 * 52);
        bytes.extend_from_slice(b"edge-deployment-identities/v1\0");
        bytes.extend_from_slice(&4_u32.to_be_bytes());
        for (address, runtime_hash) in identities {
            bytes.extend_from_slice(address.as_slice());
            bytes.extend_from_slice(runtime_hash.as_slice());
        }
        B256::new(DefaultCrypto.sha256(&bytes))
    }

    /// SHA-256 layout: domain, `u32be` pool count, then fixed pool fields and tagged quote data.
    ///
    /// Integers are big-endian. Each quote starts with a one-byte tag (`0` constant product,
    /// `1` stable, `2` V3). V3 appends a `u32be` tick count and repeated `i32be` tick plus
    /// 32-byte two's-complement liquidity net.
    fn prepared_route_digest(
        prepared_route: &[PreparedPoolState; 2],
    ) -> Result<B256, EdgeProducerError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"edge-prepared-route/v1\0");
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        for pool in prepared_route {
            bytes.extend_from_slice(pool.pool.as_slice());
            bytes.push(pool.protocol as u8);
            bytes.extend_from_slice(pool.token0.as_slice());
            bytes.extend_from_slice(pool.token1.as_slice());
            bytes.push(pool.decimals0);
            bytes.push(pool.decimals1);
            bytes.extend_from_slice(&pool.fee_pips.to_be_bytes());
            match &pool.quote {
                PreparedPoolQuote::ConstantProduct { reserve0, reserve1 } => {
                    bytes.push(0);
                    Self::push_u256(&mut bytes, *reserve0);
                    Self::push_u256(&mut bytes, *reserve1);
                }
                PreparedPoolQuote::Stable { reserve0, reserve1 } => {
                    bytes.push(1);
                    Self::push_u256(&mut bytes, *reserve0);
                    Self::push_u256(&mut bytes, *reserve1);
                }
                PreparedPoolQuote::V3 { sqrt_price_x96, liquidity, tick, tick_spacing, ticks } => {
                    bytes.push(2);
                    Self::push_u256(&mut bytes, *sqrt_price_x96);
                    Self::push_u256(&mut bytes, *liquidity);
                    bytes.extend_from_slice(&tick.to_be_bytes());
                    bytes.extend_from_slice(&tick_spacing.to_be_bytes());
                    let tick_count = u32::try_from(ticks.len())
                        .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
                    bytes.extend_from_slice(&tick_count.to_be_bytes());
                    for initialized in ticks {
                        bytes.extend_from_slice(&initialized.tick.to_be_bytes());
                        bytes.extend_from_slice(
                            &initialized.liquidity_net.into_raw().to_be_bytes::<32>(),
                        );
                    }
                }
            }
        }
        Ok(B256::new(DefaultCrypto.sha256(&bytes)))
    }

    /// SHA-256 layout: domain, `u32be` write count, then variant byte, 20-byte address,
    /// 32-byte slot (zero for account variants), 32-byte evidence digest, and 32-byte value.
    fn materialized_state_digest(
        materialized: &MaterializedState,
    ) -> Result<B256, EdgeProducerError> {
        let account_count = materialized
            .writes
            .iter()
            .filter(|write| !matches!(write.key, AuditedWriteKey::Storage { .. }))
            .count();
        let storage_count = materialized.writes.len().saturating_sub(account_count);
        if account_count > MAX_ACCOUNTS || storage_count > MAX_STORAGE_SLOTS {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        if materialized.writes.windows(2).any(|pair| pair[0].key >= pair[1].key)
            || materialized.writes.iter().any(|write| write.key.evidence_digest().is_zero())
        {
            return Err(EdgeProducerError::CandidateEvidenceMismatch);
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"edge-materialized-state/v1\0");
        let write_count = u32::try_from(materialized.writes.len())
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        bytes.extend_from_slice(&write_count.to_be_bytes());
        for write in &materialized.writes {
            let (variant, address, slot, evidence_digest) = match write.key {
                AuditedWriteKey::AccountBalance { address, evidence_digest } => {
                    (0, address, U256::ZERO, evidence_digest)
                }
                AuditedWriteKey::AccountNonce { address, evidence_digest } => {
                    (1, address, U256::ZERO, evidence_digest)
                }
                AuditedWriteKey::Storage { address, slot, evidence_digest } => {
                    (2, address, slot, evidence_digest)
                }
            };
            bytes.push(variant);
            bytes.extend_from_slice(address.as_slice());
            Self::push_u256(&mut bytes, slot);
            bytes.extend_from_slice(evidence_digest.as_slice());
            Self::push_u256(&mut bytes, write.value);
        }
        Ok(B256::new(DefaultCrypto.sha256(&bytes)))
    }

    /// SHA-256 layout: domain, `u32be` canonical-plan byte length, exact
    /// [`MeasurementEncoder::encode`] bytes, `u32be` hop count, then repeated 20-byte adapter,
    /// 32-byte minimum output, and 20-byte funding target.
    fn economics_digest(
        plan: &BackrunPlan,
        execution_hops: &[MeasurementExecutionHopV1; 2],
    ) -> Result<B256, EdgeProducerError> {
        let plan_bytes = MeasurementEncoder::encode(plan)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let plan_length = u32::try_from(plan_bytes.len())
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"edge-economics-evidence/v1\0");
        bytes.extend_from_slice(&plan_length.to_be_bytes());
        bytes.extend_from_slice(&plan_bytes);
        bytes.extend_from_slice(&(execution_hops.len() as u32).to_be_bytes());
        for hop in execution_hops {
            bytes.extend_from_slice(hop.adapter.as_slice());
            Self::push_u256(&mut bytes, hop.min_amount_out);
            bytes.extend_from_slice(hop.funding_target.as_slice());
        }
        Ok(B256::new(DefaultCrypto.sha256(&bytes)))
    }

    fn push_u256(bytes: &mut Vec<u8>, value: U256) {
        bytes.extend_from_slice(&value.to_be_bytes::<32>());
    }

    /// Atomically terminalizes one generation and resolves its staged candidate under admission.
    pub fn record_terminal_and_resolve(
        &self,
        generation: u64,
        terminal: BlinkGenerationTerminalV1,
        outcome: Option<ShadowOutcome>,
    ) {
        let Ok(_admission) = self.admission.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        match self.generation_is_authoritative(generation) {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
        self.observe_ledger_result_admitted(
            self.ledger.record_terminal(generation, terminal, outcome),
        );
        if self.poisoned.load(Ordering::SeqCst) {
            self.release_generation_authority(generation);
            return;
        }
        let Ok(mut staged) = self.staged.lock() else {
            self.poison(EdgeProducerError::Ledger);
            self.release_generation_authority(generation);
            return;
        };
        let resolved = staged.remove(&generation);
        drop(staged);
        let staging_failed = match self.staging_failures.lock() {
            Ok(mut failures) => failures.remove(&generation),
            Err(_) => {
                self.poison(EdgeProducerError::Ledger);
                self.release_generation_authority(generation);
                return;
            }
        };
        if outcome == Some(ShadowOutcome::Selected) && resolved.is_none() && !staging_failed {
            self.poison(EdgeProducerError::CandidateEvidenceMismatch);
            self.release_generation_authority(generation);
            return;
        }
        let Some(draft) = resolved else {
            self.release_generation_authority(generation);
            return;
        };
        if outcome == Some(ShadowOutcome::Selected) {
            let sequence = self.candidate_sequence.load(Ordering::Acquire);
            let Some(next_sequence) = sequence.checked_add(1) else {
                self.poison(EdgeProducerError::SequenceOverflow);
                self.release_generation_authority(generation);
                return;
            };
            let detail = EdgeCandidateDetailV1::new(&self.config, sequence, draft);
            if detail.validate_detail().is_err() {
                self.poison(EdgeProducerError::CandidateEvidenceMismatch);
                self.release_generation_authority(generation);
                return;
            }
            if self
                .pending_candidates
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| pending.checked_add(1))
                .is_err()
            {
                self.poison(EdgeProducerError::SequenceOverflow);
                self.release_generation_authority(generation);
                return;
            }
            if let Err(reason) = Self::try_send_candidate(&self.candidate_sender, detail) {
                self.pending_candidates.fetch_sub(1, Ordering::AcqRel);
                self.record_candidate_drop(generation, reason);
                self.release_generation_authority(generation);
                return;
            }
            self.candidate_sequence.store(next_sequence, Ordering::Release);
            self.release_generation_authority(generation);
            return;
        }
        let reason = match outcome {
            Some(ShadowOutcome::Cancelled) => CandidatePreEnqueueDropReasonV3::CancelledAfterDraft,
            Some(ShadowOutcome::InternalFailure) | None => {
                CandidatePreEnqueueDropReasonV3::FailedAfterDraft
            }
            _ => CandidatePreEnqueueDropReasonV3::StaleAfterDraft,
        };
        self.record_candidate_drop(generation, reason);
        self.release_generation_authority(generation);
    }

    /// Fences new measurement admission, drains every pre-cutoff generation authority, and then
    /// returns stable Blink and candidate bounds.
    pub fn prepare_cutoff(&self) -> Result<(u64, CheckedCandidateBoundsV1), EdgeProducerError> {
        self.prepare_cutoff_until(Instant::now() + BLINK_CUTOFF_DRAIN_DEADLINE_V1)
    }

    fn prepare_cutoff_until(
        &self,
        deadline: Instant,
    ) -> Result<(u64, CheckedCandidateBoundsV1), EdgeProducerError> {
        let admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        self.cutoff_latched.store(true, Ordering::Release);
        self.ledger.close_admission();
        drop(admission);

        let authority =
            self.generation_authority.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let wait = deadline.saturating_duration_since(Instant::now());
        let (authority, timeout) = self
            .generation_authority_drained
            .wait_timeout_while(authority, wait, |authority| !authority.is_empty())
            .map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let unresolved = timeout.timed_out() && !authority.is_empty();
        drop(authority);
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        self.sweep_cutoff_deadline_drafts()?;
        if unresolved {
            self.terminalize_shutdown_pending_admitted(None);
        }
        let admitted_blink_generations = self
            .ledger
            .snapshot()
            .map_err(|_| self.poison(EdgeProducerError::Ledger))?
            .admitted_generations;
        Ok((admitted_blink_generations, self.checked_candidate_bounds()))
    }

    fn sweep_cutoff_deadline_drafts(&self) -> Result<(), EdgeProducerError> {
        let generations = {
            let mut staged =
                self.staged.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
            let generations = staged.keys().copied().collect::<Vec<_>>();
            staged.clear();
            generations
        };
        for generation in generations {
            self.record_candidate_drop(
                generation,
                CandidatePreEnqueueDropReasonV3::CutoffDrainDeadline,
            );
        }
        self.staging_failures.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?.clear();
        Ok(())
    }
    /// Latches the exact current-epoch cutoff.
    pub fn latch_cutoff(&self, fields: ProducerEpochCutoffFieldsV1) {
        let Ok(_admission) = self.admission.lock() else {
            self.poison(EdgeProducerError::Ledger);
            return;
        };
        let cutoff = ProducerEpochCutoffV1::new(fields);
        self.cutoff_latched.store(true, Ordering::Release);
        self.ledger.close_admission();
        let result = if fields.producer_epoch != self.config.producer_epoch {
            Err(EdgeMeasurementError::CutoffConflict)
        } else {
            self.cutoff.latch(cutoff)
        };
        if result.is_err() {
            self.poison(EdgeProducerError::Ledger);
        }
    }

    /// Nonblocking drain of all currently queued producer records.
    pub fn drain_records(&self) -> Result<Vec<EdgeProducerRecordV1>, EdgeProducerError> {
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let receiver =
            self.record_receiver.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let mut records = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(record) => {
                    self.pending_records
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                            pending.checked_sub(1)
                        })
                        .map_err(|_| self.poison(EdgeProducerError::Ledger))?;
                    records.push(record);
                }
                Err(TryRecvError::Empty) => return Ok(records),
                Err(TryRecvError::Disconnected) => {
                    return Err(EdgeProducerError::QueueClosed);
                }
            }
        }
    }

    /// Receives at most one campaign-time selected candidate detail without blocking.
    ///
    /// `Ok(None)` means the queue is currently empty. A disconnected queue returns
    /// [`EdgeProducerError::QueueClosed`].
    pub fn try_receive_candidate_detail(
        &self,
    ) -> Result<Option<EdgeCandidateDetailV1>, EdgeProducerError> {
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let receiver =
            self.candidate_receiver.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        match receiver.try_recv() {
            Ok(detail) => {
                self.pending_candidates
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                        pending.checked_sub(1)
                    })
                    .map_err(|_| self.poison(EdgeProducerError::Ledger))?;
                Ok(Some(detail))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(EdgeProducerError::QueueClosed),
        }
    }
    /// Nonblocking drain of all campaign-time selected candidate details.
    pub fn drain_candidate_details(&self) -> Result<Vec<EdgeCandidateDetailV1>, EdgeProducerError> {
        let _admission =
            self.admission.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let receiver =
            self.candidate_receiver.lock().map_err(|_| self.poison(EdgeProducerError::Ledger))?;
        let mut details = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(detail) => {
                    self.pending_candidates
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                            pending.checked_sub(1)
                        })
                        .map_err(|_| self.poison(EdgeProducerError::Ledger))?;
                    details.push(detail);
                }
                Err(TryRecvError::Empty) => return Ok(details),
                Err(TryRecvError::Disconnected) => {
                    return Err(EdgeProducerError::QueueClosed);
                }
            }
        }
    }
    fn poison(&self, error: EdgeProducerError) -> EdgeProducerError {
        self.poisoned.store(true, Ordering::SeqCst);
        self.cutoff_latched.store(true, Ordering::Release);
        self.ledger.close_admission();
        error
    }
}

/// Atomic, bounded generation and selected-preterminal ledger.
#[derive(Debug)]
pub struct BlinkMeasurementLedgerV1 {
    observed: AtomicU64,
    accepted: AtomicU64,
    slot_accepted: AtomicU64,
    slot_replaced: AtomicU64,
    slot_closed: AtomicU64,
    generation_overflow: AtomicU64,
    admitted_generations: AtomicU64,
    processed_terminal: AtomicU64,
    replaced_before_frame: AtomicU64,
    cancelled_before_frame: AtomicU64,
    selected_dto_built_preterminal: AtomicU64,
    selected_dto_committed: AtomicU64,
    selected_dto_cancelled_before_terminal: AtomicU64,
    selected_dto_preterminal_failure: AtomicU64,
    admission_closed: AtomicBool,
    poisoned: AtomicBool,
    generations: Mutex<BTreeMap<u64, Option<BlinkGenerationTerminalV1>>>,
    selected: Mutex<BTreeMap<u64, Option<SelectedDtoTerminalV1>>>,
}

impl Default for BlinkMeasurementLedgerV1 {
    fn default() -> Self {
        Self {
            observed: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            slot_accepted: AtomicU64::new(0),
            slot_replaced: AtomicU64::new(0),
            slot_closed: AtomicU64::new(0),
            generation_overflow: AtomicU64::new(0),
            admitted_generations: AtomicU64::new(0),
            processed_terminal: AtomicU64::new(0),
            replaced_before_frame: AtomicU64::new(0),
            cancelled_before_frame: AtomicU64::new(0),
            selected_dto_built_preterminal: AtomicU64::new(0),
            selected_dto_committed: AtomicU64::new(0),
            selected_dto_cancelled_before_terminal: AtomicU64::new(0),
            selected_dto_preterminal_failure: AtomicU64::new(0),
            admission_closed: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            generations: Mutex::new(BTreeMap::new()),
            selected: Mutex::new(BTreeMap::new()),
        }
    }
}

impl BlinkMeasurementLedgerV1 {
    /// Closes new victim, generation, and selected-DTO admission while allowing terminal drain.
    pub fn close_admission(&self) {
        self.admission_closed.store(true, Ordering::Release);
    }

    /// Records one decoded victim presented to runtime admission.
    pub fn record_observed(&self) -> Result<(), EdgeMeasurementError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        Self::increment(&self.observed)
    }

    /// Records generation allocation overflow after runtime accepted the decoded victim.
    pub fn record_generation_overflow(&self) -> Result<(), EdgeMeasurementError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        Self::increment(&self.accepted)?;
        Self::increment(&self.generation_overflow)
    }

    /// Records lifecycle closure before a generation could be allocated.
    pub fn record_slot_closed(&self) -> Result<(), EdgeMeasurementError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        Self::increment(&self.accepted)?;
        Self::increment(&self.slot_closed)
    }

    /// Records the exact capacity-one slot result for a checked generation.
    pub fn record_submission(
        &self,
        generation: u64,
        result: SlotSubmit,
    ) -> Result<(), EdgeMeasurementError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        Self::increment(&self.accepted)?;
        match result {
            SlotSubmit::Closed => Self::increment(&self.slot_closed),
            SlotSubmit::Accepted | SlotSubmit::Replaced => {
                let mut generations = self
                    .generations
                    .lock()
                    .map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
                if generations.contains_key(&generation) {
                    return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                }

                let replaced_generation = if result == SlotSubmit::Replaced {
                    let Some((&replaced_generation, _)) =
                        generations.iter().rev().find(|(candidate, terminal)| {
                            **candidate < generation && terminal.is_none()
                        })
                    else {
                        return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                    };
                    Some(replaced_generation)
                } else {
                    if generations.len() >= BLINK_LEDGER_CAPACITY {
                        Self::increment(&self.generation_overflow)
                            .map_err(|error| self.poison(error))?;
                        return Err(EdgeMeasurementError::LedgerCapacityOverflow);
                    }
                    None
                };

                let mut selected = self
                    .selected
                    .lock()
                    .map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
                Self::increment(&self.admitted_generations).map_err(|error| self.poison(error))?;
                if let Some(replaced_generation) = replaced_generation {
                    let Some(current) = generations.get_mut(&replaced_generation) else {
                        return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                    };
                    if current.replace(BlinkGenerationTerminalV1::ReplacedBeforeFrame).is_some() {
                        return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                    }
                    Self::increment(&self.replaced_before_frame)
                        .and_then(|()| Self::increment(&self.slot_replaced))
                        .map_err(|error| self.poison(error))?;
                    generations.remove(&replaced_generation);
                    if let Some(mut current) = selected.remove(&replaced_generation) {
                        if current.replace(SelectedDtoTerminalV1::PreterminalFailure).is_some() {
                            return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
                        }
                        Self::increment(&self.selected_dto_preterminal_failure)
                            .map_err(|error| self.poison(error))?;
                    }
                } else {
                    Self::increment(&self.slot_accepted).map_err(|error| self.poison(error))?;
                }
                generations.insert(generation, None);
                Ok(())
            }
        }
    }

    /// Records construction of one selected DTO while ordinary admission remains open.
    pub fn record_selected_preterminal(&self, generation: u64) -> Result<(), EdgeMeasurementError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.record_selected_preterminal_authorized(generation)
    }

    /// Records a selected DTO for an owner-validated pre-cutoff generation authority token.
    pub fn record_selected_preterminal_authorized(
        &self,
        generation: u64,
    ) -> Result<(), EdgeMeasurementError> {
        let mut generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let Some(generation_terminal) = generations.get_mut(&generation) else {
            return Err(self.poison(EdgeMeasurementError::GenerationConflict));
        };
        if generation_terminal.is_some() {
            return Err(self.poison(EdgeMeasurementError::GenerationConflict));
        }
        let mut selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        if selected.contains_key(&generation) {
            return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
        }
        Self::increment(&self.selected_dto_built_preterminal)
            .map_err(|error| self.poison(error))?;
        selected.insert(generation, None);
        Ok(())
    }

    /// Terminalizes one admitted generation and evicts its selected preterminal, when present.
    pub fn record_terminal(
        &self,
        generation: u64,
        terminal: BlinkGenerationTerminalV1,
        shadow: Option<ShadowOutcome>,
    ) -> Result<(), EdgeMeasurementError> {
        let mut generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let Some(current) = generations.get_mut(&generation) else {
            return Err(self.poison(EdgeMeasurementError::GenerationConflict));
        };
        if current.replace(terminal).is_some() {
            return Err(self.poison(EdgeMeasurementError::GenerationConflict));
        }

        let mut selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let selected_terminal = selected.get_mut(&generation).map(|current| {
            let selected_terminal = match shadow {
                Some(ShadowOutcome::Selected) => SelectedDtoTerminalV1::Committed,
                Some(ShadowOutcome::Cancelled) => SelectedDtoTerminalV1::CancelledBeforeTerminal,
                _ => SelectedDtoTerminalV1::PreterminalFailure,
            };
            (current, selected_terminal)
        });
        if let Some((current, selected_terminal)) = selected_terminal {
            if current.replace(selected_terminal).is_some() {
                return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
            }
            let counter = match selected_terminal {
                SelectedDtoTerminalV1::Committed => &self.selected_dto_committed,
                SelectedDtoTerminalV1::CancelledBeforeTerminal => {
                    &self.selected_dto_cancelled_before_terminal
                }
                SelectedDtoTerminalV1::PreterminalFailure => &self.selected_dto_preterminal_failure,
            };
            Self::increment(counter).map_err(|error| self.poison(error))?;
            selected.remove(&generation);
        }

        let counter = match terminal {
            BlinkGenerationTerminalV1::Processed
            | BlinkGenerationTerminalV1::Cancelled
            | BlinkGenerationTerminalV1::InternalFailure => &self.processed_terminal,
            BlinkGenerationTerminalV1::ReplacedBeforeFrame => &self.replaced_before_frame,
            BlinkGenerationTerminalV1::CancelledBeforeFrame => &self.cancelled_before_frame,
        };
        Self::increment(counter).map_err(|error| self.poison(error))?;
        generations.remove(&generation);
        Ok(())
    }

    /// Terminalizes and evicts every queued generation except an optional active generation.
    pub fn terminalize_shutdown_pending(
        &self,
        active_generation: Option<u64>,
    ) -> Result<(), EdgeMeasurementError> {
        let mut generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let mut selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        loop {
            let generation = generations.iter().find_map(|(generation, terminal)| {
                (terminal.is_none() && Some(*generation) != active_generation)
                    .then_some(*generation)
            });
            let Some(generation) = generation else {
                break;
            };
            let Some(current) = generations.get_mut(&generation) else {
                return Err(self.poison(EdgeMeasurementError::GenerationConflict));
            };
            if current.replace(BlinkGenerationTerminalV1::CancelledBeforeFrame).is_some() {
                return Err(self.poison(EdgeMeasurementError::GenerationConflict));
            }
            Self::increment(&self.cancelled_before_frame).map_err(|error| self.poison(error))?;
            generations.remove(&generation);
            if let Some(mut current) = selected.remove(&generation) {
                if current.replace(SelectedDtoTerminalV1::PreterminalFailure).is_some() {
                    return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
                }
                Self::increment(&self.selected_dto_preterminal_failure)
                    .map_err(|error| self.poison(error))?;
            }
        }
        Ok(())
    }

    /// Returns exact cumulative counters and live pending cardinalities for conservation checks.
    pub fn snapshot(&self) -> Result<BlinkLedgerSnapshotV1, EdgeMeasurementError> {
        let generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        Ok(BlinkLedgerSnapshotV1 {
            victim_ingress_observed: self.observed.load(Ordering::SeqCst),
            victim_ingress_accepted: self.accepted.load(Ordering::SeqCst),
            slot_accepted: self.slot_accepted.load(Ordering::SeqCst),
            slot_replaced: self.slot_replaced.load(Ordering::SeqCst),
            slot_closed: self.slot_closed.load(Ordering::SeqCst),
            generation_overflow: self.generation_overflow.load(Ordering::SeqCst),
            admitted_generations: self.admitted_generations.load(Ordering::SeqCst),
            processed_terminal: self.processed_terminal.load(Ordering::SeqCst),
            replaced_before_frame: self.replaced_before_frame.load(Ordering::SeqCst),
            cancelled_before_frame: self.cancelled_before_frame.load(Ordering::SeqCst),
            selected_dto_built_preterminal: self
                .selected_dto_built_preterminal
                .load(Ordering::SeqCst),
            selected_dto_committed: self.selected_dto_committed.load(Ordering::SeqCst),
            selected_dto_cancelled_before_terminal: self
                .selected_dto_cancelled_before_terminal
                .load(Ordering::SeqCst),
            selected_dto_preterminal_failure: self
                .selected_dto_preterminal_failure
                .load(Ordering::SeqCst),
            generation_pending: generations.len() as u64,
            selected_pending: selected.len() as u64,
            poisoned: self.poisoned.load(Ordering::SeqCst),
        })
    }

    /// Purely verifies all five exact conservation equations and final closure invariants.
    ///
    /// Every equation's right-hand side is formed with checked addition. Arithmetic overflow,
    /// poison, either pending set, or any unequal conservation term rejects the snapshot.
    pub fn verify_snapshot(snapshot: &BlinkLedgerSnapshotV1) -> Result<(), EdgeMeasurementError> {
        let ingress_rhs = snapshot
            .slot_accepted
            .checked_add(snapshot.slot_replaced)
            .and_then(|value| value.checked_add(snapshot.slot_closed))
            .and_then(|value| value.checked_add(snapshot.generation_overflow))
            .ok_or(EdgeMeasurementError::CounterOverflow)?;
        let generation_rhs = snapshot
            .processed_terminal
            .checked_add(snapshot.replaced_before_frame)
            .and_then(|value| value.checked_add(snapshot.cancelled_before_frame))
            .ok_or(EdgeMeasurementError::CounterOverflow)?;
        let selected_rhs = snapshot
            .selected_dto_committed
            .checked_add(snapshot.selected_dto_cancelled_before_terminal)
            .and_then(|value| value.checked_add(snapshot.selected_dto_preterminal_failure))
            .ok_or(EdgeMeasurementError::CounterOverflow)?;
        let admitted_rhs = snapshot
            .slot_accepted
            .checked_add(snapshot.slot_replaced)
            .ok_or(EdgeMeasurementError::CounterOverflow)?;
        if snapshot.poisoned
            || snapshot.generation_pending != 0
            || snapshot.selected_pending != 0
            || ingress_rhs != snapshot.victim_ingress_observed
            || ingress_rhs != snapshot.victim_ingress_accepted
            || admitted_rhs != snapshot.admitted_generations
            || generation_rhs != snapshot.admitted_generations
            || selected_rhs != snapshot.selected_dto_built_preterminal
        {
            return Err(EdgeMeasurementError::FinalNotClosed);
        }
        Ok(())
    }

    /// Verifies all exact conservation equations and zero final pending sets.
    pub fn verify_final(&self) -> Result<BlinkLedgerSnapshotV1, EdgeMeasurementError> {
        let snapshot = self.snapshot()?;
        Self::verify_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    fn increment(counter: &AtomicU64) -> Result<(), EdgeMeasurementError> {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| value.checked_add(1))
            .map(|_| ())
            .map_err(|_| EdgeMeasurementError::CounterOverflow)
    }

    fn poison(&self, error: EdgeMeasurementError) -> EdgeMeasurementError {
        self.poisoned.store(true, Ordering::SeqCst);
        error
    }
}

/// Exactly-once producer cutoff record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerEpochCutoffV1 {
    /// Producer epoch identifier.
    pub producer_epoch: u64,
    /// Last admitted shared clock observation ordinal.
    pub cutoff_clock_observation_ordinal: u64,
    /// Last admitted wire ordinal.
    pub last_admitted_wire_ordinal: u64,
    /// Last admitted flashblocks source generation.
    pub last_admitted_source_generation: u64,
    /// Last admitted Blink generation.
    pub last_admitted_blink_generation: u64,
    /// Last admitted pending snapshot sequence.
    pub last_pending_snapshot_sequence: u64,
    /// Last admitted coverage sequence.
    pub last_coverage_sequence: u64,
    /// Last admitted candidate sequence.
    pub last_candidate_sequence: u64,
    /// Monotonic timestamp at the latch.
    pub latch_mono_ns: u64,
    /// Standard SHA-256 over the shared canonical authority-record bytes.
    pub record_hash: [u8; 32],
}
impl Serialize for ProducerEpochCutoffV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut record = serializer.serialize_struct("ProducerEpochCutoffV1", 10)?;
        record.serialize_field("producerEpoch", &self.producer_epoch.to_string())?;
        record.serialize_field(
            "cutoffClockObservationOrdinal",
            &self.cutoff_clock_observation_ordinal.to_string(),
        )?;
        record.serialize_field(
            "lastAdmittedWireOrdinal",
            &self.last_admitted_wire_ordinal.to_string(),
        )?;
        record.serialize_field(
            "lastAdmittedSourceGeneration",
            &self.last_admitted_source_generation.to_string(),
        )?;
        record.serialize_field(
            "lastAdmittedBlinkGeneration",
            &self.last_admitted_blink_generation.to_string(),
        )?;
        record.serialize_field(
            "lastPendingSnapshotSequence",
            &self.last_pending_snapshot_sequence.to_string(),
        )?;
        record.serialize_field("lastCoverageSequence", &self.last_coverage_sequence.to_string())?;
        record
            .serialize_field("lastCandidateSequence", &self.last_candidate_sequence.to_string())?;
        record.serialize_field("latchMonoNs", &self.latch_mono_ns.to_string())?;
        record.serialize_field("recordHash", &hex::encode(self.record_hash))?;
        record.end()
    }
}

/// Unhashed fields used to construct an exactly-once producer cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerEpochCutoffFieldsV1 {
    /// Producer epoch identifier.
    pub producer_epoch: u64,
    /// Last admitted shared clock observation ordinal.
    pub cutoff_clock_observation_ordinal: u64,
    /// Last admitted wire ordinal.
    pub last_admitted_wire_ordinal: u64,
    /// Last admitted flashblocks source generation.
    pub last_admitted_source_generation: u64,
    /// Last admitted Blink generation.
    pub last_admitted_blink_generation: u64,
    /// Last admitted pending snapshot sequence.
    pub last_pending_snapshot_sequence: u64,
    /// Last admitted coverage sequence.
    pub last_coverage_sequence: u64,
    /// Last admitted candidate sequence.
    pub last_candidate_sequence: u64,
    /// Monotonic timestamp at the latch.
    pub latch_mono_ns: u64,
}

impl ProducerEpochCutoffV1 {
    /// Constructs a cutoff record and its shared canonical authority-record hash.
    pub fn new(fields: ProducerEpochCutoffFieldsV1) -> Self {
        let canonical_record = format!(
            concat!(
                "{{\"cutoffClockObservationOrdinal\":\"{}\",",
                "\"lastAdmittedBlinkGeneration\":\"{}\",",
                "\"lastAdmittedSourceGeneration\":\"{}\",",
                "\"lastAdmittedWireOrdinal\":\"{}\",",
                "\"lastCandidateSequence\":\"{}\",",
                "\"lastCoverageSequence\":\"{}\",",
                "\"lastPendingSnapshotSequence\":\"{}\",",
                "\"latchMonoNs\":\"{}\",",
                "\"producerEpoch\":\"{}\"}}"
            ),
            fields.cutoff_clock_observation_ordinal,
            fields.last_admitted_blink_generation,
            fields.last_admitted_source_generation,
            fields.last_admitted_wire_ordinal,
            fields.last_candidate_sequence,
            fields.last_coverage_sequence,
            fields.last_pending_snapshot_sequence,
            fields.latch_mono_ns,
            fields.producer_epoch,
        );
        let authority_domain = b"base-edge-authority-record-v1\0";
        let cutoff_domain = b"edge-producer-epoch-cutoff/v1";
        let mut record_bytes = Vec::with_capacity(
            12 + authority_domain.len() + cutoff_domain.len() + canonical_record.len(),
        );
        for field in
            [authority_domain.as_slice(), cutoff_domain.as_slice(), canonical_record.as_bytes()]
        {
            let Ok(length) = u32::try_from(field.len()) else {
                unreachable!("cutoff authority field length is statically bounded");
            };
            record_bytes.extend_from_slice(&length.to_be_bytes());
            record_bytes.extend_from_slice(field);
        }
        Self {
            producer_epoch: fields.producer_epoch,
            cutoff_clock_observation_ordinal: fields.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: fields.last_admitted_wire_ordinal,
            last_admitted_source_generation: fields.last_admitted_source_generation,
            last_admitted_blink_generation: fields.last_admitted_blink_generation,
            last_pending_snapshot_sequence: fields.last_pending_snapshot_sequence,
            last_coverage_sequence: fields.last_coverage_sequence,
            last_candidate_sequence: fields.last_candidate_sequence,
            latch_mono_ns: fields.latch_mono_ns,
            record_hash: DefaultCrypto.sha256(&record_bytes),
        }
    }
}

impl ProducerEpochCutoffV1 {
    /// Returns exact canonical ten-field cutoff bytes without framing or trailing bytes.
    pub fn canonical_inner_bytes(&self) -> Vec<u8> {
        format!(
            concat!(
                "{{\"cutoffClockObservationOrdinal\":\"{}\",",
                "\"lastAdmittedBlinkGeneration\":\"{}\",",
                "\"lastAdmittedSourceGeneration\":\"{}\",",
                "\"lastAdmittedWireOrdinal\":\"{}\",",
                "\"lastCandidateSequence\":\"{}\",",
                "\"lastCoverageSequence\":\"{}\",",
                "\"lastPendingSnapshotSequence\":\"{}\",",
                "\"latchMonoNs\":\"{}\",",
                "\"producerEpoch\":\"{}\",",
                "\"recordHash\":\"{}\"}}"
            ),
            self.cutoff_clock_observation_ordinal,
            self.last_admitted_blink_generation,
            self.last_admitted_source_generation,
            self.last_admitted_wire_ordinal,
            self.last_candidate_sequence,
            self.last_coverage_sequence,
            self.last_pending_snapshot_sequence,
            self.latch_mono_ns,
            self.producer_epoch,
            hex::encode(self.record_hash),
        )
        .into_bytes()
    }

    /// Verifies the byte-preserved inner authority hash.
    pub fn has_valid_record_hash(&self) -> bool {
        self == &Self::new(ProducerEpochCutoffFieldsV1 {
            producer_epoch: self.producer_epoch,
            cutoff_clock_observation_ordinal: self.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: self.last_admitted_wire_ordinal,
            last_admitted_source_generation: self.last_admitted_source_generation,
            last_admitted_blink_generation: self.last_admitted_blink_generation,
            last_pending_snapshot_sequence: self.last_pending_snapshot_sequence,
            last_coverage_sequence: self.last_coverage_sequence,
            last_candidate_sequence: self.last_candidate_sequence,
            latch_mono_ns: self.latch_mono_ns,
        })
    }
}

/// Exact-once cutoff latch.
#[derive(Debug, Default)]
pub struct ProducerEpochCutoffLatchV1 {
    value: Mutex<Option<ProducerEpochCutoffV1>>,
}

impl ProducerEpochCutoffLatchV1 {
    /// Latches a cutoff once, allowing only byte-identical idempotent repeats.
    pub fn latch(&self, cutoff: ProducerEpochCutoffV1) -> Result<(), EdgeMeasurementError> {
        let mut value = self.value.lock().map_err(|_| EdgeMeasurementError::LockPoisoned)?;
        match value.as_ref() {
            None => {
                *value = Some(cutoff);
                Ok(())
            }
            Some(existing) if existing == &cutoff => Ok(()),
            Some(_) => Err(EdgeMeasurementError::CutoffConflict),
        }
    }

    /// Returns the latched cutoff, when present.
    pub fn get(&self) -> Result<Option<ProducerEpochCutoffV1>, EdgeMeasurementError> {
        let value = self.value.lock().map_err(|_| EdgeMeasurementError::LockPoisoned)?;
        Ok(value.clone())
    }
}

/// Bounded, truthful selected-candidate row emitted during the campaign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeCandidateDetailV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Gap-free selected-candidate sequence.
    pub candidate_sequence: u64,
    /// Blink source generation.
    pub source_generation: u64,
    /// Independent Blink runtime generation.
    pub candidate_generation: u64,
    /// Independent pending-registry coverage generation.
    pub coverage_generation: u64,
    /// Pending snapshot sequence joined through the side registry.
    pub pending_snapshot_sequence: u64,
    /// Exact payload-first record sequence.
    pub payload_first_record_sequence: u64,
    /// Exact payload-first record hash.
    pub payload_first_record_hash: [u8; 32],
    /// Connection sequence observed when the detail was captured.
    pub connection_sequence_at_capture: u64,
    /// Connection record hash observed when the detail was captured.
    pub connection_record_hash_at_capture: [u8; 32],
    /// Canonical parent hash for all provider evidence.
    pub parent_hash: B256,
    /// Candidate block.
    pub block_number: u64,
    /// Payload identifier.
    pub payload_id: [u8; 8],
    /// Predecessor flashblock index.
    pub predecessor_index: u64,
    /// Total ordered transaction count including the victim.
    pub ordered_transaction_count: u64,
    /// Exact victim insertion position and exclusive predecessor prefix cutoff.
    pub ordered_transaction_cutoff_position: u64,
    /// Canonical digest of every transaction before the cutoff.
    pub ordered_transaction_digest: B256,
    /// Exact victim-absence proof at the predecessor cutoff.
    pub victim_absent_before_position: bool,
    /// Victim hash.
    pub victim_hash: B256,
    /// Exact bounded victim envelope.
    pub victim_raw: Bytes,
    /// Selected plan digest.
    pub selected_plan_digest: B256,
    /// Exact selected plan retained for digest recomputation.
    pub selected_plan: BackrunPlan,
    /// Exact selected prepared route retained for witness recomputation.
    pub prepared_route: [PreparedPoolState; 2],
    /// Exact canonical materialized writes retained for witness recomputation.
    pub materialized_state: MaterializedState,
    /// Structural terminal hash.
    pub structural_terminal_hash: [u8; 32],
    /// Canonical SHA-256 of the replayable registry terminal record.
    pub registry_terminal_record_hash: [u8; 32],
    /// State-root witness.
    pub state_root: B256,
    /// Prepared-state witness digest.
    pub prepared_state_digest: B256,
    /// Code witness digest.
    pub code_witness_digest: B256,
    /// G0 executor identity digest.
    pub g0_code_identity_digest: B256,
    /// Slot witness digest.
    pub slot_witness_digest: B256,
    /// Economics evidence digest.
    pub economics_evidence_digest: B256,
    /// Exact two-hop execution witnesses.
    pub execution_hops: [MeasurementExecutionHopV1; 2],
    /// Exact same-parent runtime identity witnesses.
    pub deployment_identities: [(Address, B256); 4],
    /// Canonical preregistration digest.
    pub prereg_digest: B256,
    /// Owner policy digest.
    pub policy_digest: B256,
    /// Owner configuration digest.
    pub config_digest: B256,
    /// Producer manifest digest.
    pub producer_digest: B256,
    /// Owner approval receipt digest.
    pub owner_approval_receipt_digest: B256,
    /// Deterministic non-envelope measurement binding.
    pub backrun_measurement_binding: crate::BackrunMeasurementBindingV1,
}

impl EdgeCandidateDetailV1 {
    fn new(
        config: &EdgeMeasurementOwnerConfigV1,
        candidate_sequence: u64,
        draft: EdgeCandidateEvidenceV3,
    ) -> Self {
        Self {
            producer_epoch: config.producer_epoch,
            candidate_sequence,
            source_generation: draft.source_generation,
            candidate_generation: draft.generation,
            coverage_generation: draft.coverage_generation,
            pending_snapshot_sequence: draft.pending_snapshot_sequence,
            payload_first_record_sequence: draft.payload_first_record_sequence,
            payload_first_record_hash: draft.payload_first_record_hash,
            connection_sequence_at_capture: draft.connection_sequence_at_capture,
            connection_record_hash_at_capture: draft.connection_record_hash_at_capture,
            parent_hash: draft.parent_hash,
            block_number: draft.selected_plan.block_number,
            payload_id: draft.selected_plan.payload_id.0.0,
            predecessor_index: draft.selected_plan.predecessor_index,
            ordered_transaction_count: draft.ordered_transaction_count,
            ordered_transaction_cutoff_position: draft.ordered_transaction_cutoff_position,
            ordered_transaction_digest: draft.ordered_transaction_digest,
            victim_absent_before_position: draft.victim_absent_before_position,
            victim_hash: draft.selected_plan.victim,
            victim_raw: draft.victim_raw,
            selected_plan_digest: draft.selected_plan.digest.0,
            selected_plan: draft.selected_plan,
            prepared_route: draft.prepared_route,
            materialized_state: draft.materialized_state,
            structural_terminal_hash: draft.structural_terminal_hash,
            registry_terminal_record_hash: draft.registry_terminal_record_hash,
            state_root: draft.state_root,
            prepared_state_digest: draft.prepared_state_digest,
            code_witness_digest: draft.code_witness_digest,
            g0_code_identity_digest: config.g0_code_identity_digest,
            slot_witness_digest: draft.slot_witness_digest,
            economics_evidence_digest: draft.economics_evidence_digest,
            execution_hops: draft.execution_hops,
            deployment_identities: draft.deployment_identities,
            prereg_digest: config.prereg_digest,
            policy_digest: config.policy_digest,
            config_digest: config.config_digest,
            producer_digest: config.producer_digest,
            owner_approval_receipt_digest: config.owner_approval_receipt_digest,
            backrun_measurement_binding: draft.backrun_measurement_binding,
        }
    }

    /// Validates all campaign-time evidence without inventing later receipts.
    pub fn validate_detail(&self) -> Result<(), EdgeMeasurementError> {
        MeasurementEncoder::validate(&self.selected_plan)
            .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        self.prepared_route
            .iter()
            .try_for_each(PreparedPoolState::validate)
            .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        let prepared_state_digest =
            EdgeMeasurementOwnerV1::prepared_route_digest(&self.prepared_route)
                .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        let slot_witness_digest =
            EdgeMeasurementOwnerV1::materialized_state_digest(&self.materialized_state)
                .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        let economics_evidence_digest =
            EdgeMeasurementOwnerV1::economics_digest(&self.selected_plan, &self.execution_hops)
                .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        if self.victim_raw.len() > EDGE_MAX_VICTIM_RAW_BYTES
            || self.parent_hash != self.selected_plan.parent_hash
            || self.block_number != self.selected_plan.block_number
            || self.predecessor_index != self.selected_plan.predecessor_index
            || self.victim_hash != self.selected_plan.victim
            || self.selected_plan_digest != self.selected_plan.digest.0
            || !self.victim_absent_before_position
            || self.ordered_transaction_cutoff_position >= self.ordered_transaction_count
            || self.ordered_transaction_cutoff_position.checked_add(1)
                != Some(self.ordered_transaction_count)
            || self.ordered_transaction_digest.is_zero()
            || keccak256(&self.victim_raw) != self.victim_hash
            || self.backrun_measurement_binding.target_tx_hash != self.victim_hash
            || self.backrun_measurement_binding.plan != self.selected_plan
            || self.backrun_measurement_binding.execution_hops != self.execution_hops
            || self.connection_record_hash_at_capture == [0; 32]
            || self.registry_terminal_record_hash == [0; 32]
            || self.prepared_state_digest != prepared_state_digest
            || self.code_witness_digest
                != EdgeMeasurementOwnerV1::deployment_identity_digest(&self.deployment_identities)
            || self.g0_code_identity_digest != self.code_witness_digest
            || self.slot_witness_digest != slot_witness_digest
            || self.economics_evidence_digest != economics_evidence_digest
        {
            return Err(EdgeMeasurementError::GenerationConflict);
        }
        MeasurementBindingDeriverV1::validate(&self.backrun_measurement_binding, &self.victim_raw)
            .map_err(|_| EdgeMeasurementError::GenerationConflict)?;
        Ok(())
    }
}

/// SHA-256 helper shared with the sole canonical CLI durability coordinator.
#[derive(Debug, Default, Clone, Copy)]
pub struct EdgeMeasurementDurabilityV1;

impl EdgeMeasurementDurabilityV1 {
    /// Ordinary SHA-256 used by the canonical coordinator.
    pub fn sha256(bytes: &[u8]) -> [u8; 32] {
        DefaultCrypto.sha256(bytes)
    }
}

#[cfg(any(test, feature = "test-utils"))]
mod candidate_fixture {
    use std::{
        collections::BTreeMap,
        str::FromStr,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use alloy_consensus::{Header, Sealed, Transaction, transaction::SignerRecoverable};
    use alloy_eips::{Decodable2718, Typed2718};
    use alloy_primitives::{Address, B256, Bytes, U256, keccak256, map::HashMap};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use base_execution_chainspec::BaseChainSpec;
    use reth_provider::StateProviderBox;
    use reth_revm::state::AccountInfo;
    use reth_revm::test_utils::StateProviderTest;

    use super::{
        BlinkGenerationTerminalV1, EdgeCandidateStageInputV3, EdgeMeasurementOwnerV1,
        EdgeProducerError, ExactProtocol, PreparedPoolQuote, PreparedPoolState, ShadowOutcome,
        SlotSubmit,
    };
    use crate::{
        AuditedWriteKey, BackrunHop, BackrunPlan, BackrunPlanDigest, BundleVisitor,
        CancellationProbe, CancellationToken, EdgeSnapshotEvidenceV1, FrameAuditPlan,
        FrameProcessor, GlobalLifecycle, MeasurementEncoder, PayloadVisitor, PendingSnapshotView,
        PortError, SnapshotCaptureCoordinator, SnapshotHandle, SnapshotHandleFactory,
        TraderSnapshotPort, TransactionVisitor, VisitControl, VisitSummary, WETH,
    };

    pub(super) const RAW_VICTIM: &str = "02f86c8221058034839a4ae283021528942f16386bb37709016023232523ff6d9daf444be380841249c58bc080a001b927eda2af9b00b52a57be0885e0303c39dd2831732e14051c2336470fd468a0681bf120baf562915841a48601c2b54a6742511e535cf8f71c95115af7ff63bd";
    pub(super) const BLOCK_NUMBER: u64 = 100;
    pub(super) const PREDECESSOR_INDEX: u64 = 1;
    pub(super) fn payload_id() -> PayloadId {
        PayloadId::new([2; 8])
    }

    #[derive(Debug)]
    struct FixtureView {
        parent_hash: B256,
        latest_header: Mutex<Sealed<Header>>,
    }

    impl PendingSnapshotView for FixtureView {
        fn parent_hash(&self) -> B256 {
            self.parent_hash
        }

        fn latest_block_number(&self) -> u64 {
            BLOCK_NUMBER
        }

        fn canonical_block_number(&self) -> u64 {
            BLOCK_NUMBER - 1
        }

        fn latest_flashblock_index(&self) -> u64 {
            PREDECESSOR_INDEX
        }

        fn latest_header(&self) -> Sealed<Header> {
            self.latest_header.lock().expect("fixture header lock").clone()
        }

        fn pending_account_nonce(
            &self,
            _address: Address,
        ) -> Result<Option<crate::PendingAccountNonce>, PortError> {
            Ok(None)
        }

        fn latest_block_transaction_count(&self) -> usize {
            0
        }

        fn has_transaction_hash(&self, _transaction_hash: B256) -> bool {
            false
        }

        fn transaction_position(
            &self,
            _block_number: u64,
            _transaction_hash: B256,
        ) -> Option<usize> {
            None
        }

        fn visit_latest_block_payloads(
            &self,
            visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            let control = visitor.visit(payload_id(), PREDECESSOR_INDEX)?;
            Ok(VisitSummary { visited: 1, complete: control == VisitControl::Continue })
        }

        fn visit_transactions_for_block(
            &self,
            block_number: u64,
            start: usize,
            limit: usize,
            _visitor: &mut dyn TransactionVisitor,
        ) -> Result<VisitSummary, PortError> {
            if block_number != BLOCK_NUMBER || start != 0 || limit != 0 {
                return Err(PortError::Incoherent);
            }
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_bundle(
            &self,
            _visitor: &mut dyn BundleVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }
    }

    #[derive(Debug)]
    struct FixturePort {
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
        parent_header: Sealed<Header>,
        provider: StateProviderTest,
        evidence: EdgeSnapshotEvidenceV1,
    }

    impl TraderSnapshotPort for FixturePort {
        fn capture_latest(
            &self,
            factory: &SnapshotHandleFactory,
        ) -> Result<Option<SnapshotHandle>, PortError> {
            factory
                .issue_with_edge_evidence(Arc::clone(&self.view), self.received_at, self.evidence)
                .map(Some)
        }

        fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
            handle.matches_capture(&self.view, self.received_at)
        }

        fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError> {
            if block_hash != self.parent_header.hash() {
                return Err(PortError::Incoherent);
            }
            Ok(Box::new(self.provider.clone()))
        }

        fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError> {
            if block_hash != self.parent_header.hash() {
                return Err(PortError::Incoherent);
            }
            Ok(self.parent_header.clone())
        }
    }

    pub(super) fn stage(
        owner: &EdgeMeasurementOwnerV1,
        evidence: EdgeSnapshotEvidenceV1,
        admit_generation: bool,
        terminalize: bool,
    ) -> Result<(), EdgeProducerError> {
        let victim_raw = Bytes::from_str(&format!("0x{RAW_VICTIM}"))
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let transaction = BaseTxEnvelope::decode_2718_exact(victim_raw.as_ref())
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let victim_hash = keccak256(&victim_raw);
        let victim_sender = transaction
            .recover_signer()
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;

        let parent_header = Sealed::new(Header {
            number: BLOCK_NUMBER - 1,
            gas_limit: 30_000_000,
            state_root: B256::repeat_byte(0x44),
            ..Default::default()
        });
        let parent_hash = parent_header.hash();
        let latest_header = Sealed::new(Header {
            parent_hash,
            number: BLOCK_NUMBER,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(0),
            ..Default::default()
        });
        let view = Arc::new(FixtureView { parent_hash, latest_header: Mutex::new(latest_header) });
        let captured_view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::<FixtureView>::clone(&view);

        let mut provider = StateProviderTest::default();
        let measurement_account = Default::default();
        provider.insert_account(
            owner.config.measurement_sender,
            measurement_account,
            None,
            HashMap::default(),
        );
        let victim_account =
            AccountInfo { balance: U256::MAX, nonce: transaction.nonce(), ..Default::default() };
        provider.insert_account(victim_sender, victim_account.into(), None, HashMap::default());
        for (address, runtime_hash) in [
            (crate::MEASUREMENT_EXECUTOR, owner.config.executor_runtime_hash),
            (owner.config.v2_adapter, owner.config.v2_adapter_runtime_hash),
            (owner.config.v3_adapter, owner.config.v3_adapter_runtime_hash),
            (owner.config.aerodrome_adapter, owner.config.aerodrome_adapter_runtime_hash),
        ] {
            assert_eq!(runtime_hash, keccak256([]));
            provider.insert_account(
                address,
                Default::default(),
                Some(Bytes::new()),
                HashMap::default(),
            );
        }
        let received_at = Instant::now();
        let port =
            FixturePort { view: captured_view, received_at, parent_header, provider, evidence };
        let snapshot = SnapshotCaptureCoordinator
            .capture(&port)
            .map_err(|_| EdgeProducerError::MissingCandidateEvidence)?
            .ok_or(EdgeProducerError::MissingCandidateEvidence)?;

        let probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(30))),
            Arc::new(GlobalLifecycle::default()),
        );
        let audit = FrameAuditPlan::new(
            vec![
                AuditedWriteKey::AccountBalance {
                    address: victim_sender,
                    evidence_digest: B256::repeat_byte(0x45),
                },
                AuditedWriteKey::AccountNonce {
                    address: victim_sender,
                    evidence_digest: B256::repeat_byte(0x46),
                },
                AuditedWriteKey::AccountBalance {
                    address: Address::ZERO,
                    evidence_digest: B256::repeat_byte(0x47),
                },
            ],
            BTreeMap::new(),
        )
        .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let frame = crate::VictimFrame {
            chain_id: transaction.chain_id().ok_or(EdgeProducerError::CandidateEvidenceMismatch)?,
            transaction_type: transaction.ty(),
            transaction_hash: victim_hash,
            from: victim_sender,
            raw_tx: victim_raw.clone(),
            parent_hash,
            block_number: BLOCK_NUMBER,
            victim_flashblock_index: PREDECESSOR_INDEX + 1,
            received_at: Instant::now(),
        };
        let processed = FrameProcessor::process(
            &port,
            &snapshot,
            &frame,
            Instant::now(),
            Arc::new(BaseChainSpec::mainnet()),
            &audit,
            &probe,
        )
        .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?
        .ok_or(EdgeProducerError::CandidateEvidenceMismatch)?;

        *view.latest_header.lock().map_err(|_| EdgeProducerError::Ledger)? = Sealed::new(Header {
            parent_hash,
            number: BLOCK_NUMBER,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            ..Default::default()
        });

        let token = Address::repeat_byte(0x51);
        let prepared = [
            PreparedPoolState {
                pool: Address::repeat_byte(0x52),
                protocol: ExactProtocol::UniswapV2,
                token0: WETH,
                token1: token,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(2_000_000),
                },
            },
            PreparedPoolState {
                pool: Address::repeat_byte(0x53),
                protocol: ExactProtocol::UniswapV2,
                token0: token,
                token1: WETH,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(1_000_000),
                },
            },
        ];
        let amount_in = U256::from(1_000);
        let first_out = prepared[0]
            .quote_exact_in(WETH, amount_in, &probe)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let amount_out = prepared[1]
            .quote_exact_in(token, first_out, &probe)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;
        let gross_profit = amount_out
            .checked_sub(amount_in)
            .ok_or(EdgeProducerError::CandidateEvidenceMismatch)?;
        let mut plan = BackrunPlan {
            parent_hash,
            block_number: BLOCK_NUMBER,
            predecessor_index: PREDECESSOR_INDEX,
            payload_id: payload_id(),
            victim: victim_hash,
            route: [
                BackrunHop {
                    pool: prepared[0].pool,
                    protocol: prepared[0].protocol,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: prepared[0].fee_pips,
                },
                BackrunHop {
                    pool: prepared[1].pool,
                    protocol: prepared[1].protocol,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: prepared[1].fee_pips,
                },
            ],
            amount_in,
            amount_out,
            gross_profit,
            digest: BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan)
            .map_err(|_| EdgeProducerError::CandidateEvidenceMismatch)?;

        let generation =
            owner.ledger().snapshot().map_err(|_| EdgeProducerError::Ledger)?.admitted_generations;
        if admit_generation {
            owner.with_blink_admission(|owner, authoritative| {
                if authoritative {
                    owner.observe_ledger_result_admitted(owner.ledger().record_observed());
                    owner.record_submission_admitted(generation, SlotSubmit::Accepted);
                }
            });
        }
        owner.stage_selected_candidate(EdgeCandidateStageInputV3 {
            generation,
            port: &port,
            snapshot: &snapshot,
            processed: &processed,
            prepared: &prepared,
            plan: &plan,
            victim_raw: &victim_raw,
            probe: &probe,
        })?;
        if terminalize {
            owner.record_terminal_and_resolve(
                generation,
                BlinkGenerationTerminalV1::Processed,
                Some(ShadowOutcome::Selected),
            );
        }
        Ok(())
    }
}
#[cfg(feature = "test-utils")]
/// Independent literal oracle for the deterministic selected-candidate test fixture.
#[derive(Debug, Default, Clone, Copy)]
pub struct EdgeCandidateDetailOracleV1;

#[cfg(feature = "test-utils")]
impl EdgeCandidateDetailOracleV1 {
    /// Constructs the frozen candidate fixture without invoking producer staging or owner queues.
    pub fn frozen(
        producer_epoch: u64,
        evidence: crate::EdgeSnapshotEvidenceV1,
    ) -> EdgeCandidateDetailV1 {
        const RAW_VICTIM: &str = "02f86c8221058034839a4ae283021528942f16386bb37709016023232523ff6d9daf444be380841249c58bc080a001b927eda2af9b00b52a57be0885e0303c39dd2831732e14051c2336470fd468a0681bf120baf562915841a48601c2b54a6742511e535cf8f71c95115af7ff63bd";
        let hash = |value: &str| B256::from_str(value).expect("frozen candidate hash");
        let address = |value: &str| Address::from_str(value).expect("frozen candidate address");
        let parent_hash =
            hash("0x0bfe528775a427e6cce7190e73c1c66249b7d1106783ff645717544168109674");
        let victim_hash =
            hash("0xf9420cbaf66a2dda75a015488d37262cbfd4abd0aad7bb2be8a63e14b1fa7a94");
        let plan_digest =
            hash("0x3c8361180a40e4c56bca76ae3a3d36df160e903c4d6d15ed8d68e48415433fa1");
        let token = Address::repeat_byte(0x51);
        let prepared_route = [
            PreparedPoolState {
                pool: Address::repeat_byte(0x52),
                protocol: ExactProtocol::UniswapV2,
                token0: crate::WETH,
                token1: token,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(2_000_000),
                },
            },
            PreparedPoolState {
                pool: Address::repeat_byte(0x53),
                protocol: ExactProtocol::UniswapV2,
                token0: token,
                token1: crate::WETH,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(1_000_000),
                },
            },
        ];
        let selected_plan = BackrunPlan {
            parent_hash,
            block_number: 100,
            predecessor_index: 1,
            payload_id: PayloadId::new([2; 8]),
            victim: victim_hash,
            route: [
                BackrunHop {
                    pool: Address::repeat_byte(0x52),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: crate::WETH,
                    token_out: token,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: Address::repeat_byte(0x53),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: token,
                    token_out: crate::WETH,
                    fee_pips: 3_000,
                },
            ],
            amount_in: U256::from(1_000),
            amount_out: U256::from(1_982),
            gross_profit: U256::from(982),
            digest: BackrunPlanDigest(plan_digest),
        };
        let execution_hops = [
            MeasurementExecutionHopV1 {
                adapter: Address::repeat_byte(2),
                min_amount_out: U256::from(1_992),
                funding_target: Address::repeat_byte(0x52),
            },
            MeasurementExecutionHopV1 {
                adapter: Address::repeat_byte(2),
                min_amount_out: U256::from(1_982),
                funding_target: Address::repeat_byte(0x53),
            },
        ];
        let empty_runtime_hash =
            hash("0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
        let deployment_identities = [
            (crate::MEASUREMENT_EXECUTOR, empty_runtime_hash),
            (Address::repeat_byte(2), empty_runtime_hash),
            (Address::repeat_byte(3), empty_runtime_hash),
            (Address::repeat_byte(4), empty_runtime_hash),
        ];
        let materialized_state = MaterializedState {
            writes: vec![
                MaterializedWrite {
                    key: AuditedWriteKey::AccountBalance {
                        address: Address::ZERO,
                        evidence_digest: B256::repeat_byte(0x47),
                    },
                    value: U256::from(1_095_328),
                },
                MaterializedWrite {
                    key: AuditedWriteKey::AccountBalance {
                        address: address("0xd5ea3c0ccc172d4022af3fd39a7ef8a0a2f3a54a"),
                        evidence_digest: B256::repeat_byte(0x45),
                    },
                    value: U256::from_str(
                        "115792089237316195423570985008687907853269984665640564039457584007913128544607",
                    )
                    .expect("frozen victim balance"),
                },
                MaterializedWrite {
                    key: AuditedWriteKey::AccountNonce {
                        address: address("0xd5ea3c0ccc172d4022af3fd39a7ef8a0a2f3a54a"),
                        evidence_digest: B256::repeat_byte(0x46),
                    },
                    value: U256::from(1),
                },
            ],
        };
        let binding = crate::BackrunMeasurementBindingV1 {
            nonce_witness: SnapshotNonceWitnessV1 {
                parent_block_hash: parent_hash,
                parent_block_number: 99,
                committed: 0,
                pending_original: None,
                pending_current: None,
            },
            selected_nonce: 0,
            snapshot_base_fee_per_gas: 1,
            victim_max_priority_fee_per_gas: 52,
            victim_max_fee_per_gas: 10_111_714,
            valid_until_block: 100,
            target_tx_hash: victim_hash,
            plan: selected_plan.clone(),
            execution_hops,
            binding_digest: hash(
                "0xae28a70c8d20da583fd18740264e7810a1c4a30402f6ad2bbf6b68f7d4839ba5",
            ),
        };
        EdgeCandidateDetailV1 {
            producer_epoch,
            candidate_sequence: 0,
            source_generation: evidence.source_generation,
            candidate_generation: 0,
            coverage_generation: evidence.coverage_sequence,
            pending_snapshot_sequence: evidence.pending_snapshot_sequence,
            payload_first_record_sequence: evidence.payload_first_record_sequence,
            payload_first_record_hash: evidence.payload_first_record_hash.0,
            connection_sequence_at_capture: evidence.connection_sequence,
            connection_record_hash_at_capture: evidence.connection_record_hash.0,
            parent_hash,
            block_number: 100,
            payload_id: [2; 8],
            predecessor_index: 1,
            ordered_transaction_count: 1,
            ordered_transaction_cutoff_position: 0,
            ordered_transaction_digest: hash(
                "0x4d368b57941b28576df0e1b763bbd1009b695d621dfaf77ccb53ce245221acf6",
            ),
            victim_absent_before_position: true,
            victim_hash,
            victim_raw: Bytes::from_str(&format!("0x{RAW_VICTIM}"))
                .expect("frozen victim envelope"),
            selected_plan_digest: plan_digest,
            selected_plan,
            prepared_route,
            materialized_state,
            structural_terminal_hash: evidence.structural_terminal_hash.0,
            registry_terminal_record_hash: evidence.registry_terminal_record_hash.0,
            state_root: B256::repeat_byte(0x44),
            prepared_state_digest: hash(
                "0x089f56de59cf153e82b21ca8f0c857c4677ed199c93aad7062534c82c0d5894e",
            ),
            code_witness_digest: hash(
                "0x751ca0280889070d84e6eaef855b8816276c4ef6e53b6c7ff6bfedf8277491a7",
            ),
            g0_code_identity_digest: hash(
                "0x751ca0280889070d84e6eaef855b8816276c4ef6e53b6c7ff6bfedf8277491a7",
            ),
            slot_witness_digest: hash(
                "0x2e72d480bd703496773d077fafcfc4671fe06ea919979a6b7f5f4f1c12ab120c",
            ),
            economics_evidence_digest: hash(
                "0x72e3b8338c43dafd5f94c119816872bb76f2ac80dce7aa46d29621875fb05beb",
            ),
            execution_hops,
            deployment_identities,
            prereg_digest: B256::repeat_byte(3),
            policy_digest: B256::repeat_byte(4),
            config_digest: B256::repeat_byte(5),
            producer_digest: B256::repeat_byte(1),
            owner_approval_receipt_digest: B256::repeat_byte(6),
            backrun_measurement_binding: binding,
        }
    }
}

#[cfg(feature = "test-utils")]
impl EdgeMeasurementOwnerV1 {
    /// Stages and terminalizes one deterministic candidate through the production producer path.
    pub fn stage_deterministic_test_candidate(
        &self,
        evidence: crate::EdgeSnapshotEvidenceV1,
    ) -> Result<(), EdgeProducerError> {
        candidate_fixture::stage(self, evidence, true, true)
    }

    /// Stages one deterministic candidate without terminalizing its generation.
    ///
    /// This test-only surface exercises the production staging path without exposing an authority
    /// capability in production builds.
    pub fn stage_deterministic_test_candidate_without_terminal(
        &self,
        evidence: crate::EdgeSnapshotEvidenceV1,
    ) -> Result<(), EdgeProducerError> {
        candidate_fixture::stage(self, evidence, true, false)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "test-utils")]
    use std::str::FromStr;
    use std::{sync::mpsc, thread, time::Duration};

    #[cfg(feature = "test-utils")]
    use alloy_consensus::{Header, Sealed, transaction::SignerRecoverable};
    #[cfg(feature = "test-utils")]
    use alloy_eips::Decodable2718;
    use alloy_primitives::I256;
    use alloy_rpc_types_engine::PayloadId;

    use super::*;
    use crate::{BackrunHop, BackrunPlanDigest, MaterializedWrite, WETH};
    #[test]
    fn reject_schema_preserves_exact_http_and_transport_semantics() {
        let switching = BlinkRejectClassifierV3::classify_http_status(101);
        assert_eq!(switching.reason, BlinkRejectReasonV3::Http101);
        assert_eq!(switching.status, A1Status::AwaitingAck);
        assert_eq!(switching.outcome, None);
        assert!(!switching.retry);

        for (status, reason) in [
            (408, BlinkRejectReasonV3::Http408),
            (429, BlinkRejectReasonV3::Http429),
            (500, BlinkRejectReasonV3::Http5xx),
            (599, BlinkRejectReasonV3::Http5xx),
        ] {
            let retry = BlinkRejectClassifierV3::classify_http_status(status);
            assert_eq!(retry.reason, reason);
            assert_eq!(retry.status, A1Status::Retrying);
            assert_eq!(retry.outcome, Some(A1Outcome::TransportFailure));
            assert!(retry.retry);
        }
        let other = BlinkRejectClassifierV3::classify_http_status(401);
        assert_eq!(other.reason, BlinkRejectReasonV3::HttpOther);
        assert_eq!(other.status, A1Status::DisabledPermanent);
        assert_eq!(other.outcome, Some(A1Outcome::ProtocolDisabled));

        let already_closed = BlinkRejectClassifierV3::classify(&WebSocketError::AlreadyClosed);
        assert_eq!(already_closed.reason, BlinkRejectReasonV3::AlreadyClosed);
        assert!(already_closed.internal);
        assert!(already_closed.cancel_root);
        assert!(!already_closed.retry);
        assert!(BlinkRejectClassifierV3::retryable_io(io::ErrorKind::NetworkUnreachable));
        assert!(!BlinkRejectClassifierV3::retryable_io(io::ErrorKind::InvalidData));
    }

    #[test]
    fn reject_schema_inventory_binds_actual_branch_ids_and_multiplicity() {
        let inventory_branches = BLINK_REJECT_BRANCH_INVENTORY_V3
            .iter()
            .map(|(branch, _)| *branch)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(inventory_branches.len(), BLINK_REJECT_BRANCH_INVENTORY_V3.len());

        let branches_for = |reason| {
            BLINK_REJECT_BRANCH_INVENTORY_V3
                .iter()
                .filter(|(_, inventory_reason)| *inventory_reason == reason)
                .map(|(branch, _)| *branch)
                .collect::<Vec<_>>()
        };
        let expected = [
            (BlinkRejectReasonV3::OperationTimeout, vec!["connect-timeout", "ack-timeout"]),
            (
                BlinkRejectReasonV3::SlotClosed,
                vec!["runtime-lifecycle-closed", "runtime-submit-closed"],
            ),
            (
                BlinkRejectReasonV3::ParamsInvalid,
                vec!["decode-params-invalid", "decode-result-invalid"],
            ),
            (
                BlinkRejectReasonV3::TxHashInvalid,
                vec!["decode-transaction-hash-invalid", "decode-transaction-hash-malformed"],
            ),
            (
                BlinkRejectReasonV3::SenderInvalid,
                vec!["decode-sender-invalid", "decode-sender-malformed"],
            ),
            (
                BlinkRejectReasonV3::RawMissingPrefix,
                vec!["decode-raw-missing-prefix", "decode-raw-prefix-invalid"],
            ),
        ];
        let actual_emit_sources =
            [include_str!("blink_ingress.rs"), include_str!("runtime.rs")].concat();
        for (reason, branches) in expected {
            assert_eq!(branches_for(reason), branches);
            for branch in branches {
                assert!(
                    actual_emit_sources.contains(branch),
                    "branch {branch} is absent from the actual emit source",
                );
            }
        }
    }

    #[test]
    fn reject_schema_digest_stops_source_or_inventory_mutation() {
        let inventory_hash = BlinkRejectClassifierV3::branch_inventory_sha256();
        let source_hash = BlinkRejectClassifierV3::source_slice_sha256();
        let runtime_hash = BlinkRejectClassifierV3::runtime_source_sha256();
        let ledger_hash = BlinkRejectClassifierV3::ledger_source_sha256();
        let digest =
            |inventory: [u8; 32], source: [u8; 32], runtime: [u8; 32], ledger: [u8; 32]| {
                let mut preimage = Vec::with_capacity(29 + 128);
                preimage.extend_from_slice(b"edge-blink-reject-schema/v3\0");
                preimage.extend_from_slice(&inventory);
                preimage.extend_from_slice(&source);
                preimage.extend_from_slice(&runtime);
                preimage.extend_from_slice(&ledger);
                B256::from(DefaultCrypto.sha256(&preimage))
            };
        assert_eq!(
            digest(inventory_hash, source_hash, runtime_hash, ledger_hash),
            BlinkRejectClassifierV3::reject_schema_digest()
        );

        let mut inventory_bytes = String::new();
        for (branch, reason) in BLINK_REJECT_BRANCH_INVENTORY_V3 {
            inventory_bytes.push_str(branch);
            inventory_bytes.push('=');
            inventory_bytes.push_str(reason.wire_name());
            inventory_bytes.push('\n');
        }
        let mut mutated_inventory_bytes = inventory_bytes.into_bytes();
        mutated_inventory_bytes[0] ^= 1;
        let mutated_inventory_hash = DefaultCrypto.sha256(&mutated_inventory_bytes);
        assert_ne!(
            digest(mutated_inventory_hash, source_hash, runtime_hash, ledger_hash),
            BlinkRejectClassifierV3::reject_schema_digest()
        );

        let mut mutated_source_bytes = include_bytes!("blink_ingress.rs").to_vec();
        mutated_source_bytes[0] ^= 1;
        let mutated_source_hash = DefaultCrypto.sha256(&mutated_source_bytes);
        assert_ne!(
            digest(inventory_hash, mutated_source_hash, runtime_hash, ledger_hash),
            BlinkRejectClassifierV3::reject_schema_digest()
        );

        let mut config = owner_config();
        config.reject_schema_digest =
            digest(inventory_hash, mutated_source_hash, runtime_hash, ledger_hash);
        assert_eq!(config.validate(), Err(EdgeProducerError::InvalidConfig));
        config.reject_schema_digest = BlinkRejectClassifierV3::reject_schema_digest();
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn generation_zero_is_first_valid_latest_wins_generation() {
        let ledger = BlinkMeasurementLedgerV1::default();
        ledger.record_observed().unwrap();
        ledger.record_submission(0, SlotSubmit::Accepted).unwrap();
        ledger.record_observed().unwrap();
        ledger.record_submission(1, SlotSubmit::Replaced).unwrap();
        ledger.record_selected_preterminal(1).unwrap();
        ledger
            .record_terminal(1, BlinkGenerationTerminalV1::Processed, Some(ShadowOutcome::Selected))
            .unwrap();
        let final_snapshot = ledger.verify_final().unwrap();
        assert_eq!(final_snapshot.admitted_generations, 2);
        assert_eq!(final_snapshot.replaced_before_frame, 1);
        assert_eq!(final_snapshot.selected_dto_committed, 1);
    }

    #[test]
    fn terminal_eviction_keeps_sequential_72_hour_ledger_counts_exact() {
        const SEQUENTIAL_GENERATIONS: u64 = 10_000;

        let ledger = BlinkMeasurementLedgerV1::default();
        let mut max_live_entries = 0;
        for generation in 0..SEQUENTIAL_GENERATIONS {
            ledger.record_observed().unwrap();
            ledger.record_submission(generation, SlotSubmit::Accepted).unwrap();
            ledger.record_selected_preterminal(generation).unwrap();
            max_live_entries = max_live_entries.max(
                ledger.generations.lock().unwrap().len() + ledger.selected.lock().unwrap().len(),
            );
            let shadow = if generation % 2 == 0 {
                ShadowOutcome::Selected
            } else {
                ShadowOutcome::Cancelled
            };
            ledger
                .record_terminal(generation, BlinkGenerationTerminalV1::Processed, Some(shadow))
                .unwrap();
            assert!(ledger.generations.lock().unwrap().is_empty());
            assert!(ledger.selected.lock().unwrap().is_empty());
        }

        let snapshot = ledger.verify_final().unwrap();
        assert_eq!(snapshot.victim_ingress_observed, SEQUENTIAL_GENERATIONS);
        assert_eq!(snapshot.victim_ingress_accepted, SEQUENTIAL_GENERATIONS);
        assert_eq!(snapshot.admitted_generations, SEQUENTIAL_GENERATIONS);
        assert_eq!(snapshot.processed_terminal, SEQUENTIAL_GENERATIONS);
        assert_eq!(snapshot.selected_dto_built_preterminal, SEQUENTIAL_GENERATIONS);
        assert_eq!(snapshot.selected_dto_committed, SEQUENTIAL_GENERATIONS / 2);
        assert_eq!(snapshot.selected_dto_cancelled_before_terminal, SEQUENTIAL_GENERATIONS / 2);
        assert_eq!(max_live_entries, 2);
        assert!(max_live_entries <= BLINK_LEDGER_CAPACITY * 2);
    }

    #[test]
    fn concurrent_live_generation_capacity_overflow_is_counted_without_poison() {
        const THREADS: u64 = 8;
        const ATTEMPTS: u64 = BLINK_LEDGER_CAPACITY as u64 + 1;

        let ledger = Arc::new(BlinkMeasurementLedgerV1::default());
        let mut handles = Vec::new();
        for worker in 0..THREADS {
            let ledger = Arc::clone(&ledger);
            handles.push(thread::spawn(move || {
                let mut admitted = 0;
                let mut overflowed = 0;
                for generation in (worker..ATTEMPTS).step_by(THREADS as usize) {
                    ledger.record_observed().unwrap();
                    match ledger.record_submission(generation, SlotSubmit::Accepted) {
                        Ok(()) => admitted += 1,
                        Err(EdgeMeasurementError::LedgerCapacityOverflow) => overflowed += 1,
                        Err(error) => panic!("unexpected ledger result: {error}"),
                    }
                }
                (admitted, overflowed)
            }));
        }

        let (admitted, overflowed) = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .fold((0, 0), |(admitted, overflowed), result| {
                (admitted + result.0, overflowed + result.1)
            });
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(admitted, BLINK_LEDGER_CAPACITY);
        assert_eq!(overflowed, 1);
        assert_eq!(snapshot.generation_pending, BLINK_LEDGER_CAPACITY as u64);
        assert_eq!(snapshot.generation_overflow, 1);
        assert!(!snapshot.poisoned);
        assert_eq!(ledger.generations.lock().unwrap().len(), BLINK_LEDGER_CAPACITY);
        assert!(ledger.selected.lock().unwrap().is_empty());
    }

    #[test]
    fn replacement_without_old_generation_poison_fails_closed() {
        let ledger = BlinkMeasurementLedgerV1::default();
        ledger.record_observed().unwrap();
        assert_eq!(
            ledger.record_submission(1, SlotSubmit::Replaced),
            Err(EdgeMeasurementError::GenerationConflict)
        );
        assert!(ledger.snapshot().unwrap().poisoned);
    }

    fn owner_config() -> EdgeMeasurementOwnerConfigV1 {
        let v2_adapter = Address::repeat_byte(2);
        let v3_adapter = Address::repeat_byte(3);
        let aerodrome_adapter = Address::repeat_byte(4);
        let executor_runtime_hash = B256::repeat_byte(20);
        let v2_adapter_runtime_hash = B256::repeat_byte(21);
        let v3_adapter_runtime_hash = B256::repeat_byte(22);
        let aerodrome_adapter_runtime_hash = B256::repeat_byte(23);
        let identities = [
            (crate::MEASUREMENT_EXECUTOR, executor_runtime_hash),
            (v2_adapter, v2_adapter_runtime_hash),
            (v3_adapter, v3_adapter_runtime_hash),
            (aerodrome_adapter, aerodrome_adapter_runtime_hash),
        ];
        EdgeMeasurementOwnerConfigV1 {
            producer_epoch: 1,
            output_root: std::env::temp_dir(),
            output_root_handle: Arc::new(File::open(std::env::temp_dir()).expect("temporary root")),
            producer_digest: B256::repeat_byte(1),
            reject_schema_digest: BlinkRejectClassifierV3::reject_schema_digest(),
            prereg_digest: B256::repeat_byte(3),
            policy_digest: B256::repeat_byte(4),
            config_digest: B256::repeat_byte(5),
            owner_approval_receipt_digest: B256::repeat_byte(6),
            record_queue_capacity: 4,
            candidate_queue_capacity: 4,
            measurement_sender: Address::repeat_byte(1),
            executor_runtime_hash,
            v2_adapter,
            v2_adapter_runtime_hash,
            v3_adapter,
            v3_adapter_runtime_hash,
            aerodrome_adapter,
            aerodrome_adapter_runtime_hash,
            g0_code_identity_digest: EdgeMeasurementOwnerV1::deployment_identity_digest(
                &identities,
            ),
            raw_reject_inventory_sha256: EdgeMeasurementOwnerConfigV1::raw_reject_inventory_sha256(
            ),
            raw_reject_source_sha256: B256::new(
                DefaultCrypto.sha256(include_bytes!("edge_measurement.rs")),
            ),
            measurement_binding_source_sha256: B256::new(
                DefaultCrypto.sha256(include_bytes!("measurement_tx.rs")),
            ),
        }
    }

    fn candidate_fixture_owner_config() -> EdgeMeasurementOwnerConfigV1 {
        let mut config = owner_config();
        let empty_runtime_hash = keccak256([]);
        config.executor_runtime_hash = empty_runtime_hash;
        config.v2_adapter_runtime_hash = empty_runtime_hash;
        config.v3_adapter_runtime_hash = empty_runtime_hash;
        config.aerodrome_adapter_runtime_hash = empty_runtime_hash;
        config.g0_code_identity_digest = EdgeMeasurementOwnerV1::deployment_identity_digest(&[
            (crate::MEASUREMENT_EXECUTOR, empty_runtime_hash),
            (config.v2_adapter, empty_runtime_hash),
            (config.v3_adapter, empty_runtime_hash),
            (config.aerodrome_adapter, empty_runtime_hash),
        ]);
        config
    }

    fn candidate_fixture_evidence() -> crate::EdgeSnapshotEvidenceV1 {
        crate::EdgeSnapshotEvidenceV1 {
            source_generation: 1,
            pending_snapshot_sequence: 2,
            coverage_sequence: 3,
            payload_first_record_sequence: 4,
            payload_first_record_hash: B256::repeat_byte(5),
            structural_terminal_hash: B256::repeat_byte(6),
            connection_sequence: 7,
            connection_record_hash: B256::repeat_byte(8),
            registry_terminal_record_hash: B256::repeat_byte(9),
        }
    }

    const fn cutoff_fields(latch_mono_ns: u64) -> ProducerEpochCutoffFieldsV1 {
        ProducerEpochCutoffFieldsV1 {
            producer_epoch: 1,
            cutoff_clock_observation_ordinal: 0,
            last_admitted_wire_ordinal: 0,
            last_admitted_source_generation: 0,
            last_admitted_blink_generation: 0,
            last_pending_snapshot_sequence: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
            latch_mono_ns,
        }
    }

    #[test]
    fn checked_candidate_bounds_distinguish_empty_from_first_sequence_zero() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        assert_eq!(owner.cutoff_cursors().unwrap().1, 0);

        owner.candidate_sequence.store(1, Ordering::Release);
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 1, last_sequence: Some(0) }
        );
        assert_eq!(owner.cutoff_cursors().unwrap().1, 1);
        owner.candidate_sequence.store(u64::MAX, Ordering::Release);
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: u64::MAX, last_sequence: Some(u64::MAX - 1) }
        );
    }

    #[test]
    fn cutoff_is_idempotent_only_for_identical_fields() {
        let latch = ProducerEpochCutoffLatchV1::default();
        let cutoff = ProducerEpochCutoffV1::new(cutoff_fields(10));
        assert_eq!(latch.latch(cutoff.clone()), Ok(()));
        assert_eq!(latch.latch(cutoff), Ok(()));
        assert_eq!(
            latch.latch(ProducerEpochCutoffV1::new(cutoff_fields(11))),
            Err(EdgeMeasurementError::CutoffConflict)
        );

        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.latch_cutoff(cutoff_fields(10));
        owner.latch_cutoff(cutoff_fields(10));
        assert!(owner.cutoff_drained_snapshot().is_ok());

        let conflicting_owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        conflicting_owner.latch_cutoff(cutoff_fields(10));
        conflicting_owner.latch_cutoff(cutoff_fields(11));
        let conflicting = conflicting_owner.raw_accounting_snapshot().unwrap();
        assert_eq!(conflicting.cutoff.as_ref().unwrap().latch_mono_ns, 10);
        assert_eq!(conflicting.revision[34], 1);
    }

    #[test]
    fn cutoff_deadline_terminalizes_unresolved_blink_authority_without_poisoning() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.observe_ledger_result_admitted(owner.ledger().record_observed());
        owner.record_submission_admitted(0, SlotSubmit::Accepted);
        let started = Instant::now();
        let bounds = owner.prepare_cutoff_until(started).expect("bounded cutoff");
        assert_eq!(bounds.0, 1);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(owner.generation_authority.lock().expect("authority").is_empty());
        assert!(!owner.poisoned.load(Ordering::Acquire));
        owner.latch_cutoff(cutoff_fields(10));
        assert!(owner.cutoff_drained_snapshot().is_ok());
    }

    #[test]
    fn cutoff_deadline_sweeps_staged_production_draft_without_poisoning_snapshot() {
        let owner = EdgeMeasurementOwnerV1::new(candidate_fixture_owner_config()).unwrap();
        candidate_fixture::stage(&owner, candidate_fixture_evidence(), true, false)
            .expect("production candidate staging");
        assert_eq!(owner.staged.lock().unwrap().len(), 1);

        let bounds = owner.prepare_cutoff_until(Instant::now()).expect("bounded cutoff");
        assert_eq!(bounds.0, 1);
        assert_eq!(bounds.1, CheckedCandidateBoundsV1 { count: 0, last_sequence: None });
        assert!(owner.staged.lock().unwrap().is_empty());
        assert!(owner.generation_authority.lock().unwrap().is_empty());
        assert_eq!(
            owner.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 { cutoff_drain_deadline: 1, ..Default::default() }
        );
        assert_eq!(
            owner.drain_records().unwrap(),
            vec![EdgeProducerRecordV1::CandidateDrop {
                generation: 0,
                reason: CandidatePreEnqueueDropReasonV3::CutoffDrainDeadline,
            }]
        );

        owner.latch_cutoff(cutoff_fields(10));
        let snapshot = owner.cutoff_drained_snapshot().expect("cutoff-drained snapshot");
        assert_eq!(snapshot.blink.cancelled_before_frame, 1);
        assert_eq!(snapshot.blink.selected_dto_built_preterminal, 1);
        assert_eq!(snapshot.blink.selected_dto_preterminal_failure, 1);
        assert!(!snapshot.blink.poisoned);
    }
    #[test]
    fn late_post_cutoff_terminal_without_authority_is_an_inert_noop() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.observe_ledger_result_admitted(owner.ledger().record_observed());
        owner.record_submission_admitted(0, SlotSubmit::Accepted);
        owner.terminalize_shutdown_pending_admitted(None);
        owner.latch_cutoff(cutoff_fields(10));

        owner.record_terminal_and_resolve(
            0,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::Selected),
        );

        assert!(!owner.poisoned.load(Ordering::Acquire));
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        assert!(owner.cutoff_drained_snapshot().is_ok());
    }
    #[test]
    fn cutoff_cannot_split_observed_from_slot_accounting() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        let worker_owner = Arc::clone(&owner);
        let (observed_tx, observed_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            worker_owner.with_blink_admission(|owner, authoritative| {
                assert!(authoritative);
                owner.observe_ledger_result_admitted(owner.ledger().record_observed());
                observed_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                owner.observe_ledger_result_admitted(owner.ledger().record_slot_closed());
            });
        });
        observed_rx.recv().unwrap();

        let cutoff_owner = Arc::clone(&owner);
        let (cutoff_tx, cutoff_rx) = mpsc::channel();
        let cutoff = thread::spawn(move || {
            cutoff_tx.send(cutoff_owner.prepare_cutoff()).unwrap();
        });
        assert!(cutoff_rx.recv_timeout(Duration::from_millis(20)).is_err());
        continue_tx.send(()).unwrap();
        worker.join().unwrap();

        let (admitted, candidates) = cutoff_rx.recv().unwrap().unwrap();
        cutoff.join().unwrap();
        assert_eq!(admitted, 0);
        assert_eq!(candidates, CheckedCandidateBoundsV1 { count: 0, last_sequence: None });
        let snapshot = owner.ledger().snapshot().unwrap();
        assert_eq!(snapshot.victim_ingress_observed, 1);
        assert_eq!(snapshot.slot_closed, 1);

        owner.with_blink_admission(|_, authoritative| assert!(!authoritative));
    }

    #[test]
    fn cutoff_before_stage_drains_admitted_authority_before_freezing_bounds() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.with_blink_admission(|owner, authoritative| {
            assert!(authoritative);
            owner.record_submission_admitted(0, SlotSubmit::Accepted);
        });

        let cutoff_owner = Arc::clone(&owner);
        let (cutoff_tx, cutoff_rx) = mpsc::channel();
        let cutoff = thread::spawn(move || {
            cutoff_tx.send(cutoff_owner.prepare_cutoff()).unwrap();
        });
        while owner.is_accepting() {
            thread::yield_now();
        }
        assert!(cutoff_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(owner.generation_is_authoritative(0).unwrap());

        owner.candidate_sequence.fetch_add(1, Ordering::SeqCst);
        owner.record_terminal_and_resolve(
            0,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::NoCandidate),
        );

        let (admitted, bounds) = cutoff_rx.recv().unwrap().unwrap();
        cutoff.join().unwrap();
        assert_eq!(admitted, 1);
        assert_eq!(bounds, CheckedCandidateBoundsV1 { count: 1, last_sequence: Some(0) });
    }

    #[test]
    fn cutoff_during_stage_waits_for_stage_and_terminal_drain() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.with_blink_admission(|owner, authoritative| {
            assert!(authoritative);
            owner.record_submission_admitted(0, SlotSubmit::Accepted);
        });

        let stage_owner = Arc::clone(&owner);
        let (stage_tx, stage_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let stage = thread::spawn(move || {
            stage_owner.with_blink_admission(|owner, authoritative| {
                assert!(authoritative);
                stage_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                owner.candidate_sequence.fetch_add(1, Ordering::SeqCst);
            });
        });
        stage_rx.recv().unwrap();

        let cutoff_owner = Arc::clone(&owner);
        let (cutoff_tx, cutoff_rx) = mpsc::channel();
        let cutoff = thread::spawn(move || {
            cutoff_tx.send(cutoff_owner.prepare_cutoff()).unwrap();
        });
        assert!(cutoff_rx.recv_timeout(Duration::from_millis(20)).is_err());
        continue_tx.send(()).unwrap();
        stage.join().unwrap();
        while owner.is_accepting() {
            thread::yield_now();
        }
        owner.record_terminal_and_resolve(
            0,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::NoCandidate),
        );

        let (_, bounds) = cutoff_rx.recv().unwrap().unwrap();
        cutoff.join().unwrap();
        assert_eq!(bounds, CheckedCandidateBoundsV1 { count: 1, last_sequence: Some(0) });
    }
    #[test]
    fn selected_terminal_without_staged_draft_poisons_owner() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.with_blink_admission(|owner, authoritative| {
            assert!(authoritative);
            owner.observe_ledger_result_admitted(owner.ledger().record_observed());
            owner.record_submission_admitted(1, SlotSubmit::Accepted);
        });
        owner.record_terminal_and_resolve(
            1,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::Selected),
        );
        assert!(owner.poisoned.load(Ordering::SeqCst));
        assert_eq!(owner.checked_candidate_bounds().count, 0);
    }

    #[test]
    fn staging_failure_drop_is_counted_in_cutoff_snapshot() {
        let owner = EdgeMeasurementOwnerV1::new(candidate_fixture_owner_config()).unwrap();
        let mut evidence = candidate_fixture_evidence();
        evidence.structural_terminal_hash = B256::ZERO;
        assert_eq!(
            candidate_fixture::stage(&owner, evidence, true, false),
            Err(EdgeProducerError::CandidateEvidenceMismatch)
        );
        owner.record_terminal_and_resolve(
            0,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::Selected),
        );

        assert_eq!(
            owner.drain_records().unwrap(),
            vec![EdgeProducerRecordV1::CandidateDrop {
                generation: 0,
                reason: CandidatePreEnqueueDropReasonV3::EvidenceMismatch,
            }]
        );
        assert_eq!(
            owner.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 { evidence_mismatch: 1, ..Default::default() }
        );
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        owner.latch_cutoff(cutoff_fields(10));
        let snapshot = owner.cutoff_drained_snapshot().unwrap();
        assert_eq!(snapshot.blink.processed_terminal, 1);
        assert_eq!(snapshot.blink.selected_dto_built_preterminal, 0);
        assert_eq!(snapshot.blink.selected_dto_committed, 0);
        assert!(!snapshot.blink.poisoned);
    }

    #[test]
    fn non_authoritative_staging_is_named_and_counted_without_poisoning() {
        let owner = EdgeMeasurementOwnerV1::new(candidate_fixture_owner_config()).unwrap();
        assert_eq!(
            candidate_fixture::stage(&owner, candidate_fixture_evidence(), false, false),
            Err(EdgeProducerError::CandidateEvidenceMismatch)
        );
        assert_eq!(
            owner.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 {
                missing_required_evidence: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            owner.drain_records().unwrap(),
            vec![EdgeProducerRecordV1::CandidateDrop {
                generation: 0,
                reason: CandidatePreEnqueueDropReasonV3::MissingRequiredEvidence,
            }]
        );
        assert!(owner.staged.lock().unwrap().is_empty());
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        assert!(!owner.poisoned.load(Ordering::Acquire));
    }
    #[test]
    fn post_cutoff_non_authoritative_staging_is_measurement_inert() {
        let owner = EdgeMeasurementOwnerV1::new(candidate_fixture_owner_config()).unwrap();
        owner.latch_cutoff(cutoff_fields(10));

        assert_eq!(
            candidate_fixture::stage(&owner, candidate_fixture_evidence(), false, false),
            Ok(())
        );
        assert_eq!(
            owner.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1::default()
        );
        assert_eq!(owner.pending_records.load(Ordering::Acquire), 0);
        assert!(owner.staged.lock().unwrap().is_empty());
        assert!(owner.staging_failures.lock().unwrap().is_empty());
        assert!(owner.drain_records().unwrap().is_empty());
        assert!(owner.cutoff_drained_snapshot().is_ok());
    }

    #[test]
    fn bounded_candidate_receive_preserves_order_and_decrements_pending_once() {
        let owner = EdgeMeasurementOwnerV1::new(candidate_fixture_owner_config()).unwrap();
        candidate_fixture::stage(&owner, candidate_fixture_evidence(), true, true).unwrap();
        assert_eq!(owner.pending_candidates.load(Ordering::Acquire), 1);

        let first = owner.try_receive_candidate_detail().unwrap().unwrap();
        assert_eq!(first.candidate_sequence, 0);
        assert_eq!(owner.pending_candidates.load(Ordering::Acquire), 0);
        assert_eq!(owner.try_receive_candidate_detail().unwrap(), None);
        assert_eq!(owner.pending_candidates.load(Ordering::Acquire), 0);

        let mut second = first.clone();
        second.candidate_sequence = 1;
        owner.candidate_sender.try_send(first).unwrap();
        owner.pending_candidates.fetch_add(1, Ordering::Release);
        owner.candidate_sender.try_send(second).unwrap();
        owner.pending_candidates.fetch_add(1, Ordering::Release);

        assert_eq!(owner.try_receive_candidate_detail().unwrap().unwrap().candidate_sequence, 0);
        assert_eq!(owner.pending_candidates.load(Ordering::Acquire), 1);
        assert_eq!(owner.try_receive_candidate_detail().unwrap().unwrap().candidate_sequence, 1);
        assert_eq!(owner.pending_candidates.load(Ordering::Acquire), 0);
        assert_eq!(owner.try_receive_candidate_detail().unwrap(), None);
    }
    #[cfg(feature = "test-utils")]
    #[test]
    fn owner_authority_audit_round_trips_exact_queue_and_staged_payloads_without_state_change() {
        let config = candidate_fixture_owner_config();
        let owner = EdgeMeasurementOwnerV1::new(config.clone()).unwrap();
        let first_record_hash = EdgeMeasurementOwnerV1::authority_record_hash(
            "edge-blink-reject/v3",
            "{\"branchId\":\"wire-end\",\"previousRecordHash\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"producerEpoch\":\"1\",\"reason\":\"WireEnd\",\"schema\":\"edge-blink-reject/v3\",\"sequence\":\"0\",\"state\":\"Rejected\"}",
        )
        .unwrap();
        let second_record_hash = EdgeMeasurementOwnerV1::authority_record_hash(
            "edge-blink-reject/v3",
            &format!(
                "{{\"branchId\":\"decode-json-syntax\",\"previousRecordHash\":\"{}\",\"producerEpoch\":\"1\",\"reason\":\"JsonSyntax\",\"schema\":\"edge-blink-reject/v3\",\"sequence\":\"1\",\"state\":\"Rejected\"}}",
                hex::encode(first_record_hash)
            ),
        )
        .unwrap();
        let expected_records = vec![
            EdgeProducerRecordV1::BlinkReject(BlinkRejectRecordV3 {
                schema: "edge-blink-reject/v3",
                producer_epoch: "1".to_owned(),
                sequence: "0".to_owned(),
                state: "Rejected",
                branch_id: "wire-end",
                reason: BlinkRejectReasonV3::WireEnd,
                previous_record_hash: hex::encode([0; 32]),
                record_hash: hex::encode(first_record_hash),
            }),
            EdgeProducerRecordV1::BlinkReject(BlinkRejectRecordV3 {
                schema: "edge-blink-reject/v3",
                producer_epoch: "1".to_owned(),
                sequence: "1".to_owned(),
                state: "Rejected",
                branch_id: "decode-json-syntax",
                reason: BlinkRejectReasonV3::JsonSyntax,
                previous_record_hash: hex::encode(first_record_hash),
                record_hash: hex::encode(second_record_hash),
            }),
        ];
        let victim_raw = Bytes::from_str(&format!("0x{}", candidate_fixture::RAW_VICTIM)).unwrap();
        let victim_hash = keccak256(&victim_raw);
        let victim_transaction = BaseTxEnvelope::decode_2718_exact(victim_raw.as_ref()).unwrap();
        let victim_sender = victim_transaction.recover_signer().unwrap();
        let parent_header = Sealed::new(Header {
            number: candidate_fixture::BLOCK_NUMBER - 1,
            gas_limit: 30_000_000,
            state_root: B256::repeat_byte(0x44),
            ..Default::default()
        });
        let parent_hash = parent_header.hash();
        let token = Address::repeat_byte(0x51);
        let expected_route = [
            PreparedPoolState {
                pool: Address::repeat_byte(0x52),
                protocol: ExactProtocol::UniswapV2,
                token0: WETH,
                token1: token,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(2_000_000),
                },
            },
            PreparedPoolState {
                pool: Address::repeat_byte(0x53),
                protocol: ExactProtocol::UniswapV2,
                token0: token,
                token1: WETH,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(1_000_000),
                    reserve1: U256::from(1_000_000),
                },
            },
        ];
        let quote_v2 = |amount_in: U256, reserve_in: U256, reserve_out: U256| {
            let amount_in_with_fee = amount_in * U256::from(997_000);
            amount_in_with_fee * reserve_out
                / (reserve_in * U256::from(1_000_000) + amount_in_with_fee)
        };
        let amount_in = U256::from(1_000);
        let first_out = quote_v2(amount_in, U256::from(1_000_000), U256::from(2_000_000));
        let amount_out = quote_v2(first_out, U256::from(1_000_000), U256::from(1_000_000));
        let mut expected_plan = BackrunPlan {
            parent_hash,
            block_number: candidate_fixture::BLOCK_NUMBER,
            predecessor_index: candidate_fixture::PREDECESSOR_INDEX,
            payload_id: candidate_fixture::payload_id(),
            victim: victim_hash,
            route: [
                BackrunHop {
                    pool: Address::repeat_byte(0x52),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: Address::repeat_byte(0x53),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: 3_000,
                },
            ],
            amount_in,
            amount_out,
            gross_profit: amount_out.checked_sub(amount_in).unwrap(),
            digest: BackrunPlanDigest(B256::ZERO),
        };
        expected_plan.digest = MeasurementEncoder::digest(&expected_plan).unwrap();
        let expected_execution_hops = [
            MeasurementExecutionHopV1 {
                adapter: config.v2_adapter,
                min_amount_out: first_out,
                funding_target: Address::repeat_byte(0x52),
            },
            MeasurementExecutionHopV1 {
                adapter: config.v2_adapter,
                min_amount_out: amount_out,
                funding_target: Address::repeat_byte(0x53),
            },
        ];
        let empty_runtime_hash = keccak256([]);
        let expected_deployment_identities = [
            (crate::MEASUREMENT_EXECUTOR, empty_runtime_hash),
            (config.v2_adapter, empty_runtime_hash),
            (config.v3_adapter, empty_runtime_hash),
            (config.aerodrome_adapter, empty_runtime_hash),
        ];
        let expected_binding = MeasurementBindingDeriverV1::bind(MeasurementBindingInputV1 {
            nonce: SnapshotNonceWitnessV1::committed(
                parent_hash,
                candidate_fixture::BLOCK_NUMBER - 1,
                0,
            ),
            snapshot_base_fee_per_gas: 1,
            plan: expected_plan.clone(),
            execution_hops: expected_execution_hops,
            victim_raw: victim_raw.clone(),
        })
        .unwrap();
        let mut ordered_transaction_canonical = b"edge-ordered-transaction-cutoff/v1\0".to_vec();
        ordered_transaction_canonical
            .extend_from_slice(&candidate_fixture::BLOCK_NUMBER.to_be_bytes());
        ordered_transaction_canonical.extend_from_slice(&0_u64.to_be_bytes());
        let expected_ordered_transaction_digest =
            B256::new(DefaultCrypto.sha256(&ordered_transaction_canonical));
        let fixture_evidence = candidate_fixture_evidence();
        let expected_materialized_keys = BTreeSet::from([
            AuditedWriteKey::AccountBalance {
                address: victim_sender,
                evidence_digest: B256::repeat_byte(0x45),
            },
            AuditedWriteKey::AccountNonce {
                address: victim_sender,
                evidence_digest: B256::repeat_byte(0x46),
            },
            AuditedWriteKey::AccountBalance {
                address: Address::ZERO,
                evidence_digest: B256::repeat_byte(0x47),
            },
        ]);
        let materialized_golden = |materialized: &MaterializedState| {
            assert_eq!(
                materialized.writes.iter().map(|write| write.key).collect::<BTreeSet<_>>(),
                expected_materialized_keys
            );
            let mut canonical = b"edge-materialized-state/v1\0".to_vec();
            canonical.extend_from_slice(
                &u32::try_from(materialized.writes.len()).unwrap().to_be_bytes(),
            );
            for write in &materialized.writes {
                let (variant, address, slot, evidence_digest) = match write.key {
                    AuditedWriteKey::AccountBalance { address, evidence_digest } => {
                        (0, address, U256::ZERO, evidence_digest)
                    }
                    AuditedWriteKey::AccountNonce { address, evidence_digest } => {
                        (1, address, U256::ZERO, evidence_digest)
                    }
                    AuditedWriteKey::Storage { address, slot, evidence_digest } => {
                        (2, address, slot, evidence_digest)
                    }
                };
                canonical.push(variant);
                canonical.extend_from_slice(address.as_slice());
                canonical.extend_from_slice(&slot.to_be_bytes::<32>());
                canonical.extend_from_slice(evidence_digest.as_slice());
                canonical.extend_from_slice(&write.value.to_be_bytes::<32>());
            }
            B256::new(DefaultCrypto.sha256(&canonical))
        };
        let assert_candidate_oracle = |candidate: &EdgeCandidateDetailV1, generation, sequence| {
            assert_eq!(candidate.producer_epoch, config.producer_epoch);
            assert_eq!(candidate.candidate_sequence, sequence);
            assert_eq!(candidate.candidate_generation, generation);
            assert_eq!(candidate.source_generation, fixture_evidence.source_generation);
            assert_eq!(candidate.coverage_generation, fixture_evidence.coverage_sequence);
            assert_eq!(
                candidate.pending_snapshot_sequence,
                fixture_evidence.pending_snapshot_sequence
            );
            assert_eq!(
                candidate.payload_first_record_sequence,
                fixture_evidence.payload_first_record_sequence
            );
            assert_eq!(
                candidate.payload_first_record_hash,
                fixture_evidence.payload_first_record_hash.0
            );
            assert_eq!(
                candidate.structural_terminal_hash,
                fixture_evidence.structural_terminal_hash.0
            );
            assert_eq!(
                candidate.connection_sequence_at_capture,
                fixture_evidence.connection_sequence
            );
            assert_eq!(
                candidate.connection_record_hash_at_capture,
                fixture_evidence.connection_record_hash.0
            );
            assert_eq!(
                candidate.registry_terminal_record_hash,
                fixture_evidence.registry_terminal_record_hash.0
            );
            assert_eq!(candidate.parent_hash, parent_hash);
            assert_eq!(candidate.block_number, candidate_fixture::BLOCK_NUMBER);
            assert_eq!(candidate.payload_id, candidate_fixture::payload_id().0.0);
            assert_eq!(candidate.predecessor_index, candidate_fixture::PREDECESSOR_INDEX);
            assert_eq!(candidate.state_root, B256::repeat_byte(0x44));
            assert_eq!(candidate.ordered_transaction_count, 1);
            assert_eq!(candidate.ordered_transaction_cutoff_position, 0);
            assert!(candidate.victim_absent_before_position);
            assert_eq!(candidate.ordered_transaction_digest, expected_ordered_transaction_digest);
            assert_eq!(candidate.victim_hash, victim_hash);
            assert_eq!(candidate.victim_raw, victim_raw);
            assert_eq!(candidate.selected_plan, expected_plan);
            assert_eq!(candidate.selected_plan_digest, expected_plan.digest.0);
            assert_eq!(candidate.prepared_route, expected_route);
            assert_eq!(candidate.execution_hops, expected_execution_hops);
            assert_eq!(candidate.deployment_identities, expected_deployment_identities);
            assert_eq!(
                candidate.code_witness_digest,
                EdgeMeasurementOwnerV1::deployment_identity_digest(&expected_deployment_identities)
            );
            assert_eq!(candidate.g0_code_identity_digest, config.g0_code_identity_digest);
            assert_eq!(candidate.prereg_digest, config.prereg_digest);
            assert_eq!(candidate.policy_digest, config.policy_digest);
            assert_eq!(candidate.config_digest, config.config_digest);
            assert_eq!(candidate.producer_digest, config.producer_digest);
            assert_eq!(
                candidate.owner_approval_receipt_digest,
                config.owner_approval_receipt_digest
            );
            assert_eq!(candidate.backrun_measurement_binding, expected_binding);
            assert_eq!(
                EdgeMeasurementOwnerV1::prepared_route_digest(&candidate.prepared_route).unwrap(),
                candidate.prepared_state_digest
            );
            assert_eq!(
                materialized_golden(&candidate.materialized_state),
                candidate.slot_witness_digest
            );
            assert_eq!(
                EdgeMeasurementOwnerV1::economics_digest(
                    &candidate.selected_plan,
                    &candidate.execution_hops
                )
                .unwrap(),
                candidate.economics_evidence_digest
            );
            candidate.validate_detail().unwrap();
        };
        owner.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        owner.emit_blink_reject("decode-json-syntax", BlinkRejectReasonV3::JsonSyntax);

        owner
            .stage_deterministic_test_candidate(candidate_fixture_evidence())
            .expect("production terminalized candidate");
        owner
            .stage_deterministic_test_candidate_without_terminal(candidate_fixture_evidence())
            .expect("production staged candidate");

        let before = owner.authority_audit_snapshot();
        assert_eq!(before.record_queue, expected_records);
        assert_eq!(before.candidate_queue.len(), 1);
        assert_candidate_oracle(&before.candidate_queue[0], 0, 0);

        assert_eq!(before.staged.keys().copied().collect::<Vec<_>>(), vec![1]);
        let staged = before.staged.get(&1).unwrap();
        assert_eq!(staged.generation, 1);
        assert_eq!(staged.source_generation, fixture_evidence.source_generation);
        assert_eq!(staged.coverage_generation, fixture_evidence.coverage_sequence);
        assert_eq!(staged.pending_snapshot_sequence, fixture_evidence.pending_snapshot_sequence);
        assert_eq!(
            staged.payload_first_record_sequence,
            fixture_evidence.payload_first_record_sequence
        );
        assert_eq!(staged.payload_first_record_hash, fixture_evidence.payload_first_record_hash.0);
        assert_eq!(staged.structural_terminal_hash, fixture_evidence.structural_terminal_hash.0);
        assert_eq!(staged.connection_sequence_at_capture, fixture_evidence.connection_sequence);
        assert_eq!(
            staged.connection_record_hash_at_capture,
            fixture_evidence.connection_record_hash.0
        );
        assert_eq!(
            staged.registry_terminal_record_hash,
            fixture_evidence.registry_terminal_record_hash.0
        );
        assert_eq!(staged.parent_hash, parent_hash);
        assert_eq!(staged.state_root, B256::repeat_byte(0x44));
        assert_eq!(staged.ordered_transaction_count, 1);
        assert_eq!(staged.ordered_transaction_cutoff_position, 0);
        assert!(staged.victim_absent_before_position);
        assert_eq!(staged.ordered_transaction_digest, expected_ordered_transaction_digest);
        assert_eq!(staged.victim_raw, victim_raw);
        assert_eq!(staged.selected_plan, expected_plan);
        assert_eq!(staged.prepared_route, expected_route);
        assert_eq!(staged.execution_hops, expected_execution_hops);
        assert_eq!(staged.deployment_identities, expected_deployment_identities);
        assert_eq!(staged.backrun_measurement_binding, expected_binding);
        assert_eq!(
            EdgeMeasurementOwnerV1::prepared_route_digest(&staged.prepared_route).unwrap(),
            staged.prepared_state_digest
        );
        assert_eq!(
            staged.code_witness_digest,
            EdgeMeasurementOwnerV1::deployment_identity_digest(&expected_deployment_identities)
        );
        assert_eq!(materialized_golden(&staged.materialized_state), staged.slot_witness_digest);
        assert_eq!(
            EdgeMeasurementOwnerV1::economics_digest(&staged.selected_plan, &staged.execution_hops)
                .unwrap(),
            staged.economics_evidence_digest
        );
        assert_eq!(before.staged.len(), 1);
        assert_eq!(before.pending_records, 2);
        assert_eq!(before.pending_candidates, 1);
        assert_eq!(before.reject_sequence, 2);
        assert_eq!(before.candidate_sequence, 1);

        let after_first_audit = owner.authority_audit_snapshot();
        let after_second_audit = owner.authority_audit_snapshot();
        assert_eq!(after_first_audit, before);
        assert_eq!(after_second_audit, before);

        assert_eq!(owner.drain_records().unwrap(), expected_records);
        let first_candidates = owner.drain_candidate_details().unwrap();
        assert_eq!(first_candidates.len(), 1);
        assert_candidate_oracle(&first_candidates[0], 0, 0);
        owner.record_terminal_and_resolve(
            1,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::Selected),
        );
        let staged_candidates = owner.drain_candidate_details().unwrap();
        assert_eq!(staged_candidates.len(), 1);
        assert_candidate_oracle(&staged_candidates[0], 1, 1);
        assert!(owner.drain_records().unwrap().is_empty());
        assert!(owner.drain_candidate_details().unwrap().is_empty());
        assert!(owner.staged.lock().unwrap().is_empty());
    }

    #[test]
    fn blink_reject_full_and_closed_losses_are_counted_without_advancing_or_poisoning() {
        let mut full_config = owner_config();
        full_config.record_queue_capacity = 1;
        let full = EdgeMeasurementOwnerV1::new(full_config).unwrap();
        full.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        let accepted_hash = *full.reject_previous_hash.lock().unwrap();
        full.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        full.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        assert_eq!(
            full.blink_reject_queue_loss_counters(),
            BlinkRejectQueueLossCountersV1 { queue_full: 2, queue_closed: 0 }
        );
        assert_eq!(full.reject_sequence.load(Ordering::Acquire), 1);
        assert_eq!(*full.reject_previous_hash.lock().unwrap(), accepted_hash);
        assert!(!full.poisoned.load(Ordering::Acquire));
        assert_eq!(full.drain_records().unwrap().len(), 1);
        full.latch_cutoff(cutoff_fields(10));
        assert!(full.cutoff_drained_snapshot().is_ok());

        let closed = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        let (_replacement_sender, replacement_receiver) = sync_channel(1);
        let original_receiver =
            std::mem::replace(&mut *closed.record_receiver.lock().unwrap(), replacement_receiver);
        drop(original_receiver);
        closed.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        assert_eq!(
            closed.blink_reject_queue_loss_counters(),
            BlinkRejectQueueLossCountersV1 { queue_full: 0, queue_closed: 1 }
        );
        assert_eq!(closed.reject_sequence.load(Ordering::Acquire), 0);
        assert_eq!(*closed.reject_previous_hash.lock().unwrap(), [0; 32]);
        assert!(!closed.poisoned.load(Ordering::Acquire));
        closed.latch_cutoff(cutoff_fields(10));
        assert!(closed.cutoff_drained_snapshot().is_ok());
    }

    #[test]
    fn candidate_drop_full_and_closed_losses_are_counted_without_poisoning() {
        let mut full_config = owner_config();
        full_config.record_queue_capacity = 1;
        let full = EdgeMeasurementOwnerV1::new(full_config).unwrap();
        full.emit_candidate_drop(0, CandidatePreEnqueueDropReasonV3::MissingRequiredEvidence);
        full.emit_candidate_drop(1, CandidatePreEnqueueDropReasonV3::EvidenceMismatch);
        full.emit_candidate_drop(2, CandidatePreEnqueueDropReasonV3::MeasurementDerivationRejected);
        assert_eq!(
            full.candidate_drop_queue_loss_counters(),
            CandidateDropQueueLossCountersV1 { queue_full: 2, queue_closed: 0 }
        );
        assert_eq!(
            full.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 {
                missing_required_evidence: 1,
                evidence_mismatch: 1,
                measurement_derivation_rejected: 1,
                ..Default::default()
            }
        );
        assert!(!full.poisoned.load(Ordering::Acquire));
        assert_eq!(full.drain_records().unwrap().len(), 1);

        let closed = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        let (_replacement_sender, replacement_receiver) = sync_channel(1);
        let original_receiver =
            std::mem::replace(&mut *closed.record_receiver.lock().unwrap(), replacement_receiver);
        drop(original_receiver);
        closed.emit_candidate_drop(3, CandidatePreEnqueueDropReasonV3::CandidateQueueClosed);
        assert_eq!(
            closed.candidate_drop_queue_loss_counters(),
            CandidateDropQueueLossCountersV1 { queue_full: 0, queue_closed: 1 }
        );
        assert_eq!(
            closed.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 { candidate_queue_closed: 1, ..Default::default() }
        );
        assert!(!closed.poisoned.load(Ordering::Acquire));
    }
    #[test]
    fn full_candidate_and_record_queues_preserve_exact_drop_count_at_cutoff() {
        let mut config = owner_config();
        config.record_queue_capacity = 1;
        let owner = EdgeMeasurementOwnerV1::new(config).unwrap();
        owner.with_blink_admission(|owner, authoritative| {
            assert!(authoritative);
            owner.observe_ledger_result_admitted(owner.ledger().record_observed());
            owner.record_submission_admitted(0, SlotSubmit::Accepted);
            owner.observe_ledger_result_admitted(
                owner.ledger().record_selected_preterminal_authorized(0),
            );
        });
        owner.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        let (candidate_sender, _candidate_receiver) = sync_channel(1);
        candidate_sender.try_send(()).unwrap();
        let reason = EdgeMeasurementOwnerV1::try_send_candidate(&candidate_sender, ()).unwrap_err();
        owner.emit_candidate_drop(0, reason);
        owner.observe_ledger_result(owner.ledger().record_terminal(
            0,
            BlinkGenerationTerminalV1::Processed,
            Some(ShadowOutcome::Selected),
        ));
        owner.release_generation_authority(0);
        assert_eq!(
            owner.candidate_pre_enqueue_drop_counters(),
            CandidatePreEnqueueDropCountersV1 { candidate_queue_full: 1, ..Default::default() }
        );

        let records = owner.drain_records().unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records.as_slice(),
            [EdgeProducerRecordV1::BlinkReject(BlinkRejectRecordV3 {
                reason: BlinkRejectReasonV3::WireEnd,
                ..
            })]
        ));
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        owner.latch_cutoff(cutoff_fields(10));
        let snapshot = owner.cutoff_drained_snapshot().unwrap();
        assert_eq!(snapshot.blink.processed_terminal, 1);
        assert_eq!(snapshot.blink.selected_dto_built_preterminal, 1);
        assert_eq!(snapshot.blink.selected_dto_committed, 1);
        assert!(!snapshot.blink.poisoned);
    }
    #[test]
    fn cutoff_closes_reject_and_draft_admission() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.latch_cutoff(cutoff_fields(10));
        owner.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        owner.ledger.record_observed().unwrap();
        owner.ledger.record_submission(1, SlotSubmit::Accepted).unwrap();
        owner.ledger.record_selected_preterminal(1).unwrap();
        let blink = owner.ledger.snapshot().unwrap();
        assert_eq!(blink.victim_ingress_observed, 0);
        assert_eq!(blink.admitted_generations, 0);
        assert_eq!(blink.selected_dto_built_preterminal, 0);

        assert!(!owner.is_accepting());
        assert!(owner.drain_records().unwrap().is_empty());
        assert_eq!(owner.reject_sequence.load(Ordering::Acquire), 0);
        assert_eq!(
            owner.checked_candidate_bounds(),
            CheckedCandidateBoundsV1 { count: 0, last_sequence: None }
        );
        assert!(owner.staged.lock().unwrap().is_empty());
    }

    #[test]
    fn cutoff_snapshot_waits_for_pre_cutoff_terminal_without_poisoning() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.ledger.record_observed().unwrap();
        owner.ledger.record_submission(1, SlotSubmit::Accepted).unwrap();
        owner.latch_cutoff(cutoff_fields(10));

        assert_eq!(owner.cutoff_drained_snapshot(), Err(EdgeProducerError::Ledger));
        owner
            .ledger
            .record_terminal(
                1,
                BlinkGenerationTerminalV1::Processed,
                Some(ShadowOutcome::NoCandidate),
            )
            .unwrap();
        assert!(owner.cutoff_drained_snapshot().is_ok());
    }
    #[test]
    fn cutoff_snapshot_requires_cutoff_drained_queues_and_zero_pending_ledgers() {
        let missing_cutoff = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        assert_eq!(missing_cutoff.cutoff_drained_snapshot(), Err(EdgeProducerError::CutoffMissing));

        let queued = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        queued.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        queued.latch_cutoff(cutoff_fields(10));
        assert_eq!(queued.cutoff_drained_snapshot(), Err(EdgeProducerError::Ledger));

        let drained = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        drained.emit_blink_reject("wire-end", BlinkRejectReasonV3::WireEnd);
        drained.latch_cutoff(cutoff_fields(10));
        assert_eq!(drained.drain_records().unwrap().len(), 1);
        assert!(drained.cutoff_drained_snapshot().is_ok());

        let generation_pending = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        generation_pending.ledger.record_observed().unwrap();
        generation_pending.ledger.record_submission(1, SlotSubmit::Accepted).unwrap();
        generation_pending.latch_cutoff(cutoff_fields(10));
        assert_eq!(generation_pending.cutoff_drained_snapshot(), Err(EdgeProducerError::Ledger));

        let terminal_drain = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        terminal_drain.ledger.record_observed().unwrap();
        terminal_drain.ledger.record_submission(1, SlotSubmit::Accepted).unwrap();
        terminal_drain.latch_cutoff(cutoff_fields(10));
        terminal_drain
            .ledger
            .record_terminal(1, BlinkGenerationTerminalV1::Processed, None)
            .unwrap();
        assert!(terminal_drain.cutoff_drained_snapshot().is_ok());

        let candidate_pending = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        candidate_pending.pending_candidates.store(1, Ordering::Release);
        candidate_pending.latch_cutoff(cutoff_fields(10));
        assert_eq!(candidate_pending.cutoff_drained_snapshot(), Err(EdgeProducerError::Ledger));
    }
    #[test]
    fn cutoff_hash_and_sha256_match_shared_ts_contract() {
        let cutoff = ProducerEpochCutoffV1::new(ProducerEpochCutoffFieldsV1 {
            producer_epoch: 1,
            cutoff_clock_observation_ordinal: 2,
            last_admitted_wire_ordinal: 3,
            last_admitted_source_generation: 4,
            last_admitted_blink_generation: 5,
            last_pending_snapshot_sequence: 6,
            last_coverage_sequence: 7,
            last_candidate_sequence: 8,
            latch_mono_ns: 9,
        });
        assert_eq!(
            hex::encode(cutoff.record_hash),
            "ffdf47790035f3460cd4f16a70950718d255194411c98079729e0ae9716c6bd0"
        );
        assert!(cutoff.has_valid_record_hash());
        assert_eq!(
            cutoff.canonical_inner_bytes(),
            br#"{"cutoffClockObservationOrdinal":"2","lastAdmittedBlinkGeneration":"5","lastAdmittedSourceGeneration":"4","lastAdmittedWireOrdinal":"3","lastCandidateSequence":"8","lastCoverageSequence":"7","lastPendingSnapshotSequence":"6","latchMonoNs":"9","producerEpoch":"1","recordHash":"ffdf47790035f3460cd4f16a70950718d255194411c98079729e0ae9716c6bd0"}"#
                .to_vec()
        );
        assert_eq!(
            hex::encode(EdgeMeasurementDurabilityV1::sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn edge_measurement_raw_cutoff_snapshot_preserves_blink_queues_drops_and_witness() {
        let owner = EdgeMeasurementOwnerV1::new(owner_config()).unwrap();
        owner.ledger.record_observed().unwrap();
        owner.ledger.record_submission(0, SlotSubmit::Accepted).unwrap();
        owner.ledger.record_terminal(0, BlinkGenerationTerminalV1::Processed, None).unwrap();
        owner.pending_records.store(3, Ordering::Release);
        owner.pending_candidates.store(2, Ordering::Release);
        owner.candidate_sequence.store(3, Ordering::Release);
        owner.candidate_drop_counts
            [CandidatePreEnqueueDropReasonV3::CandidateQueueFull.counter_index()]
        .store(5, Ordering::Release);
        owner.blink_reject_queue_full.store(7, Ordering::Release);
        owner.blink_reject_queue_closed.store(11, Ordering::Release);
        owner.candidate_drop_queue_full.store(13, Ordering::Release);
        owner.candidate_drop_queue_closed.store(17, Ordering::Release);

        let periodic = owner.raw_accounting_snapshot().expect("raw periodic snapshot");
        assert_eq!(periodic.blink.victim_ingress_observed, 1);
        assert_eq!(periodic.blink.admitted_generations, 1);
        assert_eq!(periodic.blink.processed_terminal, 1);
        assert_eq!(
            periodic.queues,
            EdgeQueueAccountingSnapshotV1 {
                pending_records: 3,
                pending_candidates: 2,
                staged_count: 0,
            }
        );
        assert_eq!(
            periodic.candidate_bounds,
            CheckedCandidateBoundsV1 { count: 3, last_sequence: Some(2) }
        );
        assert_eq!(periodic.candidate_drops.candidate_queue_full, 5);
        assert_eq!(
            periodic.blink_reject_loss,
            BlinkRejectQueueLossCountersV1 { queue_full: 7, queue_closed: 11 }
        );
        assert_eq!(
            periodic.candidate_drop_loss,
            CandidateDropQueueLossCountersV1 { queue_full: 13, queue_closed: 17 }
        );
        assert_eq!(periodic.cutoff, None);

        owner.pending_records.store(0, Ordering::Release);
        owner.pending_candidates.store(0, Ordering::Release);
        let mut fields = cutoff_fields(10);
        fields.last_candidate_sequence = 2;
        owner.latch_cutoff(fields);
        let after = owner.cutoff_drained_snapshot().expect("cutoff-drained snapshot");
        assert!(after.cutoff.has_valid_record_hash());
        assert_eq!(after.revision[14], after.blink.generation_pending);
        assert_eq!(after.revision[15], after.blink.selected_pending);
        assert_eq!(after.revision[17], after.queues.pending_records);
        assert_eq!(after.revision[18], after.queues.pending_candidates);
        assert_eq!(after.revision[19], 0);
        assert_eq!(after.revision[20], after.candidate_bounds.count);
        assert_eq!(after.revision[23], 5);
        assert_eq!(after.revision[30], 11);
        assert_eq!(after.revision[31], 7);
        assert_eq!(after.revision[32], 17);
        assert_eq!(after.revision[33], 13);
        assert_eq!(after.revision[35], 1);
    }
    #[test]
    fn ordered_victim_witness_rejects_victim_anywhere_and_matches_prefix_digest() {
        let victim = B256::repeat_byte(0xaa);
        let first = B256::repeat_byte(0x11);
        let second = B256::repeat_byte(0x22);
        let mut visitor = OrderedTransactionReceiptVisitor::new(7, 2, victim).unwrap();
        visitor.visit_hash(0, first).unwrap();
        visitor.visit_hash(1, second).unwrap();

        let mut canonical = b"edge-ordered-transaction-cutoff/v1\0".to_vec();
        canonical.extend_from_slice(&7_u64.to_be_bytes());
        canonical.extend_from_slice(&2_u64.to_be_bytes());
        canonical.extend_from_slice(&0_u64.to_be_bytes());
        canonical.extend_from_slice(first.as_slice());
        canonical.extend_from_slice(&1_u64.to_be_bytes());
        canonical.extend_from_slice(second.as_slice());
        assert_eq!(
            B256::new(DefaultCrypto.sha256(&visitor.canonical)),
            B256::new(DefaultCrypto.sha256(&canonical)),
        );

        for victim_position in [0, 1] {
            let mut malicious = OrderedTransactionReceiptVisitor::new(7, 2, victim).unwrap();
            if victim_position == 1 {
                malicious.visit_hash(0, first).unwrap();
            }
            assert_eq!(malicious.visit_hash(victim_position, victim), Err(PortError::Incoherent),);
        }
        assert_eq!(2_u64.checked_add(1), Some(3));
    }
    #[test]
    fn edge_retained_authority_bytes_match_frozen_oracle() {
        let identities = [
            (Address::repeat_byte(1), B256::repeat_byte(11)),
            (Address::repeat_byte(2), B256::repeat_byte(12)),
            (Address::repeat_byte(3), B256::repeat_byte(13)),
            (Address::repeat_byte(4), B256::repeat_byte(14)),
        ];
        let mut identity_bytes = b"edge-deployment-identities/v1\0".to_vec();
        identity_bytes.extend_from_slice(&4_u32.to_be_bytes());
        for (address, hash) in identities {
            identity_bytes.extend_from_slice(address.as_slice());
            identity_bytes.extend_from_slice(hash.as_slice());
        }
        assert_eq!(
            EdgeMeasurementOwnerV1::deployment_identity_digest(&identities),
            B256::new(DefaultCrypto.sha256(&identity_bytes))
        );
        let mut changed_identities = identities;
        changed_identities[0].1 = B256::repeat_byte(15);
        assert_ne!(
            EdgeMeasurementOwnerV1::deployment_identity_digest(&identities),
            EdgeMeasurementOwnerV1::deployment_identity_digest(&changed_identities)
        );

        let route = [
            PreparedPoolState {
                pool: Address::repeat_byte(21),
                protocol: ExactProtocol::UniswapV2,
                token0: Address::repeat_byte(31),
                token1: Address::repeat_byte(32),
                decimals0: 18,
                decimals1: 6,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::ConstantProduct {
                    reserve0: U256::from(41),
                    reserve1: U256::from(42),
                },
            },
            PreparedPoolState {
                pool: Address::repeat_byte(22),
                protocol: ExactProtocol::AerodromeStable,
                token0: Address::repeat_byte(33),
                token1: Address::repeat_byte(34),
                decimals0: 8,
                decimals1: 9,
                fee_pips: 500,
                quote: PreparedPoolQuote::Stable {
                    reserve0: U256::from(43),
                    reserve1: U256::from(44),
                },
            },
        ];
        let mut route_bytes = b"edge-prepared-route/v1\0".to_vec();
        route_bytes.extend_from_slice(&2_u32.to_be_bytes());
        for (pool, quote_tag, reserve0, reserve1) in [
            (&route[0], 0_u8, U256::from(41), U256::from(42)),
            (&route[1], 1_u8, U256::from(43), U256::from(44)),
        ] {
            route_bytes.extend_from_slice(pool.pool.as_slice());
            route_bytes.push(pool.protocol as u8);
            route_bytes.extend_from_slice(pool.token0.as_slice());
            route_bytes.extend_from_slice(pool.token1.as_slice());
            route_bytes.push(pool.decimals0);
            route_bytes.push(pool.decimals1);
            route_bytes.extend_from_slice(&pool.fee_pips.to_be_bytes());
            route_bytes.push(quote_tag);
            route_bytes.extend_from_slice(&reserve0.to_be_bytes::<32>());
            route_bytes.extend_from_slice(&reserve1.to_be_bytes::<32>());
        }
        assert_eq!(
            EdgeMeasurementOwnerV1::prepared_route_digest(&route).unwrap(),
            B256::new(DefaultCrypto.sha256(&route_bytes))
        );
        let mut changed_route = route.clone();
        changed_route[1].quote = PreparedPoolQuote::V3 {
            sqrt_price_x96: U256::from(43),
            liquidity: U256::from(44),
            tick: -1,
            tick_spacing: 1,
            ticks: vec![crate::PairwiseV3Tick {
                tick: 0,
                liquidity_net: I256::try_from(-1).unwrap(),
            }],
        };
        assert_ne!(
            EdgeMeasurementOwnerV1::prepared_route_digest(&route).unwrap(),
            EdgeMeasurementOwnerV1::prepared_route_digest(&changed_route).unwrap()
        );

        let materialized = MaterializedState {
            writes: vec![MaterializedWrite {
                key: AuditedWriteKey::Storage {
                    address: Address::repeat_byte(51),
                    slot: U256::from(52),
                    evidence_digest: B256::repeat_byte(53),
                },
                value: U256::from(54),
            }],
        };
        let mut state_bytes = b"edge-materialized-state/v1\0".to_vec();
        state_bytes.extend_from_slice(&1_u32.to_be_bytes());
        state_bytes.push(2);
        state_bytes.extend_from_slice(Address::repeat_byte(51).as_slice());
        state_bytes.extend_from_slice(&U256::from(52).to_be_bytes::<32>());
        state_bytes.extend_from_slice(B256::repeat_byte(53).as_slice());
        state_bytes.extend_from_slice(&U256::from(54).to_be_bytes::<32>());
        assert_eq!(
            EdgeMeasurementOwnerV1::materialized_state_digest(&materialized).unwrap(),
            B256::new(DefaultCrypto.sha256(&state_bytes))
        );
        let mut changed_materialized = materialized.clone();
        changed_materialized.writes[0].value = U256::from(55);
        assert_ne!(
            EdgeMeasurementOwnerV1::materialized_state_digest(&materialized).unwrap(),
            EdgeMeasurementOwnerV1::materialized_state_digest(&changed_materialized).unwrap()
        );

        let token = Address::repeat_byte(61);
        let mut plan = BackrunPlan {
            parent_hash: B256::repeat_byte(62),
            block_number: 0,
            predecessor_index: 0,
            payload_id: PayloadId::new([0; 8]),
            victim: B256::repeat_byte(63),
            route: [
                BackrunHop {
                    pool: Address::repeat_byte(64),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: Address::repeat_byte(65),
                    protocol: ExactProtocol::AerodromeVolatile,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: 500,
                },
            ],
            amount_in: U256::from(100),
            amount_out: U256::from(110),
            gross_profit: U256::from(10),
            digest: BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan).unwrap();
        let hops = [
            MeasurementExecutionHopV1 {
                adapter: Address::repeat_byte(71),
                min_amount_out: U256::from(105),
                funding_target: Address::repeat_byte(72),
            },
            MeasurementExecutionHopV1 {
                adapter: Address::repeat_byte(73),
                min_amount_out: U256::from(110),
                funding_target: Address::repeat_byte(74),
            },
        ];
        let plan_bytes = MeasurementEncoder::encode(&plan).unwrap();
        let mut economics_bytes = b"edge-economics-evidence/v1\0".to_vec();
        economics_bytes.extend_from_slice(&u32::try_from(plan_bytes.len()).unwrap().to_be_bytes());
        economics_bytes.extend_from_slice(&plan_bytes);
        economics_bytes.extend_from_slice(&2_u32.to_be_bytes());
        for hop in hops {
            economics_bytes.extend_from_slice(hop.adapter.as_slice());
            economics_bytes.extend_from_slice(&hop.min_amount_out.to_be_bytes::<32>());
            economics_bytes.extend_from_slice(hop.funding_target.as_slice());
        }
        assert_eq!(
            EdgeMeasurementOwnerV1::economics_digest(&plan, &hops).unwrap(),
            B256::new(DefaultCrypto.sha256(&economics_bytes))
        );
        let mut changed_hops = hops;
        changed_hops[0].min_amount_out = U256::from(106);
        assert_ne!(
            EdgeMeasurementOwnerV1::economics_digest(&plan, &hops).unwrap(),
            EdgeMeasurementOwnerV1::economics_digest(&plan, &changed_hops).unwrap()
        );
        let mut changed_plan = plan.clone();
        changed_plan.route[0].fee_pips = 3_001;
        assert_ne!(
            EdgeMeasurementOwnerV1::economics_digest(&plan, &hops).unwrap(),
            EdgeMeasurementOwnerV1::economics_digest(&changed_plan, &hops).unwrap()
        );
    }
}
