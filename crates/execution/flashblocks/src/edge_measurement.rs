//! Feature-private edge measurement state for the flashblocks producer.

use std::{
    collections::{BTreeMap, HashMap, VecDeque, hash_map::Entry},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use base_common_flashblocks::Flashblock;
use revm::precompile::{Crypto as _, DefaultCrypto};

use crate::PendingBlocks;

/// Standard SHA-256 encoder for the fixed-width S2 pending public subset contract.
#[derive(Debug, Clone, Copy)]
pub struct PendingPublicSubsetHasherV1;

impl PendingPublicSubsetHasherV1 {
    /// Hashes the exact S2 byte layout using ordinary SHA-256.
    pub fn digest(
        earliest_block_number: u64,
        latest_block_number: u64,
        payload_id: [u8; 8],
        latest_flashblock_index: u64,
        parent_hash: B256,
        latest_sealed_header_hash: B256,
    ) -> B256 {
        const DOMAIN: &[u8] = b"base-edge-pending-public-subset-v1\0";
        const ENCODED_LEN: usize = DOMAIN.len() + 8 + 8 + 8 + 8 + 32 + 32;

        let mut bytes = Vec::with_capacity(ENCODED_LEN);
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&earliest_block_number.to_be_bytes());
        bytes.extend_from_slice(&latest_block_number.to_be_bytes());
        bytes.extend_from_slice(&payload_id);
        bytes.extend_from_slice(&latest_flashblock_index.to_be_bytes());
        bytes.extend_from_slice(parent_hash.as_slice());
        bytes.extend_from_slice(latest_sealed_header_hash.as_slice());
        debug_assert_eq!(bytes.len(), ENCODED_LEN);
        B256::from(DefaultCrypto.sha256(&bytes))
    }
}

/// The fixed maximum number of registered pending snapshots retained at once.
pub const PENDING_REGISTRY_CAPACITY_V2: usize = 4_096;

/// The process-local identity assigned to one pending snapshot publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSnapshotIdentityV2 {
    /// The producer epoch containing the snapshot.
    pub producer_epoch: u64,
    /// The checked, unique publication sequence within the epoch.
    pub pending_snapshot_sequence: u64,
    /// The process-local `Arc` allocation address.
    pub arc_pointer_identity: usize,
}

/// Immutable metadata joined to a pending snapshot publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSnapshotMetadataV2 {
    /// The process-local snapshot identity.
    pub identity: PendingSnapshotIdentityV2,
    /// The source generation that produced the snapshot, when available.
    pub source_generation: Option<u64>,
    /// The six-field public subset corruption-check digest.
    pub pending_public_subset_digest_v1: B256,
}

/// Named reasons why registration could not establish an authoritative identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistrationFailure {
    /// The checked snapshot sequence overflowed.
    PendingSnapshotSequenceOverflow,
    /// The owner-approved registry capacity was exceeded.
    PendingRegistryCapacityOverflow,
    /// The registry mutex was poisoned by an earlier panic.
    PendingRegistryLockPoisoned,
    /// A pointer was still bound to an earlier live sequence.
    PendingPointerBindingConflict,
}

/// Named reasons for a CLI `Ok` lookup terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliRegistryLookupFailureReason {
    /// No published sequence was waiting for the CLI.
    NoPublishedSequence,
    /// Registration failed before the unchanged production send.
    RegistrationFailed(PendingRegistrationFailure),
    /// The primary sequence entry was absent.
    MissingPrimaryEntry,
    /// The expected sequence did not match the pointer index head.
    PendingPointerBindingConflict,
    /// The retained weak identity expired before the CLI terminal.
    PendingArcIdentityExpired,
    /// The delivered `Arc` did not match the retained allocation.
    PendingArcIdentityMismatch,
    /// The immutable public subset changed between publication and receipt.
    PendingPublicSubsetCorruption,
}

/// Exact named CLI lookup-failure terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliRegistryLookupFailed {
    /// The sequence expected by the ordered publication journal, if one existed.
    pub pending_snapshot_sequence: Option<u64>,
    /// The exact lookup failure reason.
    pub reason: CliRegistryLookupFailureReason,
}

/// Errors while terminalizing a published range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistryError {
    /// A range length could not be represented on this platform.
    RangeLengthOverflow,
    /// The published journal contained fewer entries than the exact terminal range.
    PublishedRangeMismatch,
    /// A sequence in the published journal had no primary entry.
    MissingPrimaryEntry,
    /// The registry mutex was poisoned by an earlier panic.
    LockPoisoned,
}

/// Registration state retained across the unchanged production send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistrationDispositionV2 {
    /// The sequence, pointer, and weak identity were registered.
    Succeeded,
    /// Registration failed with an exact reason.
    Failed(PendingRegistrationFailure),
}

