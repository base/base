//! Feature-private Blink accounting, cutoff, candidate, and durability records.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use alloy_primitives::{B256, Bytes, hex, keccak256};
use revm::precompile::{Crypto, DefaultCrypto};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use thiserror::Error;
use tokio_tungstenite::tungstenite::error::{
    Error as WebSocketError, ProtocolError as WebSocketProtocolError,
};

use crate::{A1Outcome, A1Status, ShadowOutcome, SlotSubmit};

/// Fixed maximum number of simultaneously retained Blink generations.
pub const BLINK_LEDGER_CAPACITY: usize = 4_096;
/// Maximum retained victim bytes in one future-query-free candidate.
pub const EDGE_MAX_VICTIM_RAW_BYTES: usize = 131_072;

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

/// Atomic, bounded generation and selected-preterminal ledger.
#[derive(Debug)]
pub struct BlinkMeasurementLedgerV1 {
    observed: AtomicU64,
    accepted: AtomicU64,
    slot_accepted: AtomicU64,
    slot_replaced: AtomicU64,
    slot_closed: AtomicU64,
    generation_overflow: AtomicU64,
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
            poisoned: AtomicBool::new(false),
            generations: Mutex::new(BTreeMap::new()),
            selected: Mutex::new(BTreeMap::new()),
        }
    }
}

impl BlinkMeasurementLedgerV1 {
    /// Records one decoded victim presented to runtime admission.
    pub fn record_observed(&self) -> Result<(), EdgeMeasurementError> {
        Self::increment(&self.observed)
    }

    /// Records generation allocation overflow after runtime accepted the decoded victim.
    pub fn record_generation_overflow(&self) -> Result<(), EdgeMeasurementError> {
        Self::increment(&self.accepted)?;
        Self::increment(&self.generation_overflow)
    }

    /// Records lifecycle closure before a generation could be allocated.
    pub fn record_slot_closed(&self) -> Result<(), EdgeMeasurementError> {
        Self::increment(&self.accepted)?;
        Self::increment(&self.slot_closed)
    }

    /// Records the exact capacity-one slot result for a checked generation.
    pub fn record_submission(
        &self,
        generation: u64,
        result: SlotSubmit,
    ) -> Result<(), EdgeMeasurementError> {
        Self::increment(&self.accepted)?;
        match result {
            SlotSubmit::Closed => Self::increment(&self.slot_closed),
            SlotSubmit::Accepted | SlotSubmit::Replaced => {
                let mut generations = self
                    .generations
                    .lock()
                    .map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
                if generations.len() >= BLINK_LEDGER_CAPACITY {
                    return Err(self.poison(EdgeMeasurementError::LedgerCapacityOverflow));
                }
                if generations.insert(generation, None).is_some() {
                    return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                }
                if result == SlotSubmit::Replaced {
                    let replaced = generations.iter_mut().rev().find(|(candidate, terminal)| {
                        **candidate < generation && terminal.is_none()
                    });
                    let Some((_, terminal)) = replaced else {
                        return Err(self.poison(EdgeMeasurementError::GenerationConflict));
                    };
                    *terminal = Some(BlinkGenerationTerminalV1::ReplacedBeforeFrame);
                    Self::increment(&self.slot_replaced)
                } else {
                    Self::increment(&self.slot_accepted)
                }
            }
        }
    }

    /// Records construction of one selected, future-query-free DTO before terminal commit.
    pub fn record_selected_preterminal(&self, generation: u64) -> Result<(), EdgeMeasurementError> {
        let mut selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        if selected.insert(generation, None).is_some() {
            return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
        }
        Ok(())
    }

    /// Terminalizes one admitted generation and its selected preterminal, when present.
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
        drop(generations);