/// Production broadcast disposition for a registered attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSendDispositionV2 {
    /// The send succeeded and reported the existing receiver count.
    Published {
        /// The positive receiver count returned by the existing broadcast send.
        receiver_count: usize,
    },
    /// The existing broadcast sender reported no receivers.
    NoReceivers,
}

/// Terminal delivery disposition for one publication sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCliTerminalV2 {
    /// CLI received the `Arc` and the metadata lookup succeeded.
    CliReceivedLookupSucceeded,
    /// CLI received the `Arc`, but metadata lookup failed exactly.
    CliRegistryLookupFailed(CliRegistryLookupFailureReason),
    /// Tokio attributed the sequence to an exact lagged range.
    CliLagged,
    /// Channel closure attributed the sequence to the exact remaining range.
    CliClosed,
    /// Shutdown cancellation attributed the sequence to the exact remaining range.
    CliCancelled,
    /// The producer send had no receivers and needed no CLI terminal.
    NoReceivers,
}

/// A registration token held by the processor until the existing send returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRegistrationAttemptV2 {
    /// The allocated sequence, absent only after sequence overflow.
    pub pending_snapshot_sequence: Option<u64>,
    /// The registration disposition terminalized before send.
    pub disposition: PendingRegistrationDispositionV2,
}

/// A terminal record retained after the weak identity is durably acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingTerminalRecordV2 {
    /// Immutable snapshot metadata.
    pub metadata: PendingSnapshotMetadataV2,
    /// Registration disposition.
    pub registration: PendingRegistrationDispositionV2,
    /// Production send disposition.
    pub send: PendingSendDispositionV2,
    /// Exact delivery terminal.
    pub terminal: PendingCliTerminalV2,
}

/// One live primary registry entry.
#[derive(Debug)]
pub struct PendingRegistryEntryV2 {
    /// Immutable snapshot metadata.
    pub metadata: PendingSnapshotMetadataV2,
    /// Retained weak identity preventing allocation-address reuse before cleanup.
    pub retained_weak: Weak<PendingBlocks>,
    /// Registration disposition.
    pub registration: PendingRegistrationDispositionV2,
    /// Send disposition once the unchanged send returns.
    pub send: Option<PendingSendDispositionV2>,
}

/// Executable counters for the H2 publication product.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingRegistryCountersV2 {
    /// Advanced snapshots that attempted registration.
    pub advanced_with_snapshot: u64,
    /// Successful registrations.
    pub registration_succeeded: u64,
    /// Failed registrations.
    pub registration_failed: u64,
    /// Sends that reported at least one receiver.
    pub send_published: u64,
    /// Sends that reported no receivers.
    pub send_no_receivers: u64,
    /// CLI `Ok` receipts whose lookup succeeded.
    pub cli_received_lookup_succeeded: u64,
    /// CLI `Ok` receipts whose lookup failed.
    pub cli_registry_lookup_failed: u64,
    /// Sequences attributed to lag.
    pub cli_lagged_attributed: u64,
    /// Sequences attributed to closure.
    pub cli_closed_attributed: u64,
    /// Sequences attributed to cancellation.
    pub cli_cancelled_attributed: u64,
}

/// Mutable registry state protected by one non-async critical section.
#[derive(Debug)]
pub struct PendingRegistryStateV2 {
    /// The next checked sequence.
    pub next_sequence: u64,
    /// Primary entries keyed by sequence within the fixed producer epoch.
    pub primary: BTreeMap<u64, PendingRegistryEntryV2>,
    /// Pointer-to-sequence FIFO index for successful registrations.
    pub secondary: HashMap<usize, VecDeque<u64>>,
    /// Ordered successful production sends waiting for CLI attribution.
    pub published: VecDeque<u64>,
    /// Terminal records whose append is the durability acknowledgement.
    pub terminal_records: Vec<PendingTerminalRecordV2>,
    /// H2 product counters.
    pub counters: PendingRegistryCountersV2,
    /// Whether any exact accounting or identity poison was observed.
    pub poisoned: bool,
}

/// Feature-private pending metadata registry shared by processor and sole trader CLI.
#[derive(Debug)]
pub struct PendingMetadataRegistryV2 {
    producer_epoch: u64,
    capacity: usize,
    state: Mutex<PendingRegistryStateV2>,
}

impl PendingMetadataRegistryV2 {
    /// Creates a registry for a fixed producer epoch and owner-approved capacity.
    pub fn new(producer_epoch: u64, capacity: usize) -> Self {
        Self {
            producer_epoch,
            capacity,
            state: Mutex::new(PendingRegistryStateV2 {
                next_sequence: 0,
                primary: BTreeMap::new(),
                secondary: HashMap::new(),
                published: VecDeque::new(),
                terminal_records: Vec::new(),
                counters: PendingRegistryCountersV2::default(),
                poisoned: false,
            }),
        }
    }

    /// Returns the immutable producer epoch.
    pub const fn producer_epoch(&self) -> u64 {
        self.producer_epoch
    }

    /// Computes the exact six-field O(1) public subset digest.
    pub fn pending_public_subset_digest_v1(pending: &PendingBlocks) -> B256 {
        PendingPublicSubsetHasherV1::digest(
            pending.earliest_block_number(),
            pending.latest_block_number(),
            pending.payload_id().0.into(),
            pending.latest_flashblock_index(),
            pending.parent_hash(),
            pending.latest_header().hash(),
        )
    }

    /// Registers one advanced snapshot before the unchanged production send.
    pub fn register(
        &self,
        pending: &Arc<PendingBlocks>,
        source_generation: Option<u64>,
    ) -> PendingRegistrationAttemptV2 {
        let (mut state, was_poisoned) = self.lock_state();
        state.counters.advanced_with_snapshot =
            state.counters.advanced_with_snapshot.checked_add(1).unwrap_or_else(|| {
                state.poisoned = true;
                u64::MAX
            });

        let Some(next_sequence) = state.next_sequence.checked_add(1) else {
            state.poisoned = true;
            state.counters.registration_failed =
                state.counters.registration_failed.saturating_add(1);
            return PendingRegistrationAttemptV2 {
                pending_snapshot_sequence: None,
                disposition: PendingRegistrationDispositionV2::Failed(
                    PendingRegistrationFailure::PendingSnapshotSequenceOverflow,
                ),
            };
        };
        let sequence = state.next_sequence;
        state.next_sequence = next_sequence;
        let pointer = Arc::as_ptr(pending) as usize;
        let digest = Self::pending_public_subset_digest_v1(pending);
        let metadata = PendingSnapshotMetadataV2 {
            identity: PendingSnapshotIdentityV2 {
                producer_epoch: self.producer_epoch,
                pending_snapshot_sequence: sequence,
                arc_pointer_identity: pointer,
            },
            source_generation,
            pending_public_subset_digest_v1: digest,
        };

        let failure = if was_poisoned {
            Some(PendingRegistrationFailure::PendingRegistryLockPoisoned)
        } else if state.primary.len() >= self.capacity {
            Some(PendingRegistrationFailure::PendingRegistryCapacityOverflow)
        } else if state.secondary.get(&pointer).is_some_and(|queue| !queue.is_empty()) {
            Some(PendingRegistrationFailure::PendingPointerBindingConflict)
        } else {
            None
        };
        let disposition = failure.map_or(
            PendingRegistrationDispositionV2::Succeeded,
            PendingRegistrationDispositionV2::Failed,
        );
        if failure.is_some() {
            state.poisoned = true;
            state.counters.registration_failed =
                state.counters.registration_failed.saturating_add(1);
        } else {
            state.counters.registration_succeeded =
                state.counters.registration_succeeded.saturating_add(1);
            state.secondary.entry(pointer).or_default().push_back(sequence);
        }
        state.primary.insert(
            sequence,
            PendingRegistryEntryV2 {
                metadata,
                retained_weak: Arc::downgrade(pending),
                registration: disposition,
                send: None,
            },
        );
        PendingRegistrationAttemptV2 { pending_snapshot_sequence: Some(sequence), disposition }
    }

    /// Records the result of the exactly-once existing broadcast send.
    pub fn record_send(
        &self,
        attempt: PendingRegistrationAttemptV2,
        receiver_count: Option<usize>,
    ) -> Result<(), PendingRegistryError> {
        let Some(sequence) = attempt.pending_snapshot_sequence else {
            return Ok(());
        };
        let mut state = self.lock_state_checked()?;
        let send = receiver_count.map_or(PendingSendDispositionV2::NoReceivers, |receiver_count| {
            PendingSendDispositionV2::Published { receiver_count }
        });
        let Some(entry) = state.primary.get_mut(&sequence) else {
            state.poisoned = true;
            return Err(PendingRegistryError::MissingPrimaryEntry);
        };
        entry.send = Some(send);
        match send {
            PendingSendDispositionV2::Published { .. } => {
                state.counters.send_published = state.counters.send_published.saturating_add(1);
                state.published.push_back(sequence);
            }
            PendingSendDispositionV2::NoReceivers => {
                state.counters.send_no_receivers =
                    state.counters.send_no_receivers.saturating_add(1);
                self.terminalize_locked(&mut state, sequence, PendingCliTerminalV2::NoReceivers)?;
            }
        }
        Ok(())
    }