        let mut selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        if let Some(current) = selected.get_mut(&generation) {
            let selected_terminal = match shadow {
                Some(ShadowOutcome::Selected) => SelectedDtoTerminalV1::Committed,
                Some(ShadowOutcome::Cancelled) => SelectedDtoTerminalV1::CancelledBeforeTerminal,
                _ => SelectedDtoTerminalV1::PreterminalFailure,
            };
            if current.replace(selected_terminal).is_some() {
                return Err(self.poison(EdgeMeasurementError::SelectedDtoConflict));
            }
        }
        Ok(())
    }

    /// Terminalizes every queued generation except an optional active generation at shutdown.
    pub fn terminalize_shutdown_pending(
        &self,
        active_generation: Option<u64>,
    ) -> Result<(), EdgeMeasurementError> {
        let mut generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        for (generation, terminal) in &mut *generations {
            if terminal.is_none() && Some(*generation) != active_generation {
                *terminal = Some(BlinkGenerationTerminalV1::CancelledBeforeFrame);
            }
        }
        Ok(())
    }

    /// Returns exact counters and pending cardinalities for conservation checks.
    pub fn snapshot(&self) -> Result<BlinkLedgerSnapshotV1, EdgeMeasurementError> {
        let generations =
            self.generations.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let selected =
            self.selected.lock().map_err(|_| self.poison(EdgeMeasurementError::LockPoisoned))?;
        let count_generation = |terminal| {
            generations.values().filter(|value| **value == Some(terminal)).count() as u64
        };
        let count_selected =
            |terminal| selected.values().filter(|value| **value == Some(terminal)).count() as u64;
        let processed_terminal = count_generation(BlinkGenerationTerminalV1::Processed)
            + count_generation(BlinkGenerationTerminalV1::Cancelled)
            + count_generation(BlinkGenerationTerminalV1::InternalFailure);
        Ok(BlinkLedgerSnapshotV1 {
            victim_ingress_observed: self.observed.load(Ordering::SeqCst),
            victim_ingress_accepted: self.accepted.load(Ordering::SeqCst),
            slot_accepted: self.slot_accepted.load(Ordering::SeqCst),
            slot_replaced: self.slot_replaced.load(Ordering::SeqCst),
            slot_closed: self.slot_closed.load(Ordering::SeqCst),
            generation_overflow: self.generation_overflow.load(Ordering::SeqCst),
            admitted_generations: generations.len() as u64,
            processed_terminal,
            replaced_before_frame: count_generation(BlinkGenerationTerminalV1::ReplacedBeforeFrame),
            cancelled_before_frame: count_generation(
                BlinkGenerationTerminalV1::CancelledBeforeFrame,
            ),
            selected_dto_built_preterminal: selected.len() as u64,
            selected_dto_committed: count_selected(SelectedDtoTerminalV1::Committed),
            selected_dto_cancelled_before_terminal: count_selected(
                SelectedDtoTerminalV1::CancelledBeforeTerminal,
            ),
            selected_dto_preterminal_failure: count_selected(
                SelectedDtoTerminalV1::PreterminalFailure,
            ),
            generation_pending: generations.values().filter(|terminal| terminal.is_none()).count()
                as u64,
            selected_pending: selected.values().filter(|terminal| terminal.is_none()).count()
                as u64,
            poisoned: self.poisoned.load(Ordering::SeqCst),
        })
    }

    /// Verifies both exact conservation equations and zero final pending sets.
    pub fn verify_final(&self) -> Result<BlinkLedgerSnapshotV1, EdgeMeasurementError> {
        let snapshot = self.snapshot()?;
        let ingress_rhs = snapshot
            .slot_accepted
            .checked_add(snapshot.slot_replaced)
            .and_then(|value| value.checked_add(snapshot.slot_closed))
            .and_then(|value| value.checked_add(snapshot.generation_overflow));
        let generation_rhs = snapshot
            .processed_terminal
            .checked_add(snapshot.replaced_before_frame)
            .and_then(|value| value.checked_add(snapshot.cancelled_before_frame));
        let selected_rhs = snapshot
            .selected_dto_committed
            .checked_add(snapshot.selected_dto_cancelled_before_terminal)
            .and_then(|value| value.checked_add(snapshot.selected_dto_preterminal_failure));
        if snapshot.poisoned
            || snapshot.generation_pending != 0
            || snapshot.selected_pending != 0
            || ingress_rhs != Some(snapshot.victim_ingress_observed)
            || snapshot.admitted_generations != snapshot.slot_accepted + snapshot.slot_replaced
            || generation_rhs != Some(snapshot.admitted_generations)
            || selected_rhs != Some(snapshot.selected_dto_built_preterminal)
        {
            return Err(EdgeMeasurementError::FinalNotClosed);
        }
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
        self.value.lock().map(|value| value.clone()).map_err(|_| EdgeMeasurementError::LockPoisoned)
    }
}

/// Bounded future-query-free candidate measurement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeCandidateV3 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Blink source generation.
    pub source_generation: u64,
    /// Candidate generation, equal to source generation.
    pub candidate_generation: u64,
    /// Coverage generation, equal to source generation.
    pub coverage_generation: u64,
    /// Pending snapshot sequence joined through the side registry.
    pub pending_snapshot_sequence: u64,
    /// Candidate block.
    pub block_number: u64,
    /// Payload identifier.
    pub payload_id: [u8; 8],
    /// Predecessor flashblock index.
    pub predecessor_index: u64,
    /// Victim hash.
    pub victim_hash: B256,
    /// Exact bounded victim envelope.
    pub victim_raw: Bytes,
    /// Selected plan digest.
    pub selected_plan_digest: B256,
    /// Structural terminal hash.
    pub structural_terminal_hash: [u8; 32],
    /// Connection coverage receipt hash.
    pub connection_coverage_receipt_hash: [u8; 32],
    /// Registry terminal receipt hash.
    pub registry_terminal_receipt_hash: [u8; 32],
    /// Cutoff record hash.
    pub cutoff_record_hash: [u8; 32],
    /// Resolved unsigned measurement transaction.
    pub backrun_measurement_tx: crate::BackrunMeasurementTxV1,
}