    /// Looks up and durably acknowledges one CLI `Ok` delivery.
    pub fn cli_received(
        &self,
        pending: &Arc<PendingBlocks>,
    ) -> Result<PendingSnapshotMetadataV2, CliRegistryLookupFailed> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisoned = true;
                return Err(CliRegistryLookupFailed {
                    pending_snapshot_sequence: state.published.front().copied(),
                    reason: CliRegistryLookupFailureReason::NoPublishedSequence,
                });
            }
        };
        let Some(sequence) = state.published.pop_front() else {
            state.poisoned = true;
            return Err(CliRegistryLookupFailed {
                pending_snapshot_sequence: None,
                reason: CliRegistryLookupFailureReason::NoPublishedSequence,
            });
        };
        let pointer = Arc::as_ptr(pending) as usize;
        let reason = state.primary.get(&sequence).map_or(
            Some(CliRegistryLookupFailureReason::MissingPrimaryEntry),
            |entry| match entry.registration {
                PendingRegistrationDispositionV2::Failed(reason) => {
                    Some(CliRegistryLookupFailureReason::RegistrationFailed(reason))
                }
                PendingRegistrationDispositionV2::Succeeded => {
                    let index_matches = state
                        .secondary
                        .get(&pointer)
                        .and_then(|queue| queue.front())
                        .is_some_and(|indexed| *indexed == sequence);
                    if !index_matches {
                        Some(CliRegistryLookupFailureReason::PendingPointerBindingConflict)
                    } else if entry.retained_weak.upgrade().is_none() {
                        Some(CliRegistryLookupFailureReason::PendingArcIdentityExpired)
                    } else if entry
                        .retained_weak
                        .upgrade()
                        .is_some_and(|retained| !Arc::ptr_eq(&retained, pending))
                    {
                        Some(CliRegistryLookupFailureReason::PendingArcIdentityMismatch)
                    } else if entry.metadata.pending_public_subset_digest_v1
                        != Self::pending_public_subset_digest_v1(pending)
                    {
                        Some(CliRegistryLookupFailureReason::PendingPublicSubsetCorruption)
                    } else {
                        None
                    }
                }
            },
        );

        if let Some(reason) = reason {
            state.poisoned = true;
            state.counters.cli_registry_lookup_failed =
                state.counters.cli_registry_lookup_failed.saturating_add(1);
            let _ = self.terminalize_locked(
                &mut state,
                sequence,
                PendingCliTerminalV2::CliRegistryLookupFailed(reason),
            );
            return Err(CliRegistryLookupFailed {
                pending_snapshot_sequence: Some(sequence),
                reason,
            });
        }

        let metadata = state.primary.get(&sequence).map(|entry| entry.metadata).ok_or(
            CliRegistryLookupFailed {
                pending_snapshot_sequence: Some(sequence),
                reason: CliRegistryLookupFailureReason::MissingPrimaryEntry,
            },
        )?;
        state.counters.cli_received_lookup_succeeded =
            state.counters.cli_received_lookup_succeeded.saturating_add(1);
        let _ = self.terminalize_locked(
            &mut state,
            sequence,
            PendingCliTerminalV2::CliReceivedLookupSucceeded,
        );
        Ok(metadata)
    }

    /// Attributes exactly `count` published sequences to a Tokio lag terminal.
    pub fn cli_lagged(&self, count: u64) -> Result<(), PendingRegistryError> {
        let count =
            usize::try_from(count).map_err(|_| PendingRegistryError::RangeLengthOverflow)?;
        let mut state = self.lock_state_checked()?;
        self.terminalize_range_locked(&mut state, count, PendingCliTerminalV2::CliLagged)?;
        state.counters.cli_lagged_attributed =
            state.counters.cli_lagged_attributed.saturating_add(count as u64);
        Ok(())
    }

    /// Attributes the exact remaining published range to channel closure.
    pub fn cli_closed(&self) -> Result<(), PendingRegistryError> {
        let mut state = self.lock_state_checked()?;
        let count = state.published.len();
        self.terminalize_range_locked(&mut state, count, PendingCliTerminalV2::CliClosed)?;
        state.counters.cli_closed_attributed =
            state.counters.cli_closed_attributed.saturating_add(count as u64);
        Ok(())
    }

    /// Attributes the exact remaining published range to shutdown cancellation.
    pub fn cli_cancelled(&self) -> Result<(), PendingRegistryError> {
        let mut state = self.lock_state_checked()?;
        let count = state.published.len();
        self.terminalize_range_locked(&mut state, count, PendingCliTerminalV2::CliCancelled)?;
        state.counters.cli_cancelled_attributed =
            state.counters.cli_cancelled_attributed.saturating_add(count as u64);
        Ok(())
    }

    /// Returns executable product counters and live pending counts.
    pub fn snapshot(&self) -> PendingRegistrySnapshotV2 {
        let (state, was_poisoned) = self.lock_state();
        PendingRegistrySnapshotV2 {
            counters: state.counters,
            primary_pending: state.primary.len(),
            secondary_pending: state.secondary.values().map(VecDeque::len).sum(),
            published_pending: state.published.len(),
            terminal_records: state.terminal_records.len(),
            poisoned: state.poisoned || was_poisoned,
        }
    }

    /// Returns a copy of all terminal records in durable append order.
    pub fn terminal_records(&self) -> Vec<PendingTerminalRecordV2> {
        self.lock_state().0.terminal_records.clone()
    }

    /// Locks state while preserving poison as measurement data.
    pub fn lock_state(&self) -> (MutexGuard<'_, PendingRegistryStateV2>, bool) {
        match self.state.lock() {
            Ok(state) => (state, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        }
    }

    /// Locks state for a fallible public terminal operation.
    pub fn lock_state_checked(
        &self,
    ) -> Result<MutexGuard<'_, PendingRegistryStateV2>, PendingRegistryError> {
        self.state.lock().map_err(|_| PendingRegistryError::LockPoisoned)
    }

    /// Terminalizes one sequence, appends its record, then drops pointer/weak state.
    pub fn terminalize_locked(
        &self,
        state: &mut PendingRegistryStateV2,
        sequence: u64,
        terminal: PendingCliTerminalV2,
    ) -> Result<(), PendingRegistryError> {
        let Some(entry) = state.primary.remove(&sequence) else {
            state.poisoned = true;
            return Err(PendingRegistryError::MissingPrimaryEntry);
        };
        let Some(send) = entry.send else {
            state.poisoned = true;
            return Err(PendingRegistryError::MissingPrimaryEntry);
        };
        state.terminal_records.push(PendingTerminalRecordV2 {
            metadata: entry.metadata,
            registration: entry.registration,
            send,
            terminal,
        });
        if matches!(entry.registration, PendingRegistrationDispositionV2::Succeeded) {
            let pointer = entry.metadata.identity.arc_pointer_identity;
            if let Some(queue) = state.secondary.get_mut(&pointer) {
                if queue.front().copied() == Some(sequence) {
                    queue.pop_front();
                } else {
                    state.poisoned = true;
                }
                if queue.is_empty() {
                    state.secondary.remove(&pointer);
                }
            } else {
                state.poisoned = true;
            }
        }
        drop(entry.retained_weak);
        Ok(())
    }

    /// Terminalizes an exact prefix of the published journal without partial success.
    pub fn terminalize_range_locked(
        &self,
        state: &mut PendingRegistryStateV2,
        count: usize,
        terminal: PendingCliTerminalV2,
    ) -> Result<(), PendingRegistryError> {
        if state.published.len() < count {
            state.poisoned = true;
            return Err(PendingRegistryError::PublishedRangeMismatch);
        }
        let sequences: Vec<u64> = state.published.iter().take(count).copied().collect();
        if sequences.iter().any(|sequence| !state.primary.contains_key(sequence)) {
            state.poisoned = true;
            return Err(PendingRegistryError::MissingPrimaryEntry);
        }
        for sequence in sequences {
            state.published.pop_front();
            self.terminalize_locked(state, sequence, terminal)?;
        }
        Ok(())
    }
}

/// Read-only registry snapshot used by cutoff and focused conservation tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRegistrySnapshotV2 {
    /// H2 counters.
    pub counters: PendingRegistryCountersV2,
    /// Live primary entry count.
    pub primary_pending: usize,
    /// Live secondary sequence count.
    pub secondary_pending: usize,
    /// Published journal entries awaiting a CLI terminal.
    pub published_pending: usize,
    /// Durably acknowledged terminal records.
    pub terminal_records: usize,
    /// Whether the measurement epoch is poisoned.
    pub poisoned: bool,
}

/// Payload-first immutable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PayloadFirstKeyV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Payload block number.
    pub block_number: u64,
    /// Eight-byte engine payload identifier.
    pub payload_id: [u8; 8],
}

/// One predecode clock and wire observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireObservationV1 {
    /// Shared checked observation ordinal.
    pub clock_observation_ordinal: u64,
    /// Realtime nanoseconds since the Unix epoch, when available.
    pub realtime_ns: Option<u128>,
    /// Monotonic nanoseconds since recorder start.
    pub monotonic_ns: u128,
    /// SHA-256 of the exact yielded websocket bytes.
    pub wire_digest: B256,
}

/// Immutable index-zero payload-first binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadFirstObservationV1 {
    /// Payload-first key.
    pub key: PayloadFirstKeyV1,
    /// Source generation for the decoded index-zero frame.
    pub source_generation: u64,
    /// Predecode observation.
    pub observation: WireObservationV1,
}