impl EdgeCandidateV3 {
    /// Validates boundedness, same-generation lineage, and victim-envelope binding.
    pub fn validate(&self) -> Result<(), EdgeMeasurementError> {
        if self.victim_raw.len() > EDGE_MAX_VICTIM_RAW_BYTES
            || self.source_generation != self.candidate_generation
            || self.source_generation != self.coverage_generation
            || keccak256(&self.victim_raw) != self.victim_hash
            || self.backrun_measurement_tx.target_tx_hash != self.victim_hash
        {
            return Err(EdgeMeasurementError::GenerationConflict);
        }
        Ok(())
    }
}

/// Complete final record persisted after all Blink domains drain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgeMeasurementFinalV1 {
    /// Frozen Blink conservation snapshot.
    pub blink: BlinkLedgerSnapshotV1,
    /// Exactly-once cutoff record.
    pub cutoff: ProducerEpochCutoffV1,
}

/// Crash-safe final writer using temp-file fsync, rename, and directory fsync.
#[derive(Debug, Default, Clone, Copy)]
pub struct EdgeMeasurementDurabilityV1;

impl EdgeMeasurementDurabilityV1 {
    /// Persists independent Blink final, manifest, and checkpoint records.
    pub fn persist(
        directory: &Path,
        final_record: &EdgeMeasurementFinalV1,
    ) -> io::Result<[PathBuf; 3]> {
        fs::create_dir_all(directory)?;
        let final_bytes = serde_json::to_vec(final_record).map_err(io::Error::other)?;
        let final_hash = DefaultCrypto.sha256(&final_bytes);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": "base-edge-measurement-manifest-v1",
            "blinkFinalSha256": hex::encode(final_hash),
            "pending": final_record.blink.generation_pending + final_record.blink.selected_pending,
        }))
        .map_err(io::Error::other)?;
        let manifest_hash = DefaultCrypto.sha256(&manifest);
        let checkpoint = serde_json::to_vec(&serde_json::json!({
            "schema": "base-edge-measurement-checkpoint-v1",
            "manifestSha256": hex::encode(manifest_hash),
            "cutoffRecordHash": hex::encode(final_record.cutoff.record_hash),
        }))
        .map_err(io::Error::other)?;
        let paths = [
            directory.join("blink-final-v1.json"),
            directory.join("producer-manifest-v1.json"),
            directory.join("producer-checkpoint-v1.json"),
        ];
        for (path, bytes) in paths.iter().zip([final_bytes, manifest, checkpoint]) {
            Self::persist_one(path, &bytes)?;
        }
        File::open(directory)?.sync_all()?;
        Ok(paths)
    }

    fn persist_one(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let temporary = path.with_extension("tmp");
        let mut file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn reject_schema_preserves_exact_http_and_transport_semantics() {
        let switching = BlinkRejectClassifierV3::classify_http_status(101);
        assert_eq!(switching.reason, BlinkRejectReasonV3::Http101);
        assert_eq!(switching.status, A1Status::AwaitingAck);
        assert_eq!(switching.outcome, None);
        assert!(!switching.retry);

        for status in [408, 429, 500, 599] {
            let retry = BlinkRejectClassifierV3::classify_http_status(status);
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
    fn latest_wins_and_selected_ledgers_conserve() {
        let ledger = BlinkMeasurementLedgerV1::default();
        ledger.record_observed().unwrap();
        ledger.record_submission(1, SlotSubmit::Accepted).unwrap();
        ledger.record_observed().unwrap();
        ledger.record_submission(2, SlotSubmit::Replaced).unwrap();
        ledger.record_selected_preterminal(2).unwrap();
        ledger
            .record_terminal(2, BlinkGenerationTerminalV1::Processed, Some(ShadowOutcome::Selected))
            .unwrap();
        let final_snapshot = ledger.verify_final().unwrap();
        assert_eq!(final_snapshot.admitted_generations, 2);
        assert_eq!(final_snapshot.replaced_before_frame, 1);
        assert_eq!(final_snapshot.selected_dto_committed, 1);
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

    #[test]
    fn cutoff_hash_matches_shared_ts_contract_and_final_is_durable() {
        let ledger = BlinkMeasurementLedgerV1::default();
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
        let root = std::env::temp_dir().join(format!(
            "base-edge-measurement-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let final_record = EdgeMeasurementFinalV1 { blink: ledger.verify_final().unwrap(), cutoff };
        let paths = EdgeMeasurementDurabilityV1::persist(&root, &final_record).unwrap();
        assert!(paths.iter().all(|path| path.is_file()));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert_eq!(persisted["cutoff"]["producerEpoch"], "1");
        assert_eq!(
            persisted["cutoff"]["recordHash"],
            "ffdf47790035f3460cd4f16a70950718d255194411c98079729e0ae9716c6bd0"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