/// Structural key joining decoded source generations to processor admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecodedFlashblockKeyV1 {
    /// Payload block number.
    pub block_number: u64,
    /// Eight-byte engine payload identifier.
    pub payload_id: [u8; 8],
    /// Flashblock index within the payload.
    pub flashblock_index: u64,
}

impl DecodedFlashblockKeyV1 {
    /// Extracts the stable structural key from a decoded flashblock.
    pub fn from_flashblock(flashblock: &Flashblock) -> Self {
        Self {
            block_number: flashblock.metadata.block_number,
            payload_id: flashblock.payload_id.0.into(),
            flashblock_index: flashblock.index,
        }
    }
}

/// Connection transition labels matching the source-faithful H3 state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceConnectionTransitionV1 {
    /// The subscription owner task started.
    OwnerStart,
    /// Owner task began its first connection attempt.
    InitialConnectAttemptStarted,
    /// A connection attempt failed.
    ConnectFailure,
    /// Existing backoff began.
    BackoffStarted,
    /// Existing backoff completed.
    BackoffCompleted,
    /// The outer loop began an attempt after backoff.
    BackoffReconnectAttemptStarted,
    /// A connection was established.
    Established,
    /// A data message was yielded before decode.
    DataMessageYielded,
    /// An upstream control ping was received.
    ControlPingReceived,
    /// An upstream control pong was received.
    ControlPongReceived,
    /// The existing timer made an outgoing ping due.
    OutgoingPingDue,
    /// An outgoing ping was written successfully.
    OutgoingPingWritten,
    /// A pong was observed and cleared the existing wait flag.
    PongObserved,
    /// An upstream close frame was received.
    CloseFrameReceived,
    /// The established read returned an error.
    ReadError,
    /// The existing timer detected a missing pong.
    NoPongTimeout,
    /// The existing ping write failed.
    PingWriteFailure,
    /// A close frame ended an established interval.
    EstablishedClosedByClose,
    /// A read error ended an established interval.
    EstablishedClosedByReadError,
    /// The next outer-loop attempt followed close/read error without backoff.
    DirectReconnectAttemptStarted,
    /// A missing pong ended an established interval before existing backoff.
    EstablishedClosedByNoPong,
    /// A ping write failure ended an established interval before existing backoff.
    EstablishedClosedByPingWriteFailure,
    /// The read half closed and only the read arm was disabled.
    ReadHalfClosedWaitingForControl,
    /// A ping was written while the read half remained closed.
    OutgoingPingWrittenWhileReadHalfClosed,
}

/// One connection transition record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConnectionRecordV1 {
    /// Checked transition sequence.
    pub connection_sequence: u64,
    /// Transition label.
    pub transition: SourceConnectionTransitionV1,
}

/// Atomic producer cutoff receipt for the bounded measurement epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerEpochCutoffV1 {
    /// Producer epoch being closed.
    pub producer_epoch: u64,
    /// Last allocated clock observation ordinal.
    pub cutoff_clock_observation_ordinal: u64,
    /// Last allocated source generation.
    pub last_admitted_source_generation: u64,
    /// Last allocated pending sequence.
    pub last_pending_snapshot_sequence: u64,
}

/// Recorder state for clock, payload-first bindings, and connection transitions.
#[derive(Debug)]
pub struct EdgeMeasurementRecorderStateV1 {
    /// Next shared clock ordinal.
    pub next_clock_ordinal: u64,
    /// Next decoded source generation.
    pub next_source_generation: u64,
    /// First-write-wins payload bindings.
    pub payload_first: HashMap<PayloadFirstKeyV1, PayloadFirstObservationV1>,
    /// FIFO source generations awaiting processor admission by structural key.
    pub decoded_source_generations: HashMap<DecodedFlashblockKeyV1, VecDeque<u64>>,
    /// Ordered connection transitions.
    pub connection_records: Vec<SourceConnectionRecordV1>,
    /// Whether an invariant was poisoned.
    pub poisoned: bool,
    /// Optional cutoff receipt.
    pub cutoff: Option<ProducerEpochCutoffV1>,
}

/// Process-wide recorder used without changing public broadcast or receiver signatures.
#[derive(Debug)]
pub struct EdgeMeasurementRecorderV1 {
    producer_epoch: u64,
    start: Instant,
    state: Mutex<EdgeMeasurementRecorderStateV1>,
    registry: Arc<PendingMetadataRegistryV2>,
}

impl EdgeMeasurementRecorderV1 {
    /// Creates a recorder for a fixed process-local producer epoch.
    pub fn new(producer_epoch: u64) -> Self {
        Self {
            producer_epoch,
            start: Instant::now(),
            state: Mutex::new(EdgeMeasurementRecorderStateV1 {
                next_clock_ordinal: 0,
                next_source_generation: 0,
                payload_first: HashMap::new(),
                decoded_source_generations: HashMap::new(),
                connection_records: Vec::new(),
                poisoned: false,
                cutoff: None,
            }),
            registry: Arc::new(PendingMetadataRegistryV2::new(
                producer_epoch,
                PENDING_REGISTRY_CAPACITY_V2,
            )),
        }
    }

    /// Returns the shared pending metadata registry.
    pub fn registry(&self) -> Arc<PendingMetadataRegistryV2> {
        Arc::clone(&self.registry)
    }

    /// Samples realtime then monotonic under the shared ordinal critical section.
    pub fn observe_wire(&self, bytes: &[u8]) -> Option<WireObservationV1> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cutoff.is_some() {
            return None;
        }
        let ordinal = state.next_clock_ordinal;
        let Some(next) = ordinal.checked_add(1) else {
            state.poisoned = true;
            return None;
        };
        state.next_clock_ordinal = next;
        let realtime_ns =
            SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|value| value.as_nanos());
        let monotonic_ns = self.start.elapsed().as_nanos();
        let wire_digest = B256::from(DefaultCrypto.sha256(bytes));
        Some(WireObservationV1 {
            clock_observation_ordinal: ordinal,
            realtime_ns,
            monotonic_ns,
            wire_digest,
        })
    }

    /// Records successful decode and binds an index-zero payload first-write.
    pub fn decoded_flashblock(
        &self,
        observation: WireObservationV1,
        flashblock: &Flashblock,
    ) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cutoff.is_some() {
            return None;
        }
        let generation = state.next_source_generation;
        let Some(next) = generation.checked_add(1) else {
            state.poisoned = true;
            return None;
        };
        state.next_source_generation = next;
        state
            .decoded_source_generations
            .entry(DecodedFlashblockKeyV1::from_flashblock(flashblock))
            .or_default()
            .push_back(generation);
        if flashblock.index == 0 {
            let key = PayloadFirstKeyV1 {
                producer_epoch: self.producer_epoch,
                block_number: flashblock.metadata.block_number,
                payload_id: flashblock.payload_id.0.into(),
            };
            let binding =
                PayloadFirstObservationV1 { key, source_generation: generation, observation };
            match state.payload_first.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(binding);
                }
                Entry::Occupied(entry)
                    if entry.get().observation.wire_digest == observation.wire_digest => {}
                Entry::Occupied(_) => state.poisoned = true,
            }
        }
        Some(generation)
    }

    /// Takes the earliest decoded generation for one processor-admitted flashblock.
    pub fn take_source_generation(&self, flashblock: &Flashblock) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = DecodedFlashblockKeyV1::from_flashblock(flashblock);
        let generations = state.decoded_source_generations.get_mut(&key)?;
        let generation = generations.pop_front();
        if generations.is_empty() {
            state.decoded_source_generations.remove(&key);
        }
        generation
    }

    /// Appends one source-faithful connection transition.
    pub fn connection_transition(&self, transition: SourceConnectionTransitionV1) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Ok(sequence) = u64::try_from(state.connection_records.len()) else {
            state.poisoned = true;
            return;
        };
        state
            .connection_records
            .push(SourceConnectionRecordV1 { connection_sequence: sequence, transition });
    }

    /// Atomically latches the current measurement epoch cutoff once.
    pub fn latch_cutoff(&self) -> ProducerEpochCutoffV1 {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cutoff) = state.cutoff {
            return cutoff;
        }
        let registry = self.registry.snapshot();
        let cutoff = ProducerEpochCutoffV1 {
            producer_epoch: self.producer_epoch,
            cutoff_clock_observation_ordinal: state.next_clock_ordinal.saturating_sub(1),
            last_admitted_source_generation: state.next_source_generation.saturating_sub(1),
            last_pending_snapshot_sequence: registry
                .counters
                .advanced_with_snapshot
                .saturating_sub(1),
        };
        state.cutoff = Some(cutoff);
        cutoff
    }

    /// Returns connection records in append order.
    pub fn connection_records(&self) -> Vec<SourceConnectionRecordV1> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .connection_records
            .clone()
    }
}

/// Accessor for the single process-wide recorder used by the node pipeline.
#[derive(Debug, Clone, Copy)]
pub struct EdgeMeasurementGlobal;

impl EdgeMeasurementGlobal {
    /// Returns the lazily initialized recorder handle.
    pub fn recorder() -> Arc<EdgeMeasurementRecorderV1> {
        static RECORDER: OnceLock<Arc<EdgeMeasurementRecorderV1>> = OnceLock::new();
        Arc::clone(RECORDER.get_or_init(|| Arc::new(EdgeMeasurementRecorderV1::new(0))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::hex;
    use alloy_primitives::{Address, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata,
    };

    fn test_pending_blocks() -> Arc<PendingBlocks> {
        let flashblock = Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(1),
        };
        let mut builder = crate::PendingBlocksBuilder::new();
        builder.with_flashblocks([flashblock]);
        builder.with_header(Sealed::new_unchecked(Header::default(), B256::ZERO));
        Arc::new(builder.build().expect("pending blocks should build"))
    }

    #[test]
    fn wire_digest_uses_sha256_and_checked_ordinals() {
        let recorder = EdgeMeasurementRecorderV1::new(7);
        let first = recorder.observe_wire(b"").expect("first observation");
        let second = recorder.observe_wire(b"base").expect("second observation");
        assert_eq!(first.clock_observation_ordinal, 0);
        assert_eq!(second.clock_observation_ordinal, 1);
        assert_eq!(
            first.wire_digest,
            B256::from_slice(
                &hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                    .expect("valid vector")
            )
        );
    }

    #[test]
    fn pending_public_subset_digest_matches_node_crypto_golden_vector() {
        let digest = PendingPublicSubsetHasherV1::digest(
            1,
            2,
            [1, 2, 3, 4, 5, 6, 7, 8],
            3,
            B256::repeat_byte(4),
            B256::repeat_byte(5),
        );
        assert_eq!(
            digest,
            B256::from_slice(
                &hex::decode("0c59546a3812ea8b9234ebb83b20dbf3a24b6b59cc3c29a41b44627da19a5d97")
                    .expect("valid Node crypto SHA-256 vector")
            )
        );
    }

    #[test]
    fn h2_published_delivery_terminalizes_exactly_once() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(9, 4);
        let attempt = registry.register(&pending, Some(12));
        assert_eq!(attempt.pending_snapshot_sequence, Some(0));
        assert_eq!(attempt.disposition, PendingRegistrationDispositionV2::Succeeded);

        registry.record_send(attempt, Some(1)).expect("record published send");
        let metadata = registry.cli_received(&pending).expect("lookup succeeds");
        assert_eq!(metadata.source_generation, Some(12));
        assert_eq!(metadata.identity.producer_epoch, 9);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.primary_pending, 0);
        assert_eq!(snapshot.secondary_pending, 0);
        assert_eq!(snapshot.published_pending, 0);
        assert_eq!(snapshot.terminal_records, 1);
        assert_eq!(snapshot.counters.cli_received_lookup_succeeded, 1);
        assert!(!snapshot.poisoned);
    }

    #[test]
    fn h2_pointer_conflict_is_named_and_terminalized() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(9, 4);
        let first = registry.register(&pending, Some(1));
        let conflict = registry.register(&pending, Some(2));
        assert_eq!(
            conflict.disposition,
            PendingRegistrationDispositionV2::Failed(
                PendingRegistrationFailure::PendingPointerBindingConflict
            )
        );

        registry.record_send(first, Some(1)).expect("record first send");
        registry.record_send(conflict, Some(1)).expect("record conflict send");
        registry.cli_received(&pending).expect("first lookup succeeds");
        let failure = registry.cli_received(&pending).expect_err("conflict lookup fails");
        assert_eq!(
            failure.reason,
            CliRegistryLookupFailureReason::RegistrationFailed(
                PendingRegistrationFailure::PendingPointerBindingConflict
            )
        );

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.primary_pending, 0);
        assert_eq!(snapshot.published_pending, 0);
        assert_eq!(snapshot.terminal_records, 2);
        assert_eq!(snapshot.counters.registration_failed, 1);
        assert_eq!(snapshot.counters.cli_registry_lookup_failed, 1);
        assert!(snapshot.poisoned);
    }

    #[test]
    fn decoded_source_generation_is_joined_fifo_to_processor_admission() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = EdgeMeasurementRecorderV1::new(3);
        let observation = recorder.observe_wire(b"frame").expect("wire observation");
        assert_eq!(recorder.decoded_flashblock(observation, &flashblock), Some(0));
        assert_eq!(recorder.take_source_generation(&flashblock), Some(0));
        assert_eq!(recorder.take_source_generation(&flashblock), None);
    }
    #[test]
    fn connection_records_preserve_direct_and_backoff_paths() {
        let recorder = EdgeMeasurementRecorderV1::new(1);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::ConnectFailure);
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffCompleted);
        recorder
            .connection_transition(SourceConnectionTransitionV1::BackoffReconnectAttemptStarted);
        assert_eq!(recorder.connection_records().len(), 5);
    }
}
