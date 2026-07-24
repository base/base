//! Feature-private edge measurement state for the flashblocks producer.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::{c_int, c_long},
    fs,
    num::NonZeroU64,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak,
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
};

use alloy_primitives::B256;
use base_common_flashblocks::Flashblock;
use revm::precompile::{Crypto as _, DefaultCrypto};

use self::{ConnectionPhaseV1 as Phase, SourceConnectionTransitionV1 as Transition};
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

/// Named checked-accounting fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAccountingFieldV2 {
    /// Advanced snapshot attempts.
    AdvancedWithSnapshot,
    /// Successful registrations.
    RegistrationSucceeded,
    /// Failed registrations.
    RegistrationFailed,
    /// Published sends.
    SendPublished,
    /// No-receiver sends.
    SendNoReceivers,
    /// Successful CLI lookups.
    CliReceivedLookupSucceeded,
    /// Failed CLI lookups.
    CliRegistryLookupFailed,
    /// Lag-attributed publications.
    CliLaggedAttributed,
    /// Close-attributed publications.
    CliClosedAttributed,
    /// Cancel-attributed publications.
    CliCancelledAttributed,
}

/// Named reasons why registration could not establish an authoritative identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistrationFailure {
    /// The checked snapshot sequence overflowed.
    PendingSnapshotSequenceOverflow,
    /// A checked accounting field overflowed.
    PendingAccountingOverflow(PendingAccountingFieldV2),
    /// The owner-approved registry capacity was exceeded.
    PendingRegistryCapacityOverflow,
    /// The registry mutex was poisoned by an earlier panic.
    PendingRegistryLockPoisoned,
    /// A pointer was bound to a different retained allocation.
    PendingPointerBindingConflict,
    /// A retained weak identity expired before registration completed.
    PendingArcIdentityExpired,
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
    /// The send was an explicit unchanged-snapshot passthrough.
    PassthroughNonAdvanced,
    /// The advanced send occurred after authority cutoff and has no authority registration.
    PostCutoffAdvancedNonAuthority,
    /// A checked accounting field overflowed.
    PendingAccountingOverflow(PendingAccountingFieldV2),
}

/// Explicit non-authority entries in the every-send publication journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSendJournalMarkerV2 {
    /// Existing production behavior re-sent an unchanged snapshot.
    PassthroughNonAdvanced,
    /// An advanced snapshot was sent after authority cutoff.
    PostCutoffAdvancedNonAuthority,
}

/// One every-send journal entry consumed in broadcast receive order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSendJournalEntryV2 {
    /// An advanced authority registration attempt.
    AdvancedRegistration(u64),
    /// Registration failed before a sequence could be allocated.
    RegistrationFailedWithoutSequence(PendingRegistrationFailure),
    /// Explicit non-authority send marker.
    NonAuthority(PendingSendJournalMarkerV2),
}

/// Exact named CLI lookup-failure terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliRegistryLookupFailed {
    /// The sequence expected by the ordered publication journal, if one existed.
    pub pending_snapshot_sequence: Option<u64>,
    /// The exact lookup failure reason.
    pub reason: CliRegistryLookupFailureReason,
}

/// Errors while recording or terminalizing publication accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistryError {
    /// A range length could not be represented on this platform.
    RangeLengthOverflow,
    /// The published journal contained fewer entries than the exact terminal range.
    PublishedRangeMismatch,
    /// A sequence in the published journal had no primary entry.
    MissingPrimaryEntry,
    /// A send or terminal was recorded more than once.
    DuplicateTerminal,
    /// A terminal durability acknowledgement was not at the queue head.
    DurabilityAckOrderMismatch,
    /// The checked coverage acceptance sequence overflowed.
    CoverageSequenceOverflow,
    /// A checked accounting field overflowed.
    AccountingOverflow(PendingAccountingFieldV2),
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

/// Production broadcast disposition for a registration attempt.
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
    /// CLI received the `Arc` and metadata lookup succeeded.
    CliReceivedLookupSucceeded,
    /// CLI received the `Arc`, but metadata lookup failed exactly.
    CliRegistryLookupFailed(CliRegistryLookupFailureReason),
    /// Tokio attributed the sequence to an exact lagged range.
    CliLagged,
    /// Channel closure attributed the exact remaining range.
    CliClosed,
    /// Shutdown cancellation attributed the exact remaining range.
    CliCancelled,
    /// A successful registration had no receivers.
    NoReceivers,
    /// A failed registration had no receivers.
    RegistrationFailedNoReceivers,
}

/// A registration token held by the processor until the existing send returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRegistrationAttemptV2 {
    /// The allocated sequence, absent only after sequence overflow.
    pub pending_snapshot_sequence: Option<u64>,
    /// The registration disposition terminalized before send.
    pub disposition: PendingRegistrationDispositionV2,
}

/// A terminal record accepted by the coverage queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingTerminalRecordV2 {
    /// Independent checked sequence allocated when the coverage queue accepts this terminal.
    pub coverage_sequence: u64,
    /// Immutable snapshot metadata.
    pub metadata: PendingSnapshotMetadataV2,
    /// Registration disposition.
    pub registration: PendingRegistrationDispositionV2,
    /// Production send disposition.
    pub send: PendingSendDispositionV2,
    /// Exact delivery terminal.
    pub terminal: PendingCliTerminalV2,
}

/// Observable cleanup ordering used by the measurement ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCleanupEventV2 {
    /// The exact terminal was appended.
    TerminalAppended(u64),
    /// The coverage queue accepted the terminal record.
    CoverageQueueAccepted(u64),
    /// The caller acknowledged durable persistence of the accepted record.
    DurabilityAcknowledged(u64),
    /// The retained FIFO identity was removed after terminal durability.
    SecondaryRemoved(u64),
    /// The primary sequence entry was removed.
    PrimaryRemoved(u64),
    /// The retained weak identity was dropped.
    RetainedWeakDropped(u64),
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
    /// Terminal accepted by the coverage queue, pending explicit durability acknowledgement.
    pub terminal: Option<PendingCliTerminalV2>,
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

/// Exact sequence sets used by every H2 conservation equation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingRegistrySequenceSetsV2 {
    /// All allocated advanced snapshots.
    pub advanced_with_snapshot: BTreeSet<u64>,
    /// Registration successes.
    pub registration_succeeded: BTreeSet<u64>,
    /// Registration failures.
    pub registration_failed: BTreeSet<u64>,
    /// Successful registrations subsequently published.
    pub registered_published: BTreeSet<u64>,
    /// Successful registrations with no receivers.
    pub registered_no_receivers: BTreeSet<u64>,
    /// Failed registrations subsequently published.
    pub failed_registration_published: BTreeSet<u64>,
    /// Failed registrations with no receivers.
    pub failed_registration_no_receivers: BTreeSet<u64>,
    /// All published sends.
    pub send_published: BTreeSet<u64>,
    /// All no-receiver sends.
    pub send_no_receivers: BTreeSet<u64>,
    /// CLI lookup successes.
    pub cli_received_lookup_succeeded: BTreeSet<u64>,
    /// CLI lookup failures.
    pub cli_registry_lookup_failed: BTreeSet<u64>,
    /// Lag-attributed sequences.
    pub cli_lagged_attributed: BTreeSet<u64>,
    /// Close-attributed sequences.
    pub cli_closed_attributed: BTreeSet<u64>,
    /// Cancel-attributed sequences.
    pub cli_cancelled_attributed: BTreeSet<u64>,
    /// Published sequences lacking a CLI terminal.
    pub pending_delivery_final: BTreeSet<u64>,
    /// All CLI `Ok` receipts.
    pub cli_ok_received: BTreeSet<u64>,
    /// Snapshot records installed before measurement lookup.
    pub snapshot_records_installed: BTreeSet<u64>,
    /// Failed registrations ending in lookup failure.
    pub failed_reg_cli_registry_lookup_failed: BTreeSet<u64>,
    /// Failed registrations attributed to lag.
    pub failed_reg_cli_lagged_attributed: BTreeSet<u64>,
    /// Failed registrations attributed to close.
    pub failed_reg_cli_closed_attributed: BTreeSet<u64>,
    /// Failed registrations attributed to cancellation.
    pub failed_reg_cli_cancelled_attributed: BTreeSet<u64>,
    /// Failed registrations lacking a terminal.
    pub failed_reg_pending_final: BTreeSet<u64>,
    /// Failed registrations with no receivers.
    pub registration_failed_no_receivers: BTreeSet<u64>,
    /// Forbidden failed-registration lookup-success intersection.
    pub failed_reg_cli_received_lookup_succeeded: BTreeSet<u64>,
}

/// Named poison latched without changing the production send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRegistryPoisonV2 {
    /// The checked publication sequence overflowed.
    SequenceOverflow,
    /// A checked counter overflowed.
    AccountingOverflow(PendingAccountingFieldV2),
    /// A sequence appeared twice in one exact set.
    DuplicateSequence(u64),
    /// A primary, secondary, or terminal binding was inconsistent.
    BindingConflict(u64),
    /// A failed registration was incorrectly classified as lookup success.
    PendingRegistrationFailureUnexpectedLookupSuccess(u64),
    /// A lock was recovered after poisoning.
    LockPoisoned,
}

/// Mutable registry state protected by one non-async critical section.
#[derive(Debug)]
pub struct PendingRegistryStateV2 {
    /// The next checked pending snapshot sequence.
    pub next_sequence: u64,
    /// The next checked coverage-queue acceptance sequence.
    pub next_coverage_sequence: u64,
    /// Primary entries keyed by sequence within the fixed producer epoch.
    pub primary: BTreeMap<u64, PendingRegistryEntryV2>,
    /// Pointer-to-sequence FIFO index for successful registrations.
    pub secondary: HashMap<usize, VecDeque<u64>>,
    /// Ordered production sends waiting for CLI attribution.
    pub published: VecDeque<u64>,
    /// Every successful broadcast send in exact production order.
    pub send_journal: VecDeque<PendingSendJournalEntryV2>,
    /// Non-authority sends begun before the unchanged production send returns.
    pub unregistered_send_inflight: u64,
    /// Visible disposition for every non-authority broadcast send attempt.
    pub unregistered_send_records: Vec<(PendingSendJournalMarkerV2, PendingSendDispositionV2)>,
    /// Terminal records accepted by the coverage queue.
    pub terminal_records: Vec<PendingTerminalRecordV2>,
    /// FIFO `(coverage sequence, pending snapshot sequence)` bindings awaiting durability.
    pub durability_pending: VecDeque<(u64, u64)>,
    /// Sequences explicitly acknowledged durable by the caller.
    pub durability_acked: BTreeSet<u64>,
    /// Cleanup order evidence.
    pub cleanup_events: Vec<PendingCleanupEventV2>,
    /// H2 product counters.
    pub counters: PendingRegistryCountersV2,
    /// Exact H2 sets.
    pub sets: PendingRegistrySequenceSetsV2,
    /// Named poison observations.
    pub poisons: Vec<PendingRegistryPoisonV2>,
    /// Whether any exact accounting or identity poison was observed.
    pub poisoned: bool,
}

/// Final H2/seal verification failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingFinalSealErrorV2 {
    /// A prior named poison was latched.
    Poisoned,
    /// A sorted sequence-set equation failed.
    SequenceSetMismatch,
    /// A disjoint union had an overlapping member.
    SequenceSetOverlap,
    /// A forbidden final-pending set was nonempty.
    PendingDeliveryNotEmpty,
    /// A terminal record has not received explicit durability acknowledgement.
    DurabilityAckPending,
    /// Primary, secondary, or outstanding-reference state remains live.
    RegistryNotEmpty,
}

/// Feature-private pending metadata registry shared by processor and sole trader CLI.
#[derive(Debug)]
pub struct PendingMetadataRegistryV2 {
    producer_epoch: u64,
    capacity: usize,
    state: Mutex<PendingRegistryStateV2>,
    send_recorded: Condvar,
}

impl PendingMetadataRegistryV2 {
    /// Creates a registry for a fixed producer epoch and owner-approved capacity.
    pub fn new(producer_epoch: u64, capacity: usize) -> Self {
        Self {
            producer_epoch,
            capacity,
            state: Mutex::new(PendingRegistryStateV2 {
                next_sequence: 0,
                next_coverage_sequence: 0,
                primary: BTreeMap::new(),
                secondary: HashMap::new(),
                published: VecDeque::new(),
                send_journal: VecDeque::new(),
                unregistered_send_inflight: 0,
                unregistered_send_records: Vec::new(),
                terminal_records: Vec::new(),
                durability_pending: VecDeque::new(),
                durability_acked: BTreeSet::new(),
                cleanup_events: Vec::new(),
                counters: PendingRegistryCountersV2::default(),
                sets: PendingRegistrySequenceSetsV2::default(),
                poisons: Vec::new(),
                poisoned: false,
            }),
            send_recorded: Condvar::new(),
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
        if was_poisoned {
            Self::poison(&mut state, PendingRegistryPoisonV2::LockPoisoned);
        }
        let accounting_failure =
            Self::increment(&mut state, PendingAccountingFieldV2::AdvancedWithSnapshot).err();

        let Some(next_sequence) = state.next_sequence.checked_add(1) else {
            Self::poison(&mut state, PendingRegistryPoisonV2::SequenceOverflow);
            let failure =
                match Self::increment(&mut state, PendingAccountingFieldV2::RegistrationFailed) {
                    Ok(()) => PendingRegistrationFailure::PendingSnapshotSequenceOverflow,
                    Err(field) => PendingRegistrationFailure::PendingAccountingOverflow(field),
                };
            return PendingRegistrationAttemptV2 {
                pending_snapshot_sequence: None,
                disposition: PendingRegistrationDispositionV2::Failed(failure),
            };
        };
        let sequence = state.next_sequence;
        state.next_sequence = next_sequence;
        Self::insert(&mut state, sequence, |sets| &mut sets.advanced_with_snapshot);

        let pointer = Arc::as_ptr(pending) as usize;
        let metadata = PendingSnapshotMetadataV2 {
            identity: PendingSnapshotIdentityV2 {
                producer_epoch: self.producer_epoch,
                pending_snapshot_sequence: sequence,
                arc_pointer_identity: pointer,
            },
            source_generation,
            pending_public_subset_digest_v1: Self::pending_public_subset_digest_v1(pending),
        };

        let pointer_failure = state.secondary.get(&pointer).and_then(|queue| {
            queue.iter().find_map(|indexed| {
                let entry = state.primary.get(indexed)?;
                match entry.retained_weak.upgrade() {
                    None => Some(PendingRegistrationFailure::PendingArcIdentityExpired),
                    Some(retained) if !Arc::ptr_eq(&retained, pending) => {
                        Some(PendingRegistrationFailure::PendingPointerBindingConflict)
                    }
                    Some(_) => None,
                }
            })
        });
        let failure = accounting_failure
            .map(PendingRegistrationFailure::PendingAccountingOverflow)
            .or_else(|| {
                was_poisoned.then_some(PendingRegistrationFailure::PendingRegistryLockPoisoned)
            })
            .or_else(|| {
                (state.primary.len() >= self.capacity)
                    .then_some(PendingRegistrationFailure::PendingRegistryCapacityOverflow)
            })
            .or(pointer_failure)
            .or_else(|| {
                state.counters.registration_succeeded.checked_add(1).is_none().then_some(
                    PendingRegistrationFailure::PendingAccountingOverflow(
                        PendingAccountingFieldV2::RegistrationSucceeded,
                    ),
                )
            });
        let disposition = failure.map_or(
            PendingRegistrationDispositionV2::Succeeded,
            PendingRegistrationDispositionV2::Failed,
        );

        match disposition {
            PendingRegistrationDispositionV2::Succeeded => {
                if let Err(field) =
                    Self::increment(&mut state, PendingAccountingFieldV2::RegistrationSucceeded)
                {
                    Self::poison(&mut state, PendingRegistryPoisonV2::AccountingOverflow(field));
                }
                Self::insert(&mut state, sequence, |sets| &mut sets.registration_succeeded);
                state.secondary.entry(pointer).or_default().push_back(sequence);
            }
            PendingRegistrationDispositionV2::Failed(_) => {
                state.poisoned = true;
                if let Err(field) =
                    Self::increment(&mut state, PendingAccountingFieldV2::RegistrationFailed)
                {
                    Self::poison(&mut state, PendingRegistryPoisonV2::AccountingOverflow(field));
                }
                Self::insert(&mut state, sequence, |sets| &mut sets.registration_failed);
            }
        }
        state.primary.insert(
            sequence,
            PendingRegistryEntryV2 {
                metadata,
                retained_weak: Arc::downgrade(pending),
                registration: disposition,
                send: None,
                terminal: None,
            },
        );
        PendingRegistrationAttemptV2 { pending_snapshot_sequence: Some(sequence), disposition }
    }

    /// Marks a non-authority send as in flight before the unchanged broadcast call.
    pub fn begin_unregistered_send(&self) {
        let (mut state, was_poisoned) = self.lock_state();
        if was_poisoned {
            Self::poison(&mut state, PendingRegistryPoisonV2::LockPoisoned);
        }
        state.unregistered_send_inflight =
            state.unregistered_send_inflight.checked_add(1).unwrap_or_else(|| {
                Self::poison(
                    &mut state,
                    PendingRegistryPoisonV2::AccountingOverflow(
                        PendingAccountingFieldV2::SendPublished,
                    ),
                );
                u64::MAX
            });
    }

    /// Records an explicit passthrough or post-cutoff send disposition.
    pub fn record_unregistered_send(
        &self,
        marker: PendingSendJournalMarkerV2,
        receiver_count: Option<usize>,
    ) -> Result<(), PendingRegistryError> {
        let mut state = match self.lock_state_checked() {
            Ok(state) => state,
            Err(error) => {
                self.send_recorded.notify_all();
                return Err(error);
            }
        };
        state.unregistered_send_inflight = state.unregistered_send_inflight.checked_sub(1).ok_or(
            PendingRegistryError::AccountingOverflow(PendingAccountingFieldV2::SendPublished),
        )?;
        let send = receiver_count.map_or(PendingSendDispositionV2::NoReceivers, |receiver_count| {
            PendingSendDispositionV2::Published { receiver_count }
        });
        state.unregistered_send_records.push((marker, send));
        if receiver_count.is_some() {
            state.send_journal.push_back(PendingSendJournalEntryV2::NonAuthority(marker));
        }
        drop(state);
        self.send_recorded.notify_all();
        Ok(())
    }
    /// Records the result of the exactly-once existing broadcast send.
    pub fn record_send(
        &self,
        attempt: PendingRegistrationAttemptV2,
        receiver_count: Option<usize>,
    ) -> Result<(), PendingRegistryError> {
        let Some(sequence) = attempt.pending_snapshot_sequence else {
            let PendingRegistrationDispositionV2::Failed(failure) = attempt.disposition else {
                return Ok(());
            };
            let mut state = self.lock_state_checked()?;
            if receiver_count.is_some() {
                state.send_journal.push_back(
                    PendingSendJournalEntryV2::RegistrationFailedWithoutSequence(failure),
                );
            }
            drop(state);
            self.send_recorded.notify_all();
            return Ok(());
        };
        let mut state = match self.lock_state_checked() {
            Ok(state) => state,
            Err(error) => {
                self.send_recorded.notify_all();
                return Err(error);
            }
        };
        let result = (|| {
            let send = receiver_count
                .map_or(PendingSendDispositionV2::NoReceivers, |receiver_count| {
                    PendingSendDispositionV2::Published { receiver_count }
                });
            let registration = {
                let Some(entry) = state.primary.get_mut(&sequence) else {
                    state.poisoned = true;
                    return Err(PendingRegistryError::MissingPrimaryEntry);
                };
                if entry.send.replace(send).is_some() {
                    Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(sequence));
                    return Err(PendingRegistryError::DuplicateTerminal);
                }
                entry.registration
            };

            match send {
                PendingSendDispositionV2::Published { .. } => {
                    Self::increment(&mut state, PendingAccountingFieldV2::SendPublished)
                        .map_err(PendingRegistryError::AccountingOverflow)?;
                    Self::insert(&mut state, sequence, |sets| &mut sets.send_published);
                    Self::insert(&mut state, sequence, |sets| &mut sets.pending_delivery_final);
                    if matches!(registration, PendingRegistrationDispositionV2::Failed(_)) {
                        Self::insert(&mut state, sequence, |sets| {
                            &mut sets.failed_reg_pending_final
                        });
                    }
                    match registration {
                        PendingRegistrationDispositionV2::Succeeded => {
                            Self::insert(&mut state, sequence, |sets| {
                                &mut sets.registered_published
                            });
                        }
                        PendingRegistrationDispositionV2::Failed(_) => {
                            Self::insert(&mut state, sequence, |sets| {
                                &mut sets.failed_registration_published
                            });
                        }
                    }
                    state.published.push_back(sequence);
                    state
                        .send_journal
                        .push_back(PendingSendJournalEntryV2::AdvancedRegistration(sequence));
                }
                PendingSendDispositionV2::NoReceivers => {
                    Self::increment(&mut state, PendingAccountingFieldV2::SendNoReceivers)
                        .map_err(PendingRegistryError::AccountingOverflow)?;
                    Self::insert(&mut state, sequence, |sets| &mut sets.send_no_receivers);
                    let terminal = match registration {
                        PendingRegistrationDispositionV2::Succeeded => {
                            Self::insert(&mut state, sequence, |sets| {
                                &mut sets.registered_no_receivers
                            });
                            PendingCliTerminalV2::NoReceivers
                        }
                        PendingRegistrationDispositionV2::Failed(_) => {
                            Self::insert(&mut state, sequence, |sets| {
                                &mut sets.failed_registration_no_receivers
                            });
                            Self::insert(&mut state, sequence, |sets| {
                                &mut sets.registration_failed_no_receivers
                            });
                            PendingCliTerminalV2::RegistrationFailedNoReceivers
                        }
                    };
                    self.terminalize_locked(&mut state, sequence, terminal)?;
                }
            }
            Ok(())
        })();
        drop(state);
        self.send_recorded.notify_all();
        result
    }

    /// Records one CLI `Ok` after the existing snapshot installation.
    pub fn cli_received(
        &self,
        pending: &Arc<PendingBlocks>,
    ) -> Result<
        Result<PendingSnapshotMetadataV2, PendingSendJournalMarkerV2>,
        CliRegistryLookupFailed,
    > {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                Self::poison(&mut state, PendingRegistryPoisonV2::LockPoisoned);
                return Err(CliRegistryLookupFailed {
                    pending_snapshot_sequence: state.published.front().copied(),
                    reason: CliRegistryLookupFailureReason::NoPublishedSequence,
                });
            }
        };
        while state.send_journal.is_empty()
            && (state.unregistered_send_inflight != 0
                || state.primary.values().any(|entry| entry.send.is_none()))
        {
            state = match self.send_recorded.wait(state) {
                Ok(state) => state,
                Err(poisoned) => {
                    let mut state = poisoned.into_inner();
                    Self::poison(&mut state, PendingRegistryPoisonV2::LockPoisoned);
                    return Err(CliRegistryLookupFailed {
                        pending_snapshot_sequence: None,
                        reason: CliRegistryLookupFailureReason::NoPublishedSequence,
                    });
                }
            };
        }
        let Some(journal_entry) = state.send_journal.pop_front() else {
            state.poisoned = true;
            return Err(CliRegistryLookupFailed {
                pending_snapshot_sequence: None,
                reason: CliRegistryLookupFailureReason::NoPublishedSequence,
            });
        };
        let sequence = match journal_entry {
            PendingSendJournalEntryV2::AdvancedRegistration(sequence) => {
                if state.published.pop_front() != Some(sequence) {
                    Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(sequence));
                    return Err(CliRegistryLookupFailed {
                        pending_snapshot_sequence: Some(sequence),
                        reason: CliRegistryLookupFailureReason::PendingPointerBindingConflict,
                    });
                }
                sequence
            }
            PendingSendJournalEntryV2::NonAuthority(marker) => {
                return Ok(Err(marker));
            }
            PendingSendJournalEntryV2::RegistrationFailedWithoutSequence(failure) => {
                return Err(CliRegistryLookupFailed {
                    pending_snapshot_sequence: None,
                    reason: CliRegistryLookupFailureReason::RegistrationFailed(failure),
                });
            }
        };
        Self::insert(&mut state, sequence, |sets| &mut sets.cli_ok_received);
        Self::insert(&mut state, sequence, |sets| &mut sets.snapshot_records_installed);

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
                        .and_then(|queue| {
                            queue.iter().find(|indexed| {
                                state
                                    .primary
                                    .get(indexed)
                                    .is_some_and(|candidate| candidate.terminal.is_none())
                            })
                        })
                        .is_some_and(|indexed| *indexed == sequence);
                    if !index_matches {
                        Some(CliRegistryLookupFailureReason::PendingPointerBindingConflict)
                    } else {
                        match entry.retained_weak.upgrade() {
                            None => Some(CliRegistryLookupFailureReason::PendingArcIdentityExpired),
                            Some(retained) if !Arc::ptr_eq(&retained, pending) => {
                                Some(CliRegistryLookupFailureReason::PendingArcIdentityMismatch)
                            }
                            Some(_)
                                if entry.metadata.pending_public_subset_digest_v1
                                    != Self::pending_public_subset_digest_v1(pending) =>
                            {
                                Some(CliRegistryLookupFailureReason::PendingPublicSubsetCorruption)
                            }
                            Some(_) => None,
                        }
                    }
                }
            },
        );

        if let Some(reason) = reason {
            state.poisoned = true;
            let accounting =
                Self::increment(&mut state, PendingAccountingFieldV2::CliRegistryLookupFailed);
            Self::insert(&mut state, sequence, |sets| &mut sets.cli_registry_lookup_failed);
            if state.sets.registration_failed.contains(&sequence) {
                Self::insert(&mut state, sequence, |sets| {
                    &mut sets.failed_reg_cli_registry_lookup_failed
                });
            }
            let terminal_reason = accounting
                .err()
                .map(CliRegistryLookupFailureReason::PendingAccountingOverflow)
                .unwrap_or(reason);
            if self
                .terminalize_locked(
                    &mut state,
                    sequence,
                    PendingCliTerminalV2::CliRegistryLookupFailed(terminal_reason),
                )
                .is_err()
            {
                Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(sequence));
            }
            return Err(CliRegistryLookupFailed {
                pending_snapshot_sequence: Some(sequence),
                reason: terminal_reason,
            });
        }

        let metadata = state.primary.get(&sequence).map(|entry| entry.metadata).ok_or(
            CliRegistryLookupFailed {
                pending_snapshot_sequence: Some(sequence),
                reason: CliRegistryLookupFailureReason::MissingPrimaryEntry,
            },
        )?;
        if let Err(field) =
            Self::increment(&mut state, PendingAccountingFieldV2::CliReceivedLookupSucceeded)
        {
            Self::poison(&mut state, PendingRegistryPoisonV2::AccountingOverflow(field));
            let reason = CliRegistryLookupFailureReason::PendingAccountingOverflow(field);
            Self::insert(&mut state, sequence, |sets| &mut sets.cli_registry_lookup_failed);
            if self
                .terminalize_locked(
                    &mut state,
                    sequence,
                    PendingCliTerminalV2::CliRegistryLookupFailed(reason),
                )
                .is_err()
            {
                Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(sequence));
            }
            return Err(CliRegistryLookupFailed {
                pending_snapshot_sequence: Some(sequence),
                reason,
            });
        }
        Self::insert(&mut state, sequence, |sets| &mut sets.cli_received_lookup_succeeded);
        self.terminalize_locked(
            &mut state,
            sequence,
            PendingCliTerminalV2::CliReceivedLookupSucceeded,
        )
        .map_err(|_| CliRegistryLookupFailed {
            pending_snapshot_sequence: Some(sequence),
            reason: CliRegistryLookupFailureReason::MissingPrimaryEntry,
        })?;
        Ok(Ok(metadata))
    }

    /// Attributes exactly `count` broadcast journal entries to a Tokio lag terminal.
    pub fn cli_lagged(&self, count: u64) -> Result<(), PendingRegistryError> {
        let count =
            usize::try_from(count).map_err(|_| PendingRegistryError::RangeLengthOverflow)?;
        let mut state = self.lock_state_checked()?;
        let authority_count = self.terminalize_send_journal_locked(
            &mut state,
            count,
            PendingCliTerminalV2::CliLagged,
        )?;
        Self::add_count(
            &mut state,
            PendingAccountingFieldV2::CliLaggedAttributed,
            u64::try_from(authority_count)
                .map_err(|_| PendingRegistryError::RangeLengthOverflow)?,
        )
    }

    /// Attributes the exact remaining broadcast journal to channel closure.
    pub fn cli_closed(&self) -> Result<(), PendingRegistryError> {
        let mut state = self.lock_state_checked()?;
        let count = state.send_journal.len();
        let authority_count = self.terminalize_send_journal_locked(
            &mut state,
            count,
            PendingCliTerminalV2::CliClosed,
        )?;
        Self::add_count(
            &mut state,
            PendingAccountingFieldV2::CliClosedAttributed,
            u64::try_from(authority_count)
                .map_err(|_| PendingRegistryError::RangeLengthOverflow)?,
        )
    }

    /// Attributes the exact remaining broadcast journal to shutdown cancellation.
    pub fn cli_cancelled(&self) -> Result<(), PendingRegistryError> {
        let mut state = self.lock_state_checked()?;
        let count = state.send_journal.len();
        let authority_count = self.terminalize_send_journal_locked(
            &mut state,
            count,
            PendingCliTerminalV2::CliCancelled,
        )?;
        Self::add_count(
            &mut state,
            PendingAccountingFieldV2::CliCancelledAttributed,
            u64::try_from(authority_count)
                .map_err(|_| PendingRegistryError::RangeLengthOverflow)?,
        )
    }

    /// Acknowledges that the coverage-queue head was durably persisted, then cleans it up.
    pub fn ack_terminal_durable(&self, coverage_sequence: u64) -> Result<(), PendingRegistryError> {
        let mut state = self.lock_state_checked()?;
        let Some((queued_coverage, pending_sequence)) = state.durability_pending.front().copied()
        else {
            Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(coverage_sequence));
            return Err(PendingRegistryError::DurabilityAckOrderMismatch);
        };
        if queued_coverage != coverage_sequence {
            Self::poison(&mut state, PendingRegistryPoisonV2::BindingConflict(coverage_sequence));
            return Err(PendingRegistryError::DurabilityAckOrderMismatch);
        }
        let (pointer, registration) = state
            .primary
            .get(&pending_sequence)
            .map(|entry| (entry.metadata.identity.arc_pointer_identity, entry.registration))
            .ok_or(PendingRegistryError::MissingPrimaryEntry)?;
        let remove_secondary_key = if registration == PendingRegistrationDispositionV2::Succeeded {
            let sequences = state
                .secondary
                .get_mut(&pointer)
                .ok_or(PendingRegistryError::MissingPrimaryEntry)?;
            if sequences.front() != Some(&pending_sequence) {
                Self::poison(
                    &mut state,
                    PendingRegistryPoisonV2::BindingConflict(pending_sequence),
                );
                return Err(PendingRegistryError::DurabilityAckOrderMismatch);
            }
            sequences.pop_front();
            Some(sequences.is_empty())
        } else {
            None
        };

        state.durability_pending.pop_front();
        state.durability_acked.insert(coverage_sequence);
        state.cleanup_events.push(PendingCleanupEventV2::DurabilityAcknowledged(coverage_sequence));
        if let Some(remove_secondary_key) = remove_secondary_key {
            state.cleanup_events.push(PendingCleanupEventV2::SecondaryRemoved(pending_sequence));
            if remove_secondary_key {
                state.secondary.remove(&pointer);
            }
        }
        let entry = state
            .primary
            .remove(&pending_sequence)
            .ok_or(PendingRegistryError::MissingPrimaryEntry)?;
        state.cleanup_events.push(PendingCleanupEventV2::PrimaryRemoved(pending_sequence));
        drop(entry.retained_weak);
        state.cleanup_events.push(PendingCleanupEventV2::RetainedWeakDropped(pending_sequence));
        Ok(())
    }

    /// Returns executable product sets and live pending counts.
    pub fn snapshot(&self) -> PendingRegistrySnapshotV2 {
        let (state, was_poisoned) = self.lock_state();
        PendingRegistrySnapshotV2 {
            counters: state.counters,
            sets: state.sets.clone(),
            primary_pending: state.primary.len(),
            secondary_pending: state.secondary.values().map(VecDeque::len).sum(),
            published_pending: state.published.len(),
            send_journal: state.send_journal.iter().copied().collect(),
            unregistered_send_records: state.unregistered_send_records.clone(),
            unregistered_send_inflight: state.unregistered_send_inflight,
            coverage_queue_pending_ack: state.durability_pending.len(),
            durability_acked: state.durability_acked.len(),
            terminal_records: state.terminal_records.len(),
            coverage_count: state.next_coverage_sequence,
            last_coverage_sequence: state.next_coverage_sequence.checked_sub(1),
            last_pending_snapshot_sequence: state.next_sequence.checked_sub(1),
            poisoned: state.poisoned || was_poisoned,
        }
    }

    /// Returns terminal records in coverage-queue acceptance order.
    pub fn terminal_records(&self) -> Vec<PendingTerminalRecordV2> {
        self.lock_state().0.terminal_records.clone()
    }

    /// Returns cleanup-order evidence.
    pub fn cleanup_events(&self) -> Vec<PendingCleanupEventV2> {
        self.lock_state().0.cleanup_events.clone()
    }

    /// Verifies all sorted disjoint-union equations and the final empty seal.
    pub fn verify_final_seal(&self) -> Result<(), PendingFinalSealErrorV2> {
        let (state, was_poisoned) = self.lock_state();
        let sets = &state.sets;
        Self::partition(
            &sets.advanced_with_snapshot,
            &[&sets.registration_succeeded, &sets.registration_failed],
        )?;
        Self::partition(
            &sets.registration_succeeded,
            &[&sets.registered_published, &sets.registered_no_receivers],
        )?;
        Self::partition(
            &sets.registration_failed,
            &[&sets.failed_registration_published, &sets.failed_registration_no_receivers],
        )?;
        Self::partition(
            &sets.send_published,
            &[&sets.registered_published, &sets.failed_registration_published],
        )?;
        Self::partition(
            &sets.send_no_receivers,
            &[&sets.registered_no_receivers, &sets.failed_registration_no_receivers],
        )?;
        Self::partition(
            &sets.advanced_with_snapshot,
            &[&sets.send_published, &sets.send_no_receivers],
        )?;
        Self::partition(
            &sets.send_published,
            &[
                &sets.cli_received_lookup_succeeded,
                &sets.cli_registry_lookup_failed,
                &sets.cli_lagged_attributed,
                &sets.cli_closed_attributed,
                &sets.cli_cancelled_attributed,
                &sets.pending_delivery_final,
            ],
        )?;
        Self::partition(
            &sets.cli_ok_received,
            &[&sets.cli_received_lookup_succeeded, &sets.cli_registry_lookup_failed],
        )?;
        Self::partition(&sets.snapshot_records_installed, &[&sets.cli_ok_received])?;
        Self::partition(
            &sets.failed_registration_published,
            &[
                &sets.failed_reg_cli_registry_lookup_failed,
                &sets.failed_reg_cli_lagged_attributed,
                &sets.failed_reg_cli_closed_attributed,
                &sets.failed_reg_cli_cancelled_attributed,
                &sets.failed_reg_pending_final,
            ],
        )?;
        Self::partition(
            &sets.failed_registration_no_receivers,
            &[&sets.registration_failed_no_receivers],
        )?;
        Self::exact_intersection(
            &sets.registration_failed_no_receivers,
            &sets.registration_failed,
            &sets.send_no_receivers,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_cli_registry_lookup_failed,
            &sets.registration_failed,
            &sets.cli_registry_lookup_failed,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_cli_lagged_attributed,
            &sets.registration_failed,
            &sets.cli_lagged_attributed,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_cli_closed_attributed,
            &sets.registration_failed,
            &sets.cli_closed_attributed,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_cli_cancelled_attributed,
            &sets.registration_failed,
            &sets.cli_cancelled_attributed,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_pending_final,
            &sets.registration_failed,
            &sets.pending_delivery_final,
        )?;
        Self::exact_intersection(
            &sets.failed_reg_cli_received_lookup_succeeded,
            &sets.registration_failed,
            &sets.cli_received_lookup_succeeded,
        )?;
        let counters_match_sets = [
            (state.counters.advanced_with_snapshot, sets.advanced_with_snapshot.len()),
            (state.counters.registration_succeeded, sets.registration_succeeded.len()),
            (state.counters.registration_failed, sets.registration_failed.len()),
            (state.counters.send_published, sets.send_published.len()),
            (state.counters.send_no_receivers, sets.send_no_receivers.len()),
            (
                state.counters.cli_received_lookup_succeeded,
                sets.cli_received_lookup_succeeded.len(),
            ),
            (state.counters.cli_registry_lookup_failed, sets.cli_registry_lookup_failed.len()),
            (state.counters.cli_lagged_attributed, sets.cli_lagged_attributed.len()),
            (state.counters.cli_closed_attributed, sets.cli_closed_attributed.len()),
            (state.counters.cli_cancelled_attributed, sets.cli_cancelled_attributed.len()),
        ]
        .into_iter()
        .all(|(counter, cardinality)| u64::try_from(cardinality) == Ok(counter));
        if !counters_match_sets {
            return Err(PendingFinalSealErrorV2::SequenceSetMismatch);
        }
        let coverage_cursor_matches = u64::try_from(state.terminal_records.len())
            == Ok(state.next_coverage_sequence)
            && state
                .terminal_records
                .iter()
                .enumerate()
                .all(|(index, record)| u64::try_from(index) == Ok(record.coverage_sequence));
        if !coverage_cursor_matches {
            return Err(PendingFinalSealErrorV2::SequenceSetMismatch);
        }
        if was_poisoned || state.poisoned || !state.poisons.is_empty() {
            return Err(PendingFinalSealErrorV2::Poisoned);
        }
        if !sets.pending_delivery_final.is_empty()
            || !sets.failed_reg_pending_final.is_empty()
            || !sets.failed_reg_cli_received_lookup_succeeded.is_empty()
        {
            return Err(PendingFinalSealErrorV2::PendingDeliveryNotEmpty);
        }
        if !state.durability_pending.is_empty()
            || state.terminal_records.len() != state.durability_acked.len()
            || !state.durability_acked.iter().copied().eq(0..state.next_coverage_sequence)
        {
            return Err(PendingFinalSealErrorV2::DurabilityAckPending);
        }
        if !state.primary.is_empty()
            || !state.secondary.is_empty()
            || !state.published.is_empty()
            || !state.send_journal.is_empty()
            || state.unregistered_send_inflight != 0
        {
            return Err(PendingFinalSealErrorV2::RegistryNotEmpty);
        }
        Ok(())
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

    /// Appends an exact terminal and queue acceptance without removing identity state.
    pub fn terminalize_locked(
        &self,
        state: &mut PendingRegistryStateV2,
        sequence: u64,
        terminal: PendingCliTerminalV2,
    ) -> Result<(), PendingRegistryError> {
        let (metadata, registration, send) = {
            let Some(entry) = state.primary.get_mut(&sequence) else {
                state.poisoned = true;
                return Err(PendingRegistryError::MissingPrimaryEntry);
            };
            if entry.terminal.replace(terminal).is_some() {
                Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
                return Err(PendingRegistryError::DuplicateTerminal);
            }
            (
                entry.metadata,
                entry.registration,
                entry.send.ok_or(PendingRegistryError::MissingPrimaryEntry)?,
            )
        };
        if !state.sets.pending_delivery_final.remove(&sequence)
            && matches!(send, PendingSendDispositionV2::Published { .. })
        {
            Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
            return Err(PendingRegistryError::MissingPrimaryEntry);
        }
        if matches!(registration, PendingRegistrationDispositionV2::Failed(_))
            && matches!(send, PendingSendDispositionV2::Published { .. })
            && !state.sets.failed_reg_pending_final.remove(&sequence)
        {
            Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
            return Err(PendingRegistryError::MissingPrimaryEntry);
        }

        match terminal {
            PendingCliTerminalV2::CliReceivedLookupSucceeded
            | PendingCliTerminalV2::CliRegistryLookupFailed(_)
            | PendingCliTerminalV2::NoReceivers
            | PendingCliTerminalV2::RegistrationFailedNoReceivers => {}
            PendingCliTerminalV2::CliLagged => {
                Self::insert(state, sequence, |sets| &mut sets.cli_lagged_attributed);
                if matches!(registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_lagged_attributed
                    });
                }
            }
            PendingCliTerminalV2::CliClosed => {
                Self::insert(state, sequence, |sets| &mut sets.cli_closed_attributed);
                if matches!(registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_closed_attributed
                    });
                }
            }
            PendingCliTerminalV2::CliCancelled => {
                Self::insert(state, sequence, |sets| &mut sets.cli_cancelled_attributed);
                if matches!(registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_cancelled_attributed
                    });
                }
            }
        }
        let coverage_sequence = state.next_coverage_sequence;
        state.next_coverage_sequence = coverage_sequence.checked_add(1).ok_or_else(|| {
            Self::poison(state, PendingRegistryPoisonV2::SequenceOverflow);
            PendingRegistryError::CoverageSequenceOverflow
        })?;
        state.cleanup_events.push(PendingCleanupEventV2::TerminalAppended(sequence));
        state.terminal_records.push(PendingTerminalRecordV2 {
            coverage_sequence,
            metadata,
            registration,
            send,
            terminal,
        });
        state.durability_pending.push_back((coverage_sequence, sequence));
        state.cleanup_events.push(PendingCleanupEventV2::CoverageQueueAccepted(coverage_sequence));
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
        if sequences.iter().any(|sequence| {
            state
                .primary
                .get(sequence)
                .is_none_or(|entry| entry.terminal.is_some() || entry.send.is_none())
        }) {
            state.poisoned = true;
            return Err(PendingRegistryError::MissingPrimaryEntry);
        }
        for sequence in sequences {
            state.published.pop_front();
            self.terminalize_locked(state, sequence, terminal)?;
        }
        Ok(())
    }

    fn terminalize_send_journal_locked(
        &self,
        state: &mut PendingRegistryStateV2,
        count: usize,
        terminal: PendingCliTerminalV2,
    ) -> Result<usize, PendingRegistryError> {
        if state.send_journal.len() < count {
            state.poisoned = true;
            return Err(PendingRegistryError::PublishedRangeMismatch);
        }
        let entries: Vec<_> = state.send_journal.iter().take(count).copied().collect();
        let mut authority_count = 0usize;
        for entry in entries {
            state.send_journal.pop_front();
            if let PendingSendJournalEntryV2::AdvancedRegistration(sequence) = entry {
                if state.published.pop_front() != Some(sequence) {
                    Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
                    return Err(PendingRegistryError::PublishedRangeMismatch);
                }
                self.terminalize_locked(state, sequence, terminal)?;
                authority_count = authority_count
                    .checked_add(1)
                    .ok_or(PendingRegistryError::RangeLengthOverflow)?;
            }
        }
        Ok(authority_count)
    }
    fn increment(
        state: &mut PendingRegistryStateV2,
        field: PendingAccountingFieldV2,
    ) -> Result<(), PendingAccountingFieldV2> {
        Self::add_count(state, field, 1).map_err(|_| field)
    }

    fn add_count(
        state: &mut PendingRegistryStateV2,
        field: PendingAccountingFieldV2,
        amount: u64,
    ) -> Result<(), PendingRegistryError> {
        let current = match field {
            PendingAccountingFieldV2::AdvancedWithSnapshot => state.counters.advanced_with_snapshot,
            PendingAccountingFieldV2::RegistrationSucceeded => {
                state.counters.registration_succeeded
            }
            PendingAccountingFieldV2::RegistrationFailed => state.counters.registration_failed,
            PendingAccountingFieldV2::SendPublished => state.counters.send_published,
            PendingAccountingFieldV2::SendNoReceivers => state.counters.send_no_receivers,
            PendingAccountingFieldV2::CliReceivedLookupSucceeded => {
                state.counters.cli_received_lookup_succeeded
            }
            PendingAccountingFieldV2::CliRegistryLookupFailed => {
                state.counters.cli_registry_lookup_failed
            }
            PendingAccountingFieldV2::CliLaggedAttributed => state.counters.cli_lagged_attributed,
            PendingAccountingFieldV2::CliClosedAttributed => state.counters.cli_closed_attributed,
            PendingAccountingFieldV2::CliCancelledAttributed => {
                state.counters.cli_cancelled_attributed
            }
        };
        let Some(value) = current.checked_add(amount) else {
            Self::poison(state, PendingRegistryPoisonV2::AccountingOverflow(field));
            return Err(PendingRegistryError::AccountingOverflow(field));
        };
        match field {
            PendingAccountingFieldV2::AdvancedWithSnapshot => {
                state.counters.advanced_with_snapshot = value;
            }
            PendingAccountingFieldV2::RegistrationSucceeded => {
                state.counters.registration_succeeded = value;
            }
            PendingAccountingFieldV2::RegistrationFailed => {
                state.counters.registration_failed = value;
            }
            PendingAccountingFieldV2::SendPublished => state.counters.send_published = value,
            PendingAccountingFieldV2::SendNoReceivers => state.counters.send_no_receivers = value,
            PendingAccountingFieldV2::CliReceivedLookupSucceeded => {
                state.counters.cli_received_lookup_succeeded = value;
            }
            PendingAccountingFieldV2::CliRegistryLookupFailed => {
                state.counters.cli_registry_lookup_failed = value;
            }
            PendingAccountingFieldV2::CliLaggedAttributed => {
                state.counters.cli_lagged_attributed = value;
            }
            PendingAccountingFieldV2::CliClosedAttributed => {
                state.counters.cli_closed_attributed = value;
            }
            PendingAccountingFieldV2::CliCancelledAttributed => {
                state.counters.cli_cancelled_attributed = value;
            }
        }
        Ok(())
    }

    fn insert(
        state: &mut PendingRegistryStateV2,
        sequence: u64,
        select: impl FnOnce(&mut PendingRegistrySequenceSetsV2) -> &mut BTreeSet<u64>,
    ) {
        if !select(&mut state.sets).insert(sequence) {
            Self::poison(state, PendingRegistryPoisonV2::DuplicateSequence(sequence));
        }
    }

    fn poison(state: &mut PendingRegistryStateV2, poison: PendingRegistryPoisonV2) {
        state.poisoned = true;
        state.poisons.push(poison);
    }

    fn partition(
        whole: &BTreeSet<u64>,
        parts: &[&BTreeSet<u64>],
    ) -> Result<(), PendingFinalSealErrorV2> {
        let mut union = BTreeSet::new();
        for part in parts {
            for sequence in *part {
                if !union.insert(*sequence) {
                    return Err(PendingFinalSealErrorV2::SequenceSetOverlap);
                }
            }
        }
        if &union != whole {
            return Err(PendingFinalSealErrorV2::SequenceSetMismatch);
        }
        Ok(())
    }
    fn exact_intersection(
        recorded: &BTreeSet<u64>,
        left: &BTreeSet<u64>,
        right: &BTreeSet<u64>,
    ) -> Result<(), PendingFinalSealErrorV2> {
        let expected: BTreeSet<u64> = left.intersection(right).copied().collect();
        if &expected != recorded {
            return Err(PendingFinalSealErrorV2::SequenceSetMismatch);
        }
        Ok(())
    }
}

/// Read-only registry snapshot used by cutoff and focused conservation tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRegistrySnapshotV2 {
    /// H2 counters.
    pub counters: PendingRegistryCountersV2,
    /// Exact H2 sequence sets.
    pub sets: PendingRegistrySequenceSetsV2,
    /// Live primary entry count.
    pub primary_pending: usize,
    /// Live secondary sequence count.
    pub secondary_pending: usize,
    /// Published journal entries awaiting a CLI terminal.
    pub published_pending: usize,
    /// Exact every-send entries awaiting CLI consumption.
    pub send_journal: Vec<PendingSendJournalEntryV2>,
    /// Visible disposition of every non-authority send attempt.
    pub unregistered_send_records: Vec<(PendingSendJournalMarkerV2, PendingSendDispositionV2)>,
    /// Non-authority sends begun but not yet dispositioned.
    pub unregistered_send_inflight: u64,
    /// Coverage-queue records awaiting explicit durability acknowledgement.
    pub coverage_queue_pending_ack: usize,
    /// Explicit durable acknowledgements observed.
    pub durability_acked: usize,
    /// Terminal records accepted by the coverage queue.
    pub terminal_records: usize,
    /// Inclusive last pending snapshot sequence allocated by the registry.
    pub last_pending_snapshot_sequence: Option<u64>,
    /// Checked number of coverage records allocated in this epoch.
    pub coverage_count: u64,
    /// Inclusive last coverage sequence, absent for a clean empty epoch.
    pub last_coverage_sequence: Option<u64>,
    /// Whether the measurement epoch is poisoned.
    pub poisoned: bool,
}

/// Reviewed upper bound for the nonblocking source-record queue.
pub const EDGE_EVENT_QUEUE_CAPACITY_MAX_V1: usize = 65_536;
/// Reviewed upper bound for active source identities.
pub const EDGE_ACTIVE_STATE_CAPACITY_MAX_V1: usize = 65_536;
/// Exact Linux authority-clock implementation identifier.
pub const EDGE_CLOCK_SOURCE_VERSION_V1: &str = "linux-clock-gettime-realtime-monotonic/v1";
/// Exact periodic anchor cadence.
pub const EDGE_ANCHOR_CADENCE_NS_V1: u64 = 60_000_000_000;

const CLOCK_REALTIME_V1: c_int = 0;
const CLOCK_MONOTONIC_V1: c_int = 1;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct LinuxTimespecV1 {
    tv_sec: c_long,
    tv_nsec: c_long,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn clock_gettime(clock_id: c_int, timespec: *mut LinuxTimespecV1) -> c_int;
    fn clock_getres(clock_id: c_int, timespec: *mut LinuxTimespecV1) -> c_int;
    fn __errno_location() -> *mut c_int;
}

/// Validated owner-supplied recorder configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeMeasurementInstallConfigV1 {
    /// Nonzero producer epoch.
    pub producer_epoch: NonZeroU64,
    /// Bounded nonblocking event capacity.
    pub event_queue_capacity: usize,
    /// Bounded active identity capacity.
    pub active_state_capacity: usize,
    /// Bounded pending registry capacity.
    pub pending_registry_capacity: usize,
}

/// Named installation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMeasurementInstallErrorV1 {
    /// A capacity was zero.
    ZeroCapacity,
    /// A capacity exceeded its reviewed maximum.
    CapacityTooLarge,
    /// A different recorder was installed first.
    ConflictingInstall,
    /// `/proc/sys/kernel/random/boot_id` was unavailable or noncanonical.
    InvalidBootId,
    /// A clock resolution syscall failed.
    ClockResolutionFailed(ClockIdV1, ClockFailureV1),
}

/// Linux authority-clock identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockIdV1 {
    /// `CLOCK_REALTIME`.
    Realtime,
    /// `CLOCK_MONOTONIC`.
    Monotonic,
}

/// Named raw syscall failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockFailureV1 {
    /// Raw return status.
    pub status: c_int,
    /// Thread-local errno captured immediately after the call.
    pub errno: c_int,
}

/// Exact syscall status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockStatusV1 {
    /// Valid nonnegative timespec.
    Ok,
    /// Named failure evidence.
    Failed(ClockFailureV1),
}
impl ClockStatusV1 {
    /// Exact S2 wire label.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::Failed(_) => "Failed",
        }
    }
}

/// Route fixed at wire yield.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochRouteV1 {
    /// Admitted before cutoff.
    Authority,
    /// Explicit post-cutoff non-authority route.
    PostCutoffNonAuthority,
}

/// Admission token carried across decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochAdmissionTokenV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Checked wire ordinal.
    pub wire_ordinal: u64,
    /// Predecode clock and bytes observation.
    pub observation: WireObservationV1,
    /// Route fixed at yield.
    pub route: EpochRouteV1,
}

/// Durable lifecycle transition for one admitted websocket data sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireLifecycleTransitionV1 {
    /// The exact yielded bytes were sampled before any auxiliary record.
    WireObserved,
    /// The sampled bytes decoded and allocated a source generation.
    DecodeSucceeded {
        /// Checked generation allocated for the decoded payload.
        source_generation: u64,
    },
    /// Decode failed; this is a source terminal.
    DecodeRejected,
    /// The decoded value entered the actor mailbox.
    ActorEnqueueSucceeded,
    /// Actor mailbox enqueue failed; this is a source terminal.
    ActorEnqueueFailed,
    /// The actor received the value.
    ActorDelivered,
    /// The state queue accepted the value.
    StateHandoffSucceeded,
    /// The state queue rejected the value; this is a source terminal.
    StateHandoffFailed,
    /// The processor emitted the generation terminal.
    ProcessorTerminal,
    /// The generation entered cache ownership.
    CacheAwait {
        /// Exact nonterminal cache disposition.
        disposition: ProcessorBaseDispositionV1,
    },
    /// Cache ownership transferred atomically to processor.
    CacheClaimed,
    /// The CLI assigned the exact H2 terminal.
    CliTerminal {
        /// Pending snapshot sequence.
        pending_snapshot_sequence: u64,
        /// Exact terminal disposition and reason.
        terminal: PendingCliTerminalV2,
    },
    /// Excluded traffic decoded successfully.
    PostCutoffDecoded,
    /// Excluded decoded traffic entered the actor mailbox.
    PostCutoffActorEnqueued,
    /// The actor delivered excluded traffic.
    PostCutoffActorDelivered,
    /// The state queue accepted excluded traffic.
    PostCutoffStateHandedOff,
    /// The processor consumed excluded traffic without authority.
    PostCutoffProcessorExcluded,
    /// Traffic sampled after cutoff was explicitly excluded from authority.
    PostCutoffNonAuthority,
}

/// Exact independently hash-chained source-coverage record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCoverageRecordV3 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Contiguous checked coverage sequence.
    pub coverage_sequence: u64,
    /// Wire identity allocated at yield.
    pub wire_ordinal: u64,
    /// Authority generation, absent for decode rejection and excluded traffic.
    pub source_generation: Option<u64>,
    /// Route fixed by the admission fence.
    pub route: EpochRouteV1,
    /// Exact pipeline transition.
    pub transition: WireLifecycleTransitionV1,
    /// Previous coverage record hash.
    pub previous_record_hash: B256,
    /// SHA-256 hash of this record.
    pub record_hash: B256,
}

/// Bounded source record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSourceEventV1 {
    /// Payload-first record.
    PayloadFirst(PayloadFirstObservationV1),
    /// H3 record.
    Connection(SourceConnectionRecordV1),
    /// Clock anchor record.
    ClockAnchor(ClockAnchorRecordV1),
    /// Exhaustive processor product.
    Processor(ProcessorLifecycleProductV1),
    /// Independent exact source-coverage record.
    Coverage(SourceCoverageRecordV3),
    /// Cutoff record.
    Cutoff(ProducerEpochCutoffV1),
}

/// Nonblocking drain result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeEventDrainStatusV1 {
    /// One event.
    Event(Box<EdgeSourceEventV1>),
    /// No event currently available.
    Empty,
    /// Producer side closed.
    Closed,
}
/// Payload-first immutable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadFirstKeyV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Payload block number.
    pub block_number: u64,
    /// Eight-byte engine payload identifier.
    pub payload_id: [u8; 8],
}

/// One predecode Linux clock and wire observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireObservationV1 {
    /// Shared checked observation ordinal.
    pub clock_observation_ordinal: u64,
    /// Realtime syscall status.
    pub utc_status: ClockStatusV1,
    /// Realtime nanoseconds when successful.
    pub utc_ns: Option<u64>,
    /// Monotonic syscall status.
    pub mono_status: ClockStatusV1,
    /// Monotonic nanoseconds when successful.
    pub mono_ns: Option<u64>,
    /// SHA-256 of the exact yielded websocket bytes.
    pub wire_digest: B256,
}

/// Immutable index-zero payload-first authority record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadFirstObservationV1 {
    /// Payload-first key.
    pub key: PayloadFirstKeyV1,
    /// Source generation for the decoded index-zero frame.
    pub source_generation: u64,
    /// Predecode observation.
    pub observation: WireObservationV1,
    /// Canonical lowercase Linux boot identifier.
    pub boot_id: [u8; 36],
    /// Install-time realtime resolution.
    pub realtime_resolution_ns: u64,
    /// Install-time monotonic resolution.
    pub monotonic_resolution_ns: u64,
    /// Checked payload-first record sequence.
    pub record_sequence: u64,
    /// Previous payload-first record hash.
    pub previous_record_hash: B256,
    /// Ordinary SHA-256 S2 authority hash.
    pub record_hash: B256,
}
/// Exact startup/periodic S2 clock anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAnchorRecordV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Checked anchor sequence.
    pub anchor_sequence: u64,
    /// Shared clock observation.
    pub observation: WireObservationV1,
    /// Startup when true, periodic otherwise.
    pub startup: bool,
    /// Fixed-cadence due monotonic nanoseconds.
    pub due_mono_ns: u64,
    /// Actual sample monotonic nanoseconds.
    pub sampled_mono_ns: u64,
    /// Previous anchor hash.
    pub previous_anchor_hash: B256,
    /// Ordinary SHA-256 record hash.
    pub record_hash: B256,
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
    /// A websocket data message was yielded to decode.
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
    /// Cutoff closed an established interval.
    EstablishedClosedByCutoff,
    /// Owner shutdown closed an established interval.
    EstablishedClosedByShutdown,
    /// Authority cutoff was latched.
    AuthorityCutoffLatched,
    /// Owner shutdown was requested.
    OwnerShutdownRequested,
    /// Finite measurement connection observer exited; production transport continues.
    ConnectionTaskExited,
}
impl SourceConnectionTransitionV1 {
    /// Exact S2 wire label, deliberately independent of `Debug`.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::OwnerStart => "OwnerStart",
            Self::InitialConnectAttemptStarted => "InitialConnectAttemptStarted",
            Self::ConnectFailure => "ConnectFailure",
            Self::BackoffStarted => "BackoffStarted",
            Self::BackoffCompleted => "BackoffCompleted",
            Self::BackoffReconnectAttemptStarted => "BackoffReconnectAttemptStarted",
            Self::Established => "Established",
            Self::DataMessageYielded => "DataMessageYielded",
            Self::ControlPingReceived => "ControlPingReceived",
            Self::ControlPongReceived => "ControlPongReceived",
            Self::OutgoingPingDue => "OutgoingPingDue",
            Self::OutgoingPingWritten => "OutgoingPingWritten",
            Self::PongObserved => "PongObserved",
            Self::CloseFrameReceived => "CloseFrameReceived",
            Self::ReadError => "ReadError",
            Self::NoPongTimeout => "NoPongTimeout",
            Self::PingWriteFailure => "PingWriteFailure",
            Self::EstablishedClosedByClose => "EstablishedClosedByClose",
            Self::EstablishedClosedByReadError => "EstablishedClosedByReadError",
            Self::DirectReconnectAttemptStarted => "DirectReconnectAttemptStarted",
            Self::EstablishedClosedByNoPong => "EstablishedClosedByNoPong",
            Self::EstablishedClosedByPingWriteFailure => "EstablishedClosedByPingWriteFailure",
            Self::ReadHalfClosedWaitingForControl => "ReadHalfClosedWaitingForControl",
            Self::OutgoingPingWrittenWhileReadHalfClosed => {
                "OutgoingPingWrittenWhileReadHalfClosed"
            }
            Self::EstablishedClosedByCutoff => "EstablishedClosedByCutoff",
            Self::EstablishedClosedByShutdown => "EstablishedClosedByShutdown",
            Self::AuthorityCutoffLatched => "AuthorityCutoffLatched",
            Self::OwnerShutdownRequested => "OwnerShutdownRequested",
            Self::ConnectionTaskExited => "ConnectionTaskExited",
        }
    }
}

/// One hash-chained H3 connection record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceConnectionRecordV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Checked transition sequence.
    pub connection_sequence: u64,
    /// Shared clock observation ordinal when sampled.
    pub clock_observation_ordinal: Option<u64>,
    /// Actual monotonic transition bound.
    pub mono_ns: Option<u64>,
    /// Transition label.
    pub transition: SourceConnectionTransitionV1,
    /// Named error class, absent when not applicable.
    pub error_class: Option<SourceConnectionErrorClassV1>,
    /// Previous connection record hash.
    pub previous_record_hash: B256,
    /// Ordinary SHA-256 record hash.
    pub record_hash: B256,
}

/// Bounded H3 error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceConnectionErrorClassV1 {
    /// Connect transport failure.
    ConnectTransport,
    /// Established read transport failure.
    ReadTransport,
    /// No pong before the existing deadline.
    NoPong,
    /// Existing ping write failed.
    PingWrite,
    /// Source variant inventory drift.
    Unknown,
}
impl SourceConnectionErrorClassV1 {
    /// Exact S2 wire label.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::ConnectTransport => "ConnectTransport",
            Self::ReadTransport => "ReadTransport",
            Self::NoPong => "NoPong",
            Self::PingWrite => "PingWrite",
            Self::Unknown => "Unknown",
        }
    }
}

/// S2 processor base disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorBaseDispositionV1 {
    /// Initial base advanced.
    AdvancedInitialBase,
    /// Same-block successor advanced.
    AdvancedNextInSequence,
    /// First frame of the next block advanced.
    AdvancedFirstOfNextBlock,
    /// Advanced without a snapshot.
    AdvancedWithoutSnapshot,
    /// Exact duplicate.
    UnchangedDuplicateExact,
    /// Invalid new-block index.
    UnchangedInvalidNewBlockIndex,
    /// Sequence gap.
    UnchangedSequenceGap,
    /// Predecessor gap.
    UnchangedPredecessorGap,
    /// Awaiting canonical cache input.
    CachedAwaitCanonical,
    /// Awaiting cached predecessor.
    CachedAwaitPredecessor,
    /// Cache resolved to processor.
    CacheResolvedToProcessor,
    /// Cache replacement terminal.
    CacheReplacedOldGeneration,
    /// Cache eviction terminal.
    CacheEvicted,
    /// Cache rejection terminal.
    CacheRejectedAhead,
    /// Cache cutoff terminal.
    CachedUnresolvedAtCutoff,
    /// Missing-first terminal.
    MissingFirstUncacheable,
    /// Protocol error.
    ProcessErrorProtocol,
    /// Provider error.
    ProcessErrorProvider,
    /// Execution error.
    ProcessErrorExecution,
    /// Build error.
    ProcessErrorBuild,
    /// Source-branch inventory drift.
    UnknownProcessorBranch,
}
impl ProcessorBaseDispositionV1 {
    /// Exact S2 wire label, deliberately independent of `Debug`.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::AdvancedInitialBase => "AdvancedInitialBase",
            Self::AdvancedNextInSequence => "AdvancedNextInSequence",
            Self::AdvancedFirstOfNextBlock => "AdvancedFirstOfNextBlock",
            Self::AdvancedWithoutSnapshot => "AdvancedWithoutSnapshot",
            Self::UnchangedDuplicateExact => "UnchangedDuplicateExact",
            Self::UnchangedInvalidNewBlockIndex => "UnchangedInvalidNewBlockIndex",
            Self::UnchangedSequenceGap => "UnchangedSequenceGap",
            Self::UnchangedPredecessorGap => "UnchangedPredecessorGap",
            Self::CachedAwaitCanonical => "CachedAwaitCanonical",
            Self::CachedAwaitPredecessor => "CachedAwaitPredecessor",
            Self::CacheResolvedToProcessor => "CacheResolvedToProcessor",
            Self::CacheReplacedOldGeneration => "CacheReplacedOldGeneration",
            Self::CacheEvicted => "CacheEvicted",
            Self::CacheRejectedAhead => "CacheRejectedAhead",
            Self::CachedUnresolvedAtCutoff => "CachedUnresolvedAtCutoff",
            Self::MissingFirstUncacheable => "MissingFirstUncacheable",
            Self::ProcessErrorProtocol => "ProcessErrorProtocol",
            Self::ProcessErrorProvider => "ProcessErrorProvider",
            Self::ProcessErrorExecution => "ProcessErrorExecution",
            Self::ProcessErrorBuild => "ProcessErrorBuild",
            Self::UnknownProcessorBranch => "UnknownProcessorBranch",
        }
    }
}

/// Observer axis of the processor product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorObserverDispositionV1 {
    /// Observer absent.
    Absent,
    /// Observer returned.
    Delivered,
    /// Observer panicked and was caught.
    Panicked,
}
impl ProcessorObserverDispositionV1 {
    /// Exact S2 wire label.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Absent => "Absent",
            Self::Delivered => "Delivered",
            Self::Panicked => "Panicked",
        }
    }
}

/// Publish axis of the processor product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorPublishDispositionV1 {
    /// Publication does not apply.
    NotApplicable,
    /// Existing sender published to a positive receiver count.
    Published(u64),
    /// Existing sender reported no receivers.
    NoReceivers,
}
impl ProcessorPublishDispositionV1 {
    /// Exact S2 label and receiver count.
    pub const fn wire_parts(self) -> (&'static str, Option<u64>) {
        match self {
            Self::NotApplicable => ("NotApplicable", None),
            Self::Published(count) => ("Published", Some(count)),
            Self::NoReceivers => ("NoReceivers", None),
        }
    }
}

/// Inputs used to terminalize one admitted processor generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessorTerminalInputV1 {
    /// Source generation.
    pub source_generation: u64,
    /// Base disposition.
    pub base_disposition: ProcessorBaseDispositionV1,
    /// Observer disposition.
    pub observer_disposition: ProcessorObserverDispositionV1,
    /// Publish disposition.
    pub publish_disposition: ProcessorPublishDispositionV1,
    /// Pending snapshot sequence.
    pub pending_snapshot_sequence: Option<u64>,
    /// Exact processor error reason.
    pub processor_error_reason: Option<&'static str>,
    /// Nested cache-resolution final disposition.
    pub cache_resolved_final_disposition: Option<ProcessorBaseDispositionV1>,
}

/// Exact S2 processor lifecycle product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessorLifecycleProductV1 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Source generation.
    pub source_generation: u64,
    /// Base disposition.
    pub base_disposition: ProcessorBaseDispositionV1,
    /// Observer disposition.
    pub observer_disposition: ProcessorObserverDispositionV1,
    /// Publish disposition.
    pub publish_disposition: ProcessorPublishDispositionV1,
    /// Pending snapshot sequence.
    pub pending_snapshot_sequence: Option<u64>,
    /// Payload-first record hash.
    pub payload_first_record_hash: Option<B256>,
    /// Structural terminal hash.
    pub structural_terminal_hash: B256,
    /// Exact processor error reason.
    pub processor_error_reason: Option<&'static str>,
    /// Nested cache-resolution final disposition.
    pub cache_resolved_final_disposition: Option<ProcessorBaseDispositionV1>,
}

/// Atomic producer cutoff receipt with the exact S2 ten fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerEpochCutoffV1 {
    /// Producer epoch being closed.
    pub producer_epoch: u64,
    /// Last allocated clock observation ordinal.
    pub cutoff_clock_observation_ordinal: u64,
    /// Last admitted wire ordinal.
    pub last_admitted_wire_ordinal: u64,
    /// Last admitted source generation.
    pub last_admitted_source_generation: u64,
    /// Last admitted Blink generation.
    pub last_admitted_blink_generation: u64,
    /// Last allocated pending sequence.
    pub last_pending_snapshot_sequence: u64,
    /// Last coverage sequence.
    pub last_coverage_sequence: u64,
    /// Last candidate sequence.
    pub last_candidate_sequence: u64,
    /// Actual monotonic latch time.
    pub latch_mono_ns: u64,
    /// Ordinary SHA-256 S2 authority hash.
    pub record_hash: B256,
}

/// Coordinator-owned cutoff bounds from the other producer half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerExternalBoundsV1 {
    /// Last admitted Blink generation.
    pub last_admitted_blink_generation: u64,
    /// Last coverage sequence.
    pub last_coverage_sequence: u64,
    /// Last candidate sequence.
    pub last_candidate_sequence: u64,
}

/// Zero-drop final seal failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSourceFinalSealErrorV1 {
    /// Cutoff has not been latched.
    CutoffMissing,
    /// A named measurement poison was observed.
    Poisoned,
    /// Writer records remain unacknowledged.
    EventPending,
    /// Decoded or payload-first active state remains.
    ActiveStatePending,
    /// H1/H2 registry final failed.
    PendingRegistry(PendingFinalSealErrorV2),
    /// H3 did not end in a finite cutoff/shutdown/task-exit final.
    ConnectionFinalInvalid,
}

/// Named checked-accounting poison in the measurement recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMeasurementPoisonV1 {
    /// Checked shared ordinal overflow.
    ClockOrdinalOverflow,
    /// Checked wire ordinal overflow.
    WireOrdinalOverflow,
    /// Checked source generation overflow.
    SourceGenerationOverflow,
    /// Checked record sequence overflow.
    RecordSequenceOverflow,
    /// Active state exceeded its configured cap.
    ActiveStateCapacityOverflow,
    /// Bounded writer queue was full.
    EventQueueFull,
    /// Bounded writer queue was closed.
    EventQueueClosed,
    /// Payload-first binding conflicted.
    PayloadFirstBindingConflict,
    /// Index zero arrived after a higher index.
    PayloadIndexZeroLate,
    /// Cutoff found a payload missing index zero.
    PayloadIndexZeroMissing,
    /// Cutoff was requested before a named bound existed.
    CutoffBoundMissing(&'static str),
    /// A source generation reached a hook without its original identity.
    MissingSourceIdentity,
    /// More than one terminal product was attempted for a source generation.
    DuplicateProcessorTerminal,
    /// An observer panic was contained but poisons source health.
    ObserverPanicked,
    /// Checked terminal product count overflowed.
    ProcessorTerminalCountOverflow,
    /// The CLI coordinator observed a registry, writer, range, or task failure.
    CoordinatorFailure(&'static str),
    /// An authority wire failed decode, invalidating payload-first completeness.
    DecodeRejectedBeforePayloadAuthority,
    /// A wire lifecycle transition was missing, duplicated, or out of order.
    WireLifecycleConflict,
    /// H3 received a duplicate, impossible, or unhandled transition.
    ConnectionTransitionConflict,
    /// An admitted authority wire lacked a terminal at cutoff.
    WireLifecycleTerminalMissing,
    /// The cache owner did not drain every pre-cutoff generation before finalization.
    CacheDrainIncomplete,
}

#[derive(Debug, Clone, Copy)]
struct SourceGenerationContextV1 {
    structural_key: DecodedFlashblockKeyV1,
    payload_first_key: PayloadFirstKeyV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireLifecyclePhaseV1 {
    Observed,
    Decoded(u64),
    ActorEnqueued(u64),
    ActorDelivered(u64),
    StateHandedOff(u64),
    Terminal,
}

/// Recorder state for clock, payload-first bindings, and connection transitions.
#[derive(Debug)]
pub struct EdgeMeasurementRecorderStateV1 {
    /// Next shared clock ordinal.
    pub next_clock_ordinal: u64,
    /// Next wire ordinal.
    pub next_wire_ordinal: u64,
    /// Next decoded source generation.
    pub next_source_generation: u64,
    /// Next payload-first sequence.
    pub next_payload_first_sequence: u64,
    /// Last payload-first hash.
    pub last_payload_first_hash: B256,
    /// Next checked connection sequence.
    pub next_connection_sequence: u64,
    /// Next anchor sequence.
    pub next_anchor_sequence: u64,
    /// Last anchor hash.
    pub last_anchor_hash: B256,
    /// Next fixed-cadence due monotonic time.
    pub next_anchor_due_mono_ns: u64,
    /// First-write-wins payload bindings.
    pub payload_first: HashMap<PayloadFirstKeyV1, PayloadFirstObservationV1>,
    /// Payloads first seen above index zero.
    pub payload_without_index_zero: BTreeSet<PayloadFirstKeyV1>,
    /// FIFO source generations awaiting processor admission by structural key.
    pub decoded_source_generations: HashMap<DecodedFlashblockKeyV1, VecDeque<u64>>,
    source_generation_contexts: HashMap<u64, SourceGenerationContextV1>,
    cache_pending: BTreeMap<u64, ProcessorBaseDispositionV1>,
    processor_terminal_count: u64,
    snapshot_products: BTreeMap<u64, ProcessorLifecycleProductV1>,
    payload_first_by_generation: BTreeMap<u64, PayloadFirstObservationV1>,
    wire_lifecycle: BTreeMap<u64, WireLifecyclePhaseV1>,
    generation_wire_ordinals: BTreeMap<u64, u64>,
    authority_wire_terminals: u64,
    next_coverage_sequence: u64,
    next_post_cutoff_wire_ordinal: u64,
    last_coverage_hash: B256,
    coverage_record_count: u64,
    next_excluded_coverage_sequence: u64,
    last_excluded_coverage_hash: B256,
    excluded_coverage_record_count: u64,
    post_cutoff_routes: HashMap<DecodedFlashblockKeyV1, VecDeque<EpochAdmissionTokenV1>>,
    cutoff_fence: bool,
    connection_phase: ConnectionPhaseV1,
    connection_record_count: u64,
    connection_established_count: u64,
    connection_closed_count: u64,
    last_connection_record: Option<SourceConnectionRecordV1>,
    #[cfg(test)]
    /// Ordered bounded connection transitions.
    pub connection_records: VecDeque<SourceConnectionRecordV1>,
    /// Last connection hash.
    pub last_connection_hash: B256,
    /// Named poison observations.
    pub poisons: Vec<EdgeMeasurementPoisonV1>,
    /// Optional cutoff receipt.
    pub cutoff: Option<ProducerEpochCutoffV1>,
    /// Enqueued records awaiting durability acknowledgement.
    pub event_pending_ack: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionPhaseV1 {
    New,
    OwnerStarted,
    Connecting,
    AwaitingBackoff,
    Backoff,
    AwaitingBackoffReconnect,
    AwaitingEstablishedClose(u8),
    Established {
        read_half_closed: bool,
        ping_due: bool,
        ping_written: bool,
        control_pong_seen: bool,
    },
    AwaitingDirectReconnect,
    CutoffLatched,
    ShutdownRequested,
    Exited,
}

/// Process-wide recorder used without changing public broadcast or receiver signatures.
#[derive(Debug)]
pub struct EdgeMeasurementRecorderV1 {
    config: EdgeMeasurementInstallConfigV1,
    boot_id: [u8; 36],
    realtime_resolution_ns: u64,
    monotonic_resolution_ns: u64,
    state: Mutex<EdgeMeasurementRecorderStateV1>,
    registry: Arc<PendingMetadataRegistryV2>,
    event_sender: SyncSender<EdgeSourceEventV1>,
    event_receiver: Mutex<Receiver<EdgeSourceEventV1>>,
}

impl EdgeMeasurementRecorderV1 {
    /// Creates a recorder only from validated owner configuration.
    pub fn new(
        config: EdgeMeasurementInstallConfigV1,
    ) -> Result<Arc<Self>, EdgeMeasurementInstallErrorV1> {
        if config.event_queue_capacity == 0
            || config.active_state_capacity == 0
            || config.pending_registry_capacity == 0
        {
            return Err(EdgeMeasurementInstallErrorV1::ZeroCapacity);
        }
        if config.event_queue_capacity > EDGE_EVENT_QUEUE_CAPACITY_MAX_V1
            || config.active_state_capacity > EDGE_ACTIVE_STATE_CAPACITY_MAX_V1
            || config.pending_registry_capacity > PENDING_REGISTRY_CAPACITY_V2
        {
            return Err(EdgeMeasurementInstallErrorV1::CapacityTooLarge);
        }
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| EdgeMeasurementInstallErrorV1::InvalidBootId)?;
        let boot = boot.trim().as_bytes();
        if boot.len() != 36
            || !boot.iter().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
                }
            })
        {
            return Err(EdgeMeasurementInstallErrorV1::InvalidBootId);
        }
        let mut boot_id = [0; 36];
        boot_id.copy_from_slice(boot);
        let realtime_resolution_ns = Self::resolution(ClockIdV1::Realtime)?;
        let monotonic_resolution_ns = Self::resolution(ClockIdV1::Monotonic)?;
        let (event_sender, event_receiver) = sync_channel(config.event_queue_capacity);
        let recorder = Arc::new(Self {
            config,
            boot_id,
            realtime_resolution_ns,
            monotonic_resolution_ns,
            state: Mutex::new(EdgeMeasurementRecorderStateV1 {
                next_clock_ordinal: 0,
                next_wire_ordinal: 0,
                next_source_generation: 0,
                next_payload_first_sequence: 0,
                last_payload_first_hash: B256::ZERO,
                next_connection_sequence: 0,
                next_anchor_sequence: 0,
                last_anchor_hash: B256::ZERO,
                next_anchor_due_mono_ns: 0,
                payload_first: HashMap::new(),
                payload_without_index_zero: BTreeSet::new(),
                decoded_source_generations: HashMap::new(),
                source_generation_contexts: HashMap::new(),
                cache_pending: BTreeMap::new(),
                processor_terminal_count: 0,
                snapshot_products: BTreeMap::new(),
                payload_first_by_generation: BTreeMap::new(),
                wire_lifecycle: BTreeMap::new(),
                generation_wire_ordinals: BTreeMap::new(),
                authority_wire_terminals: 0,
                next_coverage_sequence: 0,
                next_post_cutoff_wire_ordinal: 0,
                last_coverage_hash: B256::ZERO,
                coverage_record_count: 0,
                next_excluded_coverage_sequence: 0,
                last_excluded_coverage_hash: B256::ZERO,
                excluded_coverage_record_count: 0,
                post_cutoff_routes: HashMap::new(),
                cutoff_fence: false,
                connection_phase: ConnectionPhaseV1::New,
                connection_record_count: 0,
                connection_established_count: 0,
                connection_closed_count: 0,
                last_connection_record: None,
                #[cfg(test)]
                connection_records: VecDeque::new(),
                last_connection_hash: B256::ZERO,
                poisons: Vec::new(),
                cutoff: None,
                event_pending_ack: 0,
            }),
            registry: Arc::new(PendingMetadataRegistryV2::new(
                config.producer_epoch.get(),
                config.pending_registry_capacity,
            )),
            event_sender,
            event_receiver: Mutex::new(event_receiver),
        });
        {
            let mut state = recorder.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            recorder.record_anchor_locked(&mut state, true);
        }
        Ok(recorder)
    }

    fn raw_clock(clock: ClockIdV1, resolution: bool) -> (ClockStatusV1, Option<u64>) {
        #[cfg(target_os = "linux")]
        {
            let mut timespec = LinuxTimespecV1::default();
            let id = match clock {
                ClockIdV1::Realtime => CLOCK_REALTIME_V1,
                ClockIdV1::Monotonic => CLOCK_MONOTONIC_V1,
            };
            // SAFETY: `timespec` is writable for the call and `id` is a Linux clock constant.
            let status = unsafe {
                if resolution {
                    clock_getres(id, &mut timespec)
                } else {
                    clock_gettime(id, &mut timespec)
                }
            };
            if status != 0 {
                // SAFETY: glibc returns a valid thread-local errno pointer for this thread.
                let errno = unsafe { *__errno_location() };
                return (ClockStatusV1::Failed(ClockFailureV1 { status, errno }), None);
            }
            let value = u64::try_from(timespec.tv_sec)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000_000_000))
                .and_then(|seconds| {
                    u64::try_from(timespec.tv_nsec).ok().and_then(|ns| seconds.checked_add(ns))
                });
            value.map_or(
                (ClockStatusV1::Failed(ClockFailureV1 { status: -1, errno: 0 }), None),
                |value| (ClockStatusV1::Ok, Some(value)),
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (clock, resolution);
            (ClockStatusV1::Failed(ClockFailureV1 { status: -1, errno: 0 }), None)
        }
    }

    fn resolution(clock: ClockIdV1) -> Result<u64, EdgeMeasurementInstallErrorV1> {
        match Self::raw_clock(clock, true) {
            (ClockStatusV1::Ok, Some(value)) if value > 0 => Ok(value),
            (ClockStatusV1::Failed(failure), _) => {
                Err(EdgeMeasurementInstallErrorV1::ClockResolutionFailed(clock, failure))
            }
            _ => Err(EdgeMeasurementInstallErrorV1::ClockResolutionFailed(
                clock,
                ClockFailureV1 { status: -1, errno: 0 },
            )),
        }
    }

    fn sample_locked(state: &mut EdgeMeasurementRecorderStateV1) -> Option<WireObservationV1> {
        let ordinal = state.next_clock_ordinal;
        state.next_clock_ordinal = match ordinal.checked_add(1) {
            Some(next) => next,
            None => {
                state.poisons.push(EdgeMeasurementPoisonV1::ClockOrdinalOverflow);
                return None;
            }
        };
        // Authority order is fixed: realtime is always called before monotonic.
        let (utc_status, utc_ns) = Self::raw_clock(ClockIdV1::Realtime, false);
        let (mono_status, mono_ns) = Self::raw_clock(ClockIdV1::Monotonic, false);
        Some(WireObservationV1 {
            clock_observation_ordinal: ordinal,
            utc_status,
            utc_ns,
            mono_status,
            mono_ns,
            wire_digest: B256::ZERO,
        })
    }

    fn record_anchor_locked(&self, state: &mut EdgeMeasurementRecorderStateV1, startup: bool) {
        let due_mono_ns = if startup { 0 } else { state.next_anchor_due_mono_ns };
        let Some(mut observation) = Self::sample_locked(state) else {
            return;
        };
        observation.wire_digest = B256::ZERO;
        let sampled_mono_ns = observation.mono_ns.unwrap_or(due_mono_ns);
        let due_mono_ns = if startup { sampled_mono_ns } else { due_mono_ns };
        let sequence = state.next_anchor_sequence;
        let Some(next_sequence) = sequence.checked_add(1) else {
            state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
            return;
        };
        let mut record = ClockAnchorRecordV1 {
            producer_epoch: self.config.producer_epoch.get(),
            anchor_sequence: sequence,
            observation,
            startup,
            due_mono_ns,
            sampled_mono_ns,
            previous_anchor_hash: state.last_anchor_hash,
            record_hash: B256::ZERO,
        };
        record.record_hash = AuthorityRecordHasherV1::clock_anchor(
            &record,
            self.boot_id,
            self.realtime_resolution_ns,
            self.monotonic_resolution_ns,
        );
        state.next_anchor_sequence = next_sequence;
        state.last_anchor_hash = record.record_hash;
        state.next_anchor_due_mono_ns =
            due_mono_ns.checked_add(EDGE_ANCHOR_CADENCE_NS_V1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                u64::MAX
            });
        self.enqueue(state, EdgeSourceEventV1::ClockAnchor(record));
    }
    fn enqueue(&self, state: &mut EdgeMeasurementRecorderStateV1, event: EdgeSourceEventV1) {
        match self.event_sender.try_send(event) {
            Ok(()) => {
                state.event_pending_ack =
                    state.event_pending_ack.checked_add(1).unwrap_or_else(|| {
                        state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                        u64::MAX
                    });
            }
            Err(TrySendError::Full(_)) => {
                state.poisons.push(EdgeMeasurementPoisonV1::EventQueueFull);
            }
            Err(TrySendError::Disconnected(_)) => {
                state.poisons.push(EdgeMeasurementPoisonV1::EventQueueClosed);
            }
        }
    }

    fn lifecycle_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        wire_ordinal: u64,
        source_generation: Option<u64>,
        transition: WireLifecycleTransitionV1,
    ) {
        self.coverage_locked(
            state,
            wire_ordinal,
            source_generation,
            EpochRouteV1::Authority,
            transition,
        );
    }

    const fn accounting_field_tag(field: PendingAccountingFieldV2) -> u8 {
        match field {
            PendingAccountingFieldV2::AdvancedWithSnapshot => 0,
            PendingAccountingFieldV2::RegistrationSucceeded => 1,
            PendingAccountingFieldV2::RegistrationFailed => 2,
            PendingAccountingFieldV2::SendPublished => 3,
            PendingAccountingFieldV2::SendNoReceivers => 4,
            PendingAccountingFieldV2::CliReceivedLookupSucceeded => 5,
            PendingAccountingFieldV2::CliRegistryLookupFailed => 6,
            PendingAccountingFieldV2::CliLaggedAttributed => 7,
            PendingAccountingFieldV2::CliClosedAttributed => 8,
            PendingAccountingFieldV2::CliCancelledAttributed => 9,
        }
    }

    fn push_registration_failure(bytes: &mut Vec<u8>, failure: PendingRegistrationFailure) {
        match failure {
            PendingRegistrationFailure::PendingSnapshotSequenceOverflow => bytes.push(0),
            PendingRegistrationFailure::PendingAccountingOverflow(field) => {
                bytes.extend_from_slice(&[1, Self::accounting_field_tag(field)]);
            }
            PendingRegistrationFailure::PendingRegistryCapacityOverflow => bytes.push(2),
            PendingRegistrationFailure::PendingRegistryLockPoisoned => bytes.push(3),
            PendingRegistrationFailure::PendingPointerBindingConflict => bytes.push(4),
            PendingRegistrationFailure::PendingArcIdentityExpired => bytes.push(5),
        }
    }

    fn push_lookup_failure(bytes: &mut Vec<u8>, failure: CliRegistryLookupFailureReason) {
        match failure {
            CliRegistryLookupFailureReason::NoPublishedSequence => bytes.push(0),
            CliRegistryLookupFailureReason::RegistrationFailed(reason) => {
                bytes.push(1);
                Self::push_registration_failure(bytes, reason);
            }
            CliRegistryLookupFailureReason::MissingPrimaryEntry => bytes.push(2),
            CliRegistryLookupFailureReason::PendingPointerBindingConflict => bytes.push(3),
            CliRegistryLookupFailureReason::PendingArcIdentityExpired => bytes.push(4),
            CliRegistryLookupFailureReason::PendingArcIdentityMismatch => bytes.push(5),
            CliRegistryLookupFailureReason::PendingPublicSubsetCorruption => bytes.push(6),
            CliRegistryLookupFailureReason::PassthroughNonAdvanced => bytes.push(7),
            CliRegistryLookupFailureReason::PostCutoffAdvancedNonAuthority => bytes.push(8),
            CliRegistryLookupFailureReason::PendingAccountingOverflow(field) => {
                bytes.extend_from_slice(&[9, Self::accounting_field_tag(field)]);
            }
        }
    }

    fn push_cli_terminal(bytes: &mut Vec<u8>, terminal: PendingCliTerminalV2) {
        match terminal {
            PendingCliTerminalV2::CliReceivedLookupSucceeded => bytes.push(0),
            PendingCliTerminalV2::CliRegistryLookupFailed(reason) => {
                bytes.push(1);
                Self::push_lookup_failure(bytes, reason);
            }
            PendingCliTerminalV2::CliLagged => bytes.push(2),
            PendingCliTerminalV2::CliClosed => bytes.push(3),
            PendingCliTerminalV2::CliCancelled => bytes.push(4),
            PendingCliTerminalV2::NoReceivers => bytes.push(5),
            PendingCliTerminalV2::RegistrationFailedNoReceivers => bytes.push(6),
        }
    }

    fn coverage_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        wire_ordinal: u64,
        source_generation: Option<u64>,
        route: EpochRouteV1,
        transition: WireLifecycleTransitionV1,
    ) {
        let (sequence, next, previous_record_hash) = match route {
            EpochRouteV1::Authority => (
                state.next_coverage_sequence,
                state.next_coverage_sequence.checked_add(1),
                state.last_coverage_hash,
            ),
            EpochRouteV1::PostCutoffNonAuthority => (
                state.next_excluded_coverage_sequence,
                state.next_excluded_coverage_sequence.checked_add(1),
                state.last_excluded_coverage_hash,
            ),
        };
        let Some(next) = next else {
            state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
            return;
        };
        let tag = match transition {
            WireLifecycleTransitionV1::WireObserved => 0,
            WireLifecycleTransitionV1::DecodeSucceeded { .. } => 1,
            WireLifecycleTransitionV1::DecodeRejected => 2,
            WireLifecycleTransitionV1::ActorEnqueueSucceeded => 3,
            WireLifecycleTransitionV1::ActorEnqueueFailed => 4,
            WireLifecycleTransitionV1::ActorDelivered => 5,
            WireLifecycleTransitionV1::StateHandoffSucceeded => 6,
            WireLifecycleTransitionV1::StateHandoffFailed => 7,
            WireLifecycleTransitionV1::ProcessorTerminal => 8,
            WireLifecycleTransitionV1::PostCutoffDecoded => 9,
            WireLifecycleTransitionV1::PostCutoffActorEnqueued => 10,
            WireLifecycleTransitionV1::PostCutoffActorDelivered => 11,
            WireLifecycleTransitionV1::PostCutoffStateHandedOff => 12,
            WireLifecycleTransitionV1::PostCutoffProcessorExcluded => 13,
            WireLifecycleTransitionV1::CacheAwait { .. } => 14,
            WireLifecycleTransitionV1::CacheClaimed => 15,
            WireLifecycleTransitionV1::CliTerminal { .. } => 16,
            WireLifecycleTransitionV1::PostCutoffNonAuthority => 17,
        };
        let mut bytes = Vec::with_capacity(96);
        bytes.extend_from_slice(b"base-edge-source-coverage-v3\0");
        bytes.extend_from_slice(&self.config.producer_epoch.get().to_be_bytes());
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&wire_ordinal.to_be_bytes());
        bytes.extend_from_slice(&source_generation.unwrap_or(u64::MAX).to_be_bytes());
        bytes.push(match route {
            EpochRouteV1::Authority => 0,
            EpochRouteV1::PostCutoffNonAuthority => 1,
        });
        bytes.push(tag);
        if let WireLifecycleTransitionV1::CacheAwait { disposition } = transition {
            bytes.extend_from_slice(disposition.wire_name().as_bytes());
        }
        if let WireLifecycleTransitionV1::CliTerminal { pending_snapshot_sequence, terminal } =
            transition
        {
            bytes.extend_from_slice(&pending_snapshot_sequence.to_be_bytes());
            Self::push_cli_terminal(&mut bytes, terminal);
        }
        bytes.extend_from_slice(previous_record_hash.as_slice());
        let record_hash = B256::from(DefaultCrypto.sha256(&bytes));
        let record = SourceCoverageRecordV3 {
            producer_epoch: self.config.producer_epoch.get(),
            coverage_sequence: sequence,
            wire_ordinal,
            source_generation,
            route,
            transition,
            previous_record_hash,
            record_hash,
        };
        match route {
            EpochRouteV1::Authority => {
                state.next_coverage_sequence = next;
                state.coverage_record_count =
                    state.coverage_record_count.checked_add(1).unwrap_or_else(|| {
                        state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                        u64::MAX
                    });
                state.last_coverage_hash = record_hash;
            }
            EpochRouteV1::PostCutoffNonAuthority => {
                state.next_excluded_coverage_sequence = next;
                state.excluded_coverage_record_count =
                    state.excluded_coverage_record_count.checked_add(1).unwrap_or_else(|| {
                        state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                        u64::MAX
                    });
                state.last_excluded_coverage_hash = record_hash;
            }
        }
        self.enqueue(state, EdgeSourceEventV1::Coverage(record));
    }

    fn terminalize_wire_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        wire_ordinal: u64,
        source_generation: Option<u64>,
        transition: WireLifecycleTransitionV1,
    ) {
        if !matches!(
            state.wire_lifecycle.get(&wire_ordinal),
            Some(phase) if *phase != WireLifecyclePhaseV1::Terminal
        ) {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return;
        }
        state.wire_lifecycle.remove(&wire_ordinal);
        if let Some(generation) = source_generation {
            state.generation_wire_ordinals.remove(&generation);
        }
        state.authority_wire_terminals =
            state.authority_wire_terminals.checked_add(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::ProcessorTerminalCountOverflow);
                u64::MAX
            });
        self.lifecycle_locked(state, wire_ordinal, source_generation, transition);
    }

    /// Installs the admission fence without sealing any owning ledger.
    pub fn prepare_cutoff(&self) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.cutoff_fence = true;
    }

    /// Returns whether every admitted authority route and registry terminal is durably drained.
    pub fn cutoff_drain_complete(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.wire_lifecycle.is_empty()
            && state.cache_pending.is_empty()
            && state.event_pending_ack == 0
            && self.registry.verify_final_seal().is_ok()
    }

    /// Returns the shared pending metadata registry.
    pub fn registry(&self) -> Arc<PendingMetadataRegistryV2> {
        Arc::clone(&self.registry)
    }

    /// Returns whether authority cutoff has been latched.
    pub fn cutoff_latched(&self) -> bool {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).cutoff_fence
    }

    /// Returns whether the owning source ledgers have published their final cutoff.
    pub fn cutoff_sealed(&self) -> bool {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).cutoff.is_some()
    }

    /// Classifies one upcoming production send without allowing post-cutoff authority mutation.
    pub fn prepare_pending_publication(
        &self,
        pending: &Arc<PendingBlocks>,
        advanced: bool,
        source_generation: Option<u64>,
    ) -> (Option<PendingRegistrationAttemptV2>, Option<PendingSendJournalMarkerV2>) {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let pre_cutoff_generation = source_generation
            .is_some_and(|generation| state.source_generation_contexts.contains_key(&generation));
        if advanced && (!state.cutoff_fence || pre_cutoff_generation) {
            let registration = self.registry.register(pending, source_generation);
            return (Some(registration), None);
        }
        let marker = if advanced {
            PendingSendJournalMarkerV2::PostCutoffAdvancedNonAuthority
        } else {
            PendingSendJournalMarkerV2::PassthroughNonAdvanced
        };
        self.registry.begin_unregistered_send();
        (None, Some(marker))
    }
    /// Canonical lowercase boot identifier bound at installation.
    pub const fn boot_id(&self) -> [u8; 36] {
        self.boot_id
    }

    /// Installation-time realtime clock resolution.
    pub const fn realtime_resolution_ns(&self) -> u64 {
        self.realtime_resolution_ns
    }

    /// Installation-time monotonic clock resolution.
    pub const fn monotonic_resolution_ns(&self) -> u64 {
        self.monotonic_resolution_ns
    }

    /// Records every fixed-cadence anchor due at the current Linux monotonic time.
    pub fn record_due_anchor(&self) {
        let (_, current_mono_ns) = Self::raw_clock(ClockIdV1::Monotonic, false);
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cutoff_fence {
            return;
        }
        if let Some(current_mono_ns) = current_mono_ns {
            while current_mono_ns >= state.next_anchor_due_mono_ns {
                let previous_due = state.next_anchor_due_mono_ns;
                self.record_anchor_locked(&mut state, false);
                if state.next_anchor_due_mono_ns == previous_due {
                    break;
                }
            }
        } else {
            self.record_anchor_locked(&mut state, false);
        }
    }

    /// Allocates an epoch admission and samples actual Linux clocks at wire yield.
    pub fn observe_wire(&self, bytes: &[u8]) -> Option<EpochAdmissionTokenV1> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cutoff_fence {
            let wire_ordinal = state.next_post_cutoff_wire_ordinal;
            state.next_post_cutoff_wire_ordinal =
                wire_ordinal.checked_add(1).unwrap_or_else(|| {
                    state.poisons.push(EdgeMeasurementPoisonV1::WireOrdinalOverflow);
                    u64::MAX
                });
            let (utc_status, utc_ns) = Self::raw_clock(ClockIdV1::Realtime, false);
            let (mono_status, mono_ns) = Self::raw_clock(ClockIdV1::Monotonic, false);
            let admission = EpochAdmissionTokenV1 {
                producer_epoch: self.config.producer_epoch.get(),
                wire_ordinal,
                observation: WireObservationV1 {
                    clock_observation_ordinal: state.next_clock_ordinal,
                    utc_status,
                    utc_ns,
                    mono_status,
                    mono_ns,
                    wire_digest: B256::from(DefaultCrypto.sha256(bytes)),
                },
                route: EpochRouteV1::PostCutoffNonAuthority,
            };
            self.coverage_locked(
                &mut state,
                wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::WireObserved,
            );
            return Some(admission);
        }
        let route = EpochRouteV1::Authority;
        let wire_ordinal = state.next_wire_ordinal;
        state.next_wire_ordinal = match wire_ordinal.checked_add(1) {
            Some(next) => next,
            None => {
                state.poisons.push(EdgeMeasurementPoisonV1::WireOrdinalOverflow);
                return None;
            }
        };
        let Some(mut observation) = Self::sample_locked(&mut state) else {
            state.wire_lifecycle.insert(wire_ordinal, WireLifecyclePhaseV1::Observed);
            self.lifecycle_locked(
                &mut state,
                wire_ordinal,
                None,
                WireLifecycleTransitionV1::WireObserved,
            );
            self.terminalize_wire_locked(
                &mut state,
                wire_ordinal,
                None,
                WireLifecycleTransitionV1::DecodeRejected,
            );
            return None;
        };
        observation.wire_digest = B256::from(DefaultCrypto.sha256(bytes));
        state.wire_lifecycle.insert(wire_ordinal, WireLifecyclePhaseV1::Observed);
        self.lifecycle_locked(
            &mut state,
            wire_ordinal,
            None,
            WireLifecycleTransitionV1::WireObserved,
        );
        while observation.mono_ns.is_some_and(|mono| mono >= state.next_anchor_due_mono_ns) {
            self.record_anchor_locked(&mut state, false);
        }
        Some(EpochAdmissionTokenV1 {
            producer_epoch: self.config.producer_epoch.get(),
            wire_ordinal,
            observation,
            route,
        })
    }

    /// Records an exact decode rejection terminal for sampled wire bytes.
    pub fn decode_rejected(&self, admission: EpochAdmissionTokenV1) {
        if admission.route != EpochRouteV1::Authority {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            self.coverage_locked(
                &mut state,
                admission.wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::PostCutoffNonAuthority,
            );
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.poisons.push(EdgeMeasurementPoisonV1::DecodeRejectedBeforePayloadAuthority);
        self.terminalize_wire_locked(
            &mut state,
            admission.wire_ordinal,
            None,
            WireLifecycleTransitionV1::DecodeRejected,
        );
    }

    /// Records successful decode without rechecking cutoff, preventing a wire/decode split.
    pub fn decoded_flashblock(
        &self,
        admission: EpochAdmissionTokenV1,
        flashblock: &Flashblock,
    ) -> Option<u64> {
        if admission.route != EpochRouteV1::Authority {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .post_cutoff_routes
                .entry(DecodedFlashblockKeyV1::from_flashblock(flashblock))
                .or_default()
                .push_back(admission);
            self.coverage_locked(
                &mut state,
                admission.wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::PostCutoffDecoded,
            );
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = state.next_source_generation;
        state.next_source_generation = match generation.checked_add(1) {
            Some(next) => next,
            None => {
                state.poisons.push(EdgeMeasurementPoisonV1::SourceGenerationOverflow);
                return None;
            }
        };
        if state.wire_lifecycle.get(&admission.wire_ordinal)
            != Some(&WireLifecyclePhaseV1::Observed)
        {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return None;
        }
        state
            .wire_lifecycle
            .insert(admission.wire_ordinal, WireLifecyclePhaseV1::Decoded(generation));
        state.generation_wire_ordinals.insert(generation, admission.wire_ordinal);
        self.lifecycle_locked(
            &mut state,
            admission.wire_ordinal,
            Some(generation),
            WireLifecycleTransitionV1::DecodeSucceeded { source_generation: generation },
        );
        let structural_key = DecodedFlashblockKeyV1::from_flashblock(flashblock);
        let pending: usize = state.decoded_source_generations.values().map(VecDeque::len).sum();
        if pending >= self.config.active_state_capacity {
            state.poisons.push(EdgeMeasurementPoisonV1::ActiveStateCapacityOverflow);
        } else {
            state
                .decoded_source_generations
                .entry(structural_key)
                .or_default()
                .push_back(generation);
            state.source_generation_contexts.insert(
                generation,
                SourceGenerationContextV1 {
                    structural_key,
                    payload_first_key: PayloadFirstKeyV1 {
                        producer_epoch: self.config.producer_epoch.get(),
                        block_number: flashblock.metadata.block_number,
                        payload_id: flashblock.payload_id.0.into(),
                    },
                },
            );
        }
        let key = PayloadFirstKeyV1 {
            producer_epoch: self.config.producer_epoch.get(),
            block_number: flashblock.metadata.block_number,
            payload_id: flashblock.payload_id.0.into(),
        };
        if flashblock.index == 0 {
            if state.payload_without_index_zero.remove(&key) {
                state.poisons.push(EdgeMeasurementPoisonV1::PayloadIndexZeroLate);
                return Some(generation);
            }
            let same = state.payload_first.get(&key).is_some_and(|existing| {
                existing.observation.wire_digest == admission.observation.wire_digest
                    && existing.boot_id == self.boot_id
            });
            if state.payload_first.contains_key(&key) {
                if !same {
                    state.poisons.push(EdgeMeasurementPoisonV1::PayloadFirstBindingConflict);
                }
                return Some(generation);
            }
            if state.payload_first.len() >= self.config.active_state_capacity {
                state.poisons.push(EdgeMeasurementPoisonV1::ActiveStateCapacityOverflow);
                return Some(generation);
            }
            let sequence = state.next_payload_first_sequence;
            let Some(next) = sequence.checked_add(1) else {
                state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                return Some(generation);
            };
            let previous_record_hash = state.last_payload_first_hash;
            let mut binding = PayloadFirstObservationV1 {
                key,
                source_generation: generation,
                observation: admission.observation,
                boot_id: self.boot_id,
                realtime_resolution_ns: self.realtime_resolution_ns,
                monotonic_resolution_ns: self.monotonic_resolution_ns,
                record_sequence: sequence,
                previous_record_hash,
                record_hash: B256::ZERO,
            };
            binding.record_hash = AuthorityRecordHasherV1::payload_first(&binding);
            state.next_payload_first_sequence = next;
            state.last_payload_first_hash = binding.record_hash;
            state.payload_first.insert(key, binding);
            self.enqueue(&mut state, EdgeSourceEventV1::PayloadFirst(binding));
        } else if !state.payload_first.contains_key(&key) {
            state.payload_without_index_zero.insert(key);
        }
        Some(generation)
    }

    /// Takes the earliest decoded generation for one processor-admitted flashblock.
    pub fn take_source_generation(&self, flashblock: &Flashblock) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = DecodedFlashblockKeyV1::from_flashblock(flashblock);
        let generation =
            state.decoded_source_generations.get_mut(&key).and_then(VecDeque::pop_front);
        if state.decoded_source_generations.get(&key).is_some_and(VecDeque::is_empty) {
            state.decoded_source_generations.remove(&key);
        }
        if let Some(generation) = generation {
            let valid =
                state.generation_wire_ordinals.get(&generation).is_some_and(|wire_ordinal| {
                    state.wire_lifecycle.get(wire_ordinal)
                        == Some(&WireLifecyclePhaseV1::StateHandedOff(generation))
                });
            if !valid {
                state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            }
            return Some(generation);
        }
        let excluded = state.post_cutoff_routes.get_mut(&key).and_then(VecDeque::pop_front);
        if state.post_cutoff_routes.get(&key).is_some_and(VecDeque::is_empty) {
            state.post_cutoff_routes.remove(&key);
        }
        if let Some(admission) = excluded {
            self.coverage_locked(
                &mut state,
                admission.wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::PostCutoffProcessorExcluded,
            );
        }
        None
    }

    /// Records a private excluded route entering or failing the actor mailbox.
    pub fn post_cutoff_actor_enqueue(&self, admission: EpochAdmissionTokenV1, succeeded: bool) {
        if admission.route != EpochRouteV1::PostCutoffNonAuthority {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.coverage_locked(
            &mut state,
            admission.wire_ordinal,
            None,
            admission.route,
            if succeeded {
                WireLifecycleTransitionV1::PostCutoffActorEnqueued
            } else {
                WireLifecycleTransitionV1::PostCutoffNonAuthority
            },
        );
    }

    /// Records private excluded-route actor delivery.
    pub fn post_cutoff_actor_delivered(&self, admission: EpochAdmissionTokenV1) {
        if admission.route != EpochRouteV1::PostCutoffNonAuthority {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.coverage_locked(
            &mut state,
            admission.wire_ordinal,
            None,
            admission.route,
            WireLifecycleTransitionV1::PostCutoffActorDelivered,
        );
    }
    /// Records whether the decoded source generation entered the actor mailbox.
    pub fn actor_enqueue(&self, source_generation: u64, succeeded: bool) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(wire_ordinal) = state.generation_wire_ordinals.get(&source_generation).copied()
        else {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingSourceIdentity);
            return;
        };
        if state.wire_lifecycle.get(&wire_ordinal)
            != Some(&WireLifecyclePhaseV1::Decoded(source_generation))
        {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return;
        }
        if succeeded {
            state
                .wire_lifecycle
                .insert(wire_ordinal, WireLifecyclePhaseV1::ActorEnqueued(source_generation));
            self.lifecycle_locked(
                &mut state,
                wire_ordinal,
                Some(source_generation),
                WireLifecycleTransitionV1::ActorEnqueueSucceeded,
            );
        } else {
            self.remove_generation_locked(&mut state, source_generation);
            self.terminalize_wire_locked(
                &mut state,
                wire_ordinal,
                Some(source_generation),
                WireLifecycleTransitionV1::ActorEnqueueFailed,
            );
        }
    }

    /// Records actor receipt before the existing receiver handoff.
    pub fn actor_delivered(&self, source_generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(wire_ordinal) = state.generation_wire_ordinals.get(&source_generation).copied()
        else {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingSourceIdentity);
            return;
        };
        if state.wire_lifecycle.get(&wire_ordinal)
            != Some(&WireLifecyclePhaseV1::ActorEnqueued(source_generation))
        {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return;
        }
        state
            .wire_lifecycle
            .insert(wire_ordinal, WireLifecyclePhaseV1::ActorDelivered(source_generation));
        self.lifecycle_locked(
            &mut state,
            wire_ordinal,
            Some(source_generation),
            WireLifecycleTransitionV1::ActorDelivered,
        );
    }

    /// Reserves the FIFO structural identity before exposing the state-queue item.
    pub fn begin_state_handoff(&self, key: DecodedFlashblockKeyV1) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = state.decoded_source_generations.get(&key).and_then(|generations| {
            generations.iter().copied().find(|generation| {
                state.generation_wire_ordinals.get(generation).is_some_and(|wire_ordinal| {
                    state.wire_lifecycle.get(wire_ordinal)
                        == Some(&WireLifecyclePhaseV1::ActorDelivered(*generation))
                })
            })
        });
        if let Some(generation) = generation
            && let Some(wire_ordinal) = state.generation_wire_ordinals.get(&generation).copied()
        {
            state
                .wire_lifecycle
                .insert(wire_ordinal, WireLifecyclePhaseV1::StateHandedOff(generation));
            self.lifecycle_locked(
                &mut state,
                wire_ordinal,
                Some(generation),
                WireLifecycleTransitionV1::StateHandoffSucceeded,
            );
            return Some(generation);
        }
        if let Some(admission) =
            state.post_cutoff_routes.get(&key).and_then(|routes| routes.front()).copied()
        {
            self.coverage_locked(
                &mut state,
                admission.wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::PostCutoffStateHandedOff,
            );
        }
        None
    }

    /// Terminalizes a private excluded route when the unchanged state send fails.
    pub fn post_cutoff_state_handoff_failed(&self, key: DecodedFlashblockKeyV1) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let admission = state.post_cutoff_routes.get_mut(&key).and_then(VecDeque::pop_front);
        if state.post_cutoff_routes.get(&key).is_some_and(VecDeque::is_empty) {
            state.post_cutoff_routes.remove(&key);
        }
        if let Some(admission) = admission {
            self.coverage_locked(
                &mut state,
                admission.wire_ordinal,
                None,
                admission.route,
                WireLifecycleTransitionV1::PostCutoffNonAuthority,
            );
        }
    }
    /// Rolls back a reserved state handoff when the unchanged queue send fails.
    pub fn state_handoff_failed(&self, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(wire_ordinal) = state.generation_wire_ordinals.get(&generation).copied() else {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingSourceIdentity);
            return;
        };
        if state.wire_lifecycle.get(&wire_ordinal)
            != Some(&WireLifecyclePhaseV1::StateHandedOff(generation))
        {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return;
        }
        self.remove_generation_locked(&mut state, generation);
        self.terminalize_wire_locked(
            &mut state,
            wire_ordinal,
            Some(generation),
            WireLifecycleTransitionV1::StateHandoffFailed,
        );
    }

    fn remove_generation_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        source_generation: u64,
    ) {
        if let Some(context) = state.source_generation_contexts.remove(&source_generation)
            && let Some(generations) =
                state.decoded_source_generations.get_mut(&context.structural_key)
        {
            generations.retain(|generation| *generation != source_generation);
            if generations.is_empty() {
                state.decoded_source_generations.remove(&context.structural_key);
            }
        }
        state.cache_pending.remove(&source_generation);
    }

    /// Records a nonterminal cache wait without consuming the generation.
    pub fn observe_cache_wait(
        &self,
        source_generation: u64,
        disposition: ProcessorBaseDispositionV1,
    ) {
        debug_assert!(matches!(
            disposition,
            ProcessorBaseDispositionV1::CachedAwaitCanonical
                | ProcessorBaseDispositionV1::CachedAwaitPredecessor
        ));
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.source_generation_contexts.contains_key(&source_generation) {
            state.cache_pending.insert(source_generation, disposition);
            if let Some(wire_ordinal) =
                state.generation_wire_ordinals.get(&source_generation).copied()
            {
                self.lifecycle_locked(
                    &mut state,
                    wire_ordinal,
                    Some(source_generation),
                    WireLifecycleTransitionV1::CacheAwait { disposition },
                );
            }
        } else {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        }
    }

    /// Atomically transfers one unresolved cache generation to processor ownership.
    pub fn claim_cache_resolution(&self, source_generation: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let claimed = state.cache_pending.remove(&source_generation).is_some();
        if claimed
            && let Some(wire_ordinal) =
                state.generation_wire_ordinals.get(&source_generation).copied()
        {
            self.lifecycle_locked(
                &mut state,
                wire_ordinal,
                Some(source_generation),
                WireLifecycleTransitionV1::CacheClaimed,
            );
        }
        claimed
    }

    fn structural_terminal_hash(
        context: SourceGenerationContextV1,
        source_generation: u64,
        base_disposition: ProcessorBaseDispositionV1,
        processor_error_reason: Option<&'static str>,
        cache_resolved_final_disposition: Option<ProcessorBaseDispositionV1>,
    ) -> B256 {
        const DOMAIN: &[u8] = b"base-edge-processor-terminal-v1\0";
        let mut bytes = Vec::with_capacity(DOMAIN.len() + 8 * 4 + 8 + 64);
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&source_generation.to_be_bytes());
        bytes.extend_from_slice(&context.structural_key.block_number.to_be_bytes());
        bytes.extend_from_slice(&context.structural_key.payload_id);
        bytes.extend_from_slice(&context.structural_key.flashblock_index.to_be_bytes());
        bytes.extend_from_slice(format!("{base_disposition:?}").as_bytes());
        if let Some(reason) = processor_error_reason {
            bytes.extend_from_slice(reason.as_bytes());
        }
        if let Some(disposition) = cache_resolved_final_disposition {
            bytes.extend_from_slice(format!("{disposition:?}").as_bytes());
        }
        B256::from(DefaultCrypto.sha256(&bytes))
    }

    /// Records one terminal product for an admitted source generation.
    pub(crate) fn record_generation_product(&self, input: ProcessorTerminalInputV1) {
        let ProcessorTerminalInputV1 {
            source_generation,
            base_disposition,
            observer_disposition,
            publish_disposition,
            pending_snapshot_sequence,
            processor_error_reason,
            cache_resolved_final_disposition,
        } = input;
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if observer_disposition == ProcessorObserverDispositionV1::Panicked {
            state.poisons.push(EdgeMeasurementPoisonV1::ObserverPanicked);
        }
        let wire_ordinal = state.generation_wire_ordinals.get(&source_generation).copied();
        let Some(context) = state.source_generation_contexts.remove(&source_generation) else {
            state.poisons.push(EdgeMeasurementPoisonV1::DuplicateProcessorTerminal);
            return;
        };
        state.cache_pending.remove(&source_generation);
        if let Some(next) = state.processor_terminal_count.checked_add(1) {
            state.processor_terminal_count = next;
        } else {
            state.poisons.push(EdgeMeasurementPoisonV1::ProcessorTerminalCountOverflow);
        }
        let product = ProcessorLifecycleProductV1 {
            producer_epoch: self.config.producer_epoch.get(),
            source_generation,
            base_disposition,
            observer_disposition,
            publish_disposition,
            pending_snapshot_sequence,
            payload_first_record_hash: state
                .payload_first
                .get(&context.payload_first_key)
                .map(|record| record.record_hash),
            structural_terminal_hash: Self::structural_terminal_hash(
                context,
                source_generation,
                base_disposition,
                processor_error_reason,
                cache_resolved_final_disposition,
            ),
            processor_error_reason,
            cache_resolved_final_disposition,
        };
        if let Some(sequence) = product.pending_snapshot_sequence {
            let payload_first = state.payload_first.get(&context.payload_first_key).copied();
            if state.snapshot_products.len() >= self.config.active_state_capacity
                || state.payload_first_by_generation.len() >= self.config.active_state_capacity
                || payload_first.is_none()
            {
                state.poisons.push(EdgeMeasurementPoisonV1::ActiveStateCapacityOverflow);
            } else if let Some(payload_first) = payload_first {
                state.payload_first_by_generation.insert(source_generation, payload_first);
                state.snapshot_products.insert(sequence, product);
            }
        }
        state.payload_first.remove(&context.payload_first_key);
        self.enqueue(&mut state, EdgeSourceEventV1::Processor(product));
        if let Some(wire_ordinal) = wire_ordinal {
            self.terminalize_wire_locked(
                &mut state,
                wire_ordinal,
                Some(source_generation),
                WireLifecycleTransitionV1::ProcessorTerminal,
            );
        } else {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        }
    }

    const fn advance_connection_phase(
        phase: ConnectionPhaseV1,
        transition: SourceConnectionTransitionV1,
    ) -> Option<ConnectionPhaseV1> {
        match (phase, transition) {
            (Phase::New, Transition::OwnerStart) => Some(Phase::OwnerStarted),
            (Phase::OwnerStarted, Transition::InitialConnectAttemptStarted)
            | (Phase::AwaitingDirectReconnect, Transition::DirectReconnectAttemptStarted)
            | (Phase::AwaitingBackoffReconnect, Transition::BackoffReconnectAttemptStarted) => {
                Some(Phase::Connecting)
            }
            (Phase::AwaitingBackoff, Transition::BackoffStarted) => Some(Phase::Backoff),
            (Phase::Backoff, Transition::BackoffCompleted) => Some(Phase::AwaitingBackoffReconnect),
            (Phase::Connecting, Transition::Established) => Some(Phase::Established {
                read_half_closed: false,
                ping_due: false,
                ping_written: false,
                control_pong_seen: false,
            }),
            (
                phase @ Phase::Established { .. },
                Transition::DataMessageYielded | Transition::ControlPingReceived,
            )
            | (
                phase @ Phase::Established { ping_due: true, ping_written: true, .. },
                Transition::OutgoingPingDue,
            ) => Some(phase),
            (
                Phase::Established {
                    read_half_closed,
                    ping_due: true,
                    ping_written,
                    control_pong_seen: _,
                },
                Transition::ControlPongReceived,
            ) => Some(Phase::Established {
                read_half_closed,
                ping_due: true,
                ping_written,
                control_pong_seen: true,
            }),
            (
                Phase::Established {
                    read_half_closed,
                    ping_due: false,
                    ping_written: false,
                    control_pong_seen: false,
                },
                Transition::OutgoingPingDue,
            ) => Some(Phase::Established {
                read_half_closed,
                ping_due: true,
                ping_written: false,
                control_pong_seen: false,
            }),
            (
                Phase::Established {
                    read_half_closed,
                    ping_due: true,
                    ping_written: false,
                    control_pong_seen,
                },
                Transition::OutgoingPingWritten,
            ) => Some(Phase::Established {
                read_half_closed,
                ping_due: true,
                ping_written: true,
                control_pong_seen,
            }),
            (
                Phase::Established {
                    read_half_closed: true,
                    ping_due: true,
                    ping_written: true,
                    control_pong_seen,
                },
                Transition::OutgoingPingWrittenWhileReadHalfClosed,
            ) => Some(Phase::Established {
                read_half_closed: true,
                ping_due: true,
                ping_written: true,
                control_pong_seen,
            }),
            (
                Phase::Established {
                    read_half_closed,
                    ping_due: true,
                    ping_written: true,
                    control_pong_seen: true,
                },
                Transition::PongObserved,
            ) => Some(Phase::Established {
                read_half_closed,
                ping_due: false,
                ping_written: false,
                control_pong_seen: false,
            }),
            (
                Phase::Established {
                    read_half_closed: false,
                    ping_due,
                    ping_written,
                    control_pong_seen,
                },
                Transition::ReadHalfClosedWaitingForControl,
            ) => Some(Phase::Established {
                read_half_closed: true,
                ping_due,
                ping_written,
                control_pong_seen,
            }),
            (Phase::Established { .. }, Transition::CloseFrameReceived) => {
                Some(Phase::AwaitingEstablishedClose(0))
            }
            (Phase::Established { .. }, Transition::ReadError) => {
                Some(Phase::AwaitingEstablishedClose(1))
            }
            (Phase::Established { ping_due: true, .. }, Transition::NoPongTimeout) => {
                Some(Phase::AwaitingEstablishedClose(2))
            }
            (Phase::Established { ping_due: true, .. }, Transition::PingWriteFailure) => {
                Some(Phase::AwaitingEstablishedClose(3))
            }
            (Phase::AwaitingEstablishedClose(0), Transition::EstablishedClosedByClose)
            | (Phase::AwaitingEstablishedClose(1), Transition::EstablishedClosedByReadError) => {
                Some(Phase::AwaitingDirectReconnect)
            }
            (Phase::Connecting, Transition::ConnectFailure)
            | (Phase::AwaitingEstablishedClose(2), Transition::EstablishedClosedByNoPong)
            | (
                Phase::AwaitingEstablishedClose(3),
                Transition::EstablishedClosedByPingWriteFailure,
            ) => Some(Phase::AwaitingBackoff),
            (
                Phase::Established { .. } | Phase::AwaitingEstablishedClose(_),
                Transition::EstablishedClosedByCutoff,
            ) => Some(Phase::AwaitingDirectReconnect),
            (Phase::Established { .. }, Transition::EstablishedClosedByShutdown) => {
                Some(Phase::ShutdownRequested)
            }
            (
                Phase::New
                | Phase::OwnerStarted
                | Phase::Connecting
                | Phase::AwaitingBackoff
                | Phase::Backoff
                | Phase::AwaitingBackoffReconnect
                | Phase::AwaitingDirectReconnect,
                Transition::AuthorityCutoffLatched,
            ) => Some(Phase::CutoffLatched),
            (Phase::CutoffLatched, Transition::ConnectionTaskExited)
            | (Phase::ShutdownRequested, Transition::ConnectionTaskExited) => Some(Phase::Exited),
            (
                Phase::OwnerStarted
                | Phase::Connecting
                | Phase::AwaitingBackoff
                | Phase::Backoff
                | Phase::AwaitingBackoffReconnect
                | Phase::AwaitingDirectReconnect,
                Transition::OwnerShutdownRequested,
            ) => Some(Phase::ShutdownRequested),
            _ => None,
        }
    }
    fn connection_transition_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        transition: SourceConnectionTransitionV1,
    ) {
        let Some(next_phase) = Self::advance_connection_phase(state.connection_phase, transition)
        else {
            state.poisons.push(EdgeMeasurementPoisonV1::ConnectionTransitionConflict);
            return;
        };
        state.connection_phase = next_phase;
        let error_class = match transition {
            SourceConnectionTransitionV1::ConnectFailure => {
                Some(SourceConnectionErrorClassV1::ConnectTransport)
            }
            SourceConnectionTransitionV1::ReadError => {
                Some(SourceConnectionErrorClassV1::ReadTransport)
            }
            SourceConnectionTransitionV1::NoPongTimeout => {
                Some(SourceConnectionErrorClassV1::NoPong)
            }
            SourceConnectionTransitionV1::PingWriteFailure => {
                Some(SourceConnectionErrorClassV1::PingWrite)
            }
            _ => None,
        };
        let sequence = state.next_connection_sequence;
        state.next_connection_sequence = match sequence.checked_add(1) {
            Some(next) => next,
            None => {
                state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                return;
            }
        };
        let sample = Self::sample_locked(state);
        let previous_record_hash = state.last_connection_hash;
        let mut record = SourceConnectionRecordV1 {
            producer_epoch: self.config.producer_epoch.get(),
            connection_sequence: sequence,
            clock_observation_ordinal: sample.map(|value| value.clock_observation_ordinal),
            mono_ns: sample.and_then(|value| value.mono_ns),
            transition,
            error_class,
            previous_record_hash,
            record_hash: B256::ZERO,
        };
        record.record_hash = AuthorityRecordHasherV1::connection(&record);
        state.last_connection_hash = record.record_hash;
        state.connection_record_count =
            state.connection_record_count.checked_add(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                u64::MAX
            });
        if transition == SourceConnectionTransitionV1::Established {
            state.connection_established_count =
                state.connection_established_count.checked_add(1).unwrap_or_else(|| {
                    state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                    u64::MAX
                });
        }
        if matches!(
            transition,
            SourceConnectionTransitionV1::EstablishedClosedByClose
                | SourceConnectionTransitionV1::EstablishedClosedByReadError
                | SourceConnectionTransitionV1::EstablishedClosedByNoPong
                | SourceConnectionTransitionV1::EstablishedClosedByPingWriteFailure
                | SourceConnectionTransitionV1::EstablishedClosedByCutoff
                | SourceConnectionTransitionV1::EstablishedClosedByShutdown
        ) {
            state.connection_closed_count =
                state.connection_closed_count.checked_add(1).unwrap_or_else(|| {
                    state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                    u64::MAX
                });
        }
        state.last_connection_record = Some(record);
        #[cfg(test)]
        state.connection_records.push_back(record);
        self.enqueue(state, EdgeSourceEventV1::Connection(record));
    }

    /// Appends one source-faithful H3 transition while measurement authority remains open.
    pub fn connection_transition(&self, transition: SourceConnectionTransitionV1) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cutoff_fence {
            return;
        }
        self.connection_transition_locked(&mut state, transition);
    }

    /// Atomically latches the exact S2 cutoff once.
    pub fn latch_cutoff(&self, external: ProducerExternalBoundsV1) -> ProducerEpochCutoffV1 {
        self.prepare_cutoff();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cutoff) = state.cutoff {
            return cutoff;
        }
        let sample = Self::sample_locked(&mut state);
        while sample
            .and_then(|observation| observation.mono_ns)
            .is_some_and(|mono| mono >= state.next_anchor_due_mono_ns)
        {
            self.record_anchor_locked(&mut state, false);
        }
        let cutoff_clock_observation_ordinal =
            state.next_clock_ordinal.checked_sub(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::CutoffBoundMissing("clock"));
                0
            });
        let last_admitted_wire_ordinal =
            state.next_wire_ordinal.checked_sub(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::CutoffBoundMissing("wire"));
                0
            });
        let last_admitted_source_generation =
            state.next_source_generation.checked_sub(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::CutoffBoundMissing("source"));
                0
            });
        let mut cutoff = ProducerEpochCutoffV1 {
            producer_epoch: self.config.producer_epoch.get(),
            cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal,
            last_admitted_source_generation,
            last_admitted_blink_generation: external.last_admitted_blink_generation,
            last_pending_snapshot_sequence: self
                .registry
                .snapshot()
                .last_pending_snapshot_sequence
                .unwrap_or(0),
            last_coverage_sequence: state.next_coverage_sequence.saturating_sub(1),
            last_candidate_sequence: external.last_candidate_sequence,
            latch_mono_ns: sample.and_then(|value| value.mono_ns).unwrap_or(0),
            record_hash: B256::ZERO,
        };
        cutoff.record_hash = AuthorityRecordHasherV1::cutoff(&cutoff);
        state.cutoff = Some(cutoff);
        if !state.payload_without_index_zero.is_empty() {
            state.poisons.push(EdgeMeasurementPoisonV1::PayloadIndexZeroMissing);
        }
        if !state.cache_pending.is_empty() {
            state.poisons.push(EdgeMeasurementPoisonV1::CacheDrainIncomplete);
        }
        if state.connection_established_count > state.connection_closed_count {
            self.connection_transition_locked(
                &mut state,
                SourceConnectionTransitionV1::EstablishedClosedByCutoff,
            );
        }
        self.connection_transition_locked(
            &mut state,
            SourceConnectionTransitionV1::AuthorityCutoffLatched,
        );
        self.connection_transition_locked(
            &mut state,
            SourceConnectionTransitionV1::ConnectionTaskExited,
        );
        self.enqueue(&mut state, EdgeSourceEventV1::Cutoff(cutoff));
        cutoff
    }
    /// Streams the exact CLI terminal into the independent V3 coverage chain.
    pub fn record_cli_terminal_coverage(&self, terminal: PendingTerminalRecordV2) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.coverage_locked(
            &mut state,
            u64::MAX,
            terminal.metadata.source_generation,
            EpochRouteV1::Authority,
            WireLifecycleTransitionV1::CliTerminal {
                pending_snapshot_sequence: terminal.metadata.identity.pending_snapshot_sequence,
                terminal: terminal.terminal,
            },
        );
    }
    /// Records exactly one processor product through the bounded sink.
    pub fn record_processor_product(&self, product: ProcessorLifecycleProductV1) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.enqueue(&mut state, EdgeSourceEventV1::Processor(product));
    }

    /// Returns one source record without blocking.
    pub fn try_recv_event(&self) -> EdgeEventDrainStatusV1 {
        match self.event_receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).try_recv()
        {
            Ok(event) => EdgeEventDrainStatusV1::Event(Box::new(event)),
            Err(TryRecvError::Empty) => EdgeEventDrainStatusV1::Empty,
            Err(TryRecvError::Disconnected) => EdgeEventDrainStatusV1::Closed,
        }
    }

    /// Acknowledges exactly one record after durable persistence.
    pub fn ack_event_durable(&self) -> Result<(), EdgeMeasurementPoisonV1> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.event_pending_ack = state
            .event_pending_ack
            .checked_sub(1)
            .ok_or(EdgeMeasurementPoisonV1::EventQueueClosed)?;
        Ok(())
    }

    /// Verifies independent zero-drop source and pending-registry finals.
    pub fn verify_source_final(&self) -> Result<ProducerEpochCutoffV1, EdgeSourceFinalSealErrorV1> {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let cutoff = state.cutoff.ok_or(EdgeSourceFinalSealErrorV1::CutoffMissing)?;
        if !state.poisons.is_empty() {
            return Err(EdgeSourceFinalSealErrorV1::Poisoned);
        }
        if state.event_pending_ack != 0 {
            return Err(EdgeSourceFinalSealErrorV1::EventPending);
        }
        if !state.decoded_source_generations.is_empty()
            || !state.payload_without_index_zero.is_empty()
            || !state.source_generation_contexts.is_empty()
            || !state.generation_wire_ordinals.is_empty()
            || !state.wire_lifecycle.is_empty()
            || !state.cache_pending.is_empty()
            || !state.snapshot_products.is_empty()
            || !state.payload_first_by_generation.is_empty()
        {
            return Err(EdgeSourceFinalSealErrorV1::ActiveStatePending);
        }
        let last_connection_valid = state.last_connection_record.is_some_and(|record| {
            state.connection_record_count == state.next_connection_sequence
                && record.connection_sequence.checked_add(1) == Some(state.next_connection_sequence)
                && record.record_hash == state.last_connection_hash
                && record.record_hash == AuthorityRecordHasherV1::connection(&record)
        });
        let h3_final = last_connection_valid
            && state.connection_phase == ConnectionPhaseV1::Exited
            && state.connection_established_count == state.connection_closed_count;
        let coverage_final = state.coverage_record_count == state.next_coverage_sequence
            && cutoff.last_coverage_sequence == state.next_coverage_sequence.saturating_sub(1);
        let source_final = state.authority_wire_terminals == state.next_wire_ordinal;
        if !h3_final || !coverage_final || !source_final {
            return Err(EdgeSourceFinalSealErrorV1::ConnectionFinalInvalid);
        }
        drop(state);
        self.registry.verify_final_seal().map_err(EdgeSourceFinalSealErrorV1::PendingRegistry)?;
        Ok(cutoff)
    }

    /// Returns finite source counters and pending cardinalities for final artifacts.
    pub fn source_final_counters(&self) -> (u64, u64, u64, u64, u64, u64, u64, usize) {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state.next_wire_ordinal,
            state.next_source_generation,
            state.next_payload_first_sequence,
            state.next_connection_sequence,
            state.processor_terminal_count,
            state.authority_wire_terminals,
            state.event_pending_ack,
            state.poisons.len(),
        )
    }
    #[cfg(test)]
    /// Returns connection records in append order.
    pub fn connection_records(&self) -> Vec<SourceConnectionRecordV1> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .connection_records
            .iter()
            .copied()
            .collect()
    }
    /// Returns only already-attached exact source/registry evidence for one CLI terminal.
    pub fn snapshot_evidence(
        &self,
        metadata: PendingSnapshotMetadataV2,
    ) -> Option<(
        PendingSnapshotMetadataV2,
        PayloadFirstObservationV1,
        ProcessorLifecycleProductV1,
        SourceConnectionRecordV1,
        PendingTerminalRecordV2,
    )> {
        let sequence = metadata.identity.pending_snapshot_sequence;
        let source_generation = metadata.source_generation?;
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let payload_first = state.payload_first_by_generation.remove(&source_generation)?;
        let processor = state.snapshot_products.remove(&sequence)?;
        if processor.source_generation != source_generation {
            return None;
        }
        let connection = state.last_connection_record?;
        drop(state);
        let registry_terminal = self.registry.terminal_records().into_iter().find(|record| {
            record.metadata.identity.pending_snapshot_sequence == sequence
                && record.metadata == metadata
        })?;
        Some((metadata, payload_first, processor, connection, registry_terminal))
    }
    /// Latches a named coordinator failure without changing production behavior.
    pub fn latch_coordinator_failure(&self, failure: &'static str) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .poisons
            .push(EdgeMeasurementPoisonV1::CoordinatorFailure(failure));
    }
}

/// Ordinary SHA-256 codecs matching S2 `hashAuthorityRecord`.
#[derive(Debug, Clone, Copy)]
pub struct AuthorityRecordHasherV1;

impl AuthorityRecordHasherV1 {
    fn authority(domain: &str, json: &str) -> B256 {
        const OUTER: &[u8] = b"base-edge-authority-record-v1\0";
        let mut bytes = Vec::with_capacity(12 + OUTER.len() + domain.len() + json.len());
        bytes.extend_from_slice(&(OUTER.len() as u32).to_be_bytes());
        bytes.extend_from_slice(OUTER);
        bytes.extend_from_slice(&(domain.len() as u32).to_be_bytes());
        bytes.extend_from_slice(domain.as_bytes());
        bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
        bytes.extend_from_slice(json.as_bytes());
        B256::from(DefaultCrypto.sha256(&bytes))
    }

    fn hex(value: B256) -> String {
        value.as_slice().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn payload_first(record: &PayloadFirstObservationV1) -> B256 {
        let status = |value| match value {
            ClockStatusV1::Ok => "Ok",
            ClockStatusV1::Failed(_) => "Failed",
        };
        let ns = |value: Option<u64>| {
            value.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""))
        };
        let payload_id: String =
            record.key.payload_id.iter().map(|byte| format!("{byte:02x}")).collect();
        let boot_id = String::from_utf8_lossy(&record.boot_id);
        let json = format!(
            "{{\"blockNumber\":\"{}\",\"bootId\":\"{}\",\"clockObservationOrdinal\":\"{}\",\"clockSourceVersion\":\"{}\",\"index0StructuralIdentity\":{{\"blockNumber\":\"{}\",\"canonicalWireDigest\":\"{}\",\"flashblockIndex\":\"0\",\"payloadId\":\"0x{}\",\"previousFlashblockId\":null}},\"monoNs\":{},\"monoStatus\":\"{}\",\"monotonicResolutionNs\":\"{}\",\"payloadId\":\"0x{}\",\"previousRecordHash\":\"{}\",\"producerEpoch\":\"{}\",\"realtimeResolutionNs\":\"{}\",\"recordSequence\":\"{}\",\"utcNs\":{},\"utcStatus\":\"{}\",\"wireDigest\":\"{}\"}}",
            record.key.block_number,
            boot_id,
            record.observation.clock_observation_ordinal,
            EDGE_CLOCK_SOURCE_VERSION_V1,
            record.key.block_number,
            Self::hex(record.observation.wire_digest),
            payload_id,
            ns(record.observation.mono_ns),
            status(record.observation.mono_status),
            record.monotonic_resolution_ns,
            payload_id,
            Self::hex(record.previous_record_hash),
            record.key.producer_epoch,
            record.realtime_resolution_ns,
            record.record_sequence,
            ns(record.observation.utc_ns),
            status(record.observation.utc_status),
            Self::hex(record.observation.wire_digest),
        );
        Self::authority("edge-payload-first-observation/v1", &json)
    }

    fn clock_anchor(
        record: &ClockAnchorRecordV1,
        boot_id: [u8; 36],
        realtime_resolution_ns: u64,
        monotonic_resolution_ns: u64,
    ) -> B256 {
        let optional = |value: Option<u64>| {
            value.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""))
        };
        let (pair_status, disposition, failure) =
            match (record.observation.utc_status, record.observation.mono_status) {
                (ClockStatusV1::Ok, ClockStatusV1::Ok) => ("BothOk", "Sampled", "null"),
                (ClockStatusV1::Failed(_), ClockStatusV1::Ok) => {
                    ("RealtimeFailedMonotonicOk", "Failed", "\"RealtimeSyscallFailed\"")
                }
                (ClockStatusV1::Ok, ClockStatusV1::Failed(_)) => {
                    ("RealtimeOkMonotonicFailed", "Failed", "\"MonotonicSyscallFailed\"")
                }
                (ClockStatusV1::Failed(_), ClockStatusV1::Failed(_)) => {
                    ("BothFailed", "Failed", "\"BothSyscallsFailed\"")
                }
            };
        let anchor_kind = if record.startup { "Startup" } else { "Periodic" };
        let boot_id = String::from_utf8_lossy(&boot_id);
        let json = format!(
            "{{\"anchorKind\":\"{}\",\"anchorSequence\":\"{}\",\"bootId\":\"{}\",\"clockObservationOrdinal\":\"{}\",\"clockSourceVersion\":\"{}\",\"disposition\":\"{}\",\"dueMonoNs\":\"{}\",\"failureEvidence\":{},\"kind\":\"Anchor\",\"monoNs\":{},\"monotonicResolutionNs\":\"{}\",\"pairStatus\":\"{}\",\"persistenceSequence\":\"{}\",\"previousAnchorHash\":\"{}\",\"producerEpoch\":\"{}\",\"realtimeResolutionNs\":\"{}\",\"sampledMonoNs\":\"{}\",\"schema\":\"edge-clock-anchor/v1\",\"utcNs\":{}}}",
            anchor_kind,
            record.anchor_sequence,
            boot_id,
            record.observation.clock_observation_ordinal,
            EDGE_CLOCK_SOURCE_VERSION_V1,
            disposition,
            record.due_mono_ns,
            failure,
            optional(record.observation.mono_ns),
            monotonic_resolution_ns,
            pair_status,
            record.anchor_sequence,
            Self::hex(record.previous_anchor_hash),
            record.producer_epoch,
            realtime_resolution_ns,
            record.sampled_mono_ns,
            optional(record.observation.utc_ns),
        );
        Self::authority("edge-clock-anchor/v1", &json)
    }
    fn connection(record: &SourceConnectionRecordV1) -> B256 {
        let transition = format!("{:?}", record.transition);
        let optional = |value: Option<u64>| {
            value.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""))
        };
        let error =
            record.error_class.map_or_else(|| "null".to_string(), |value| format!("\"{value:?}\""));
        let json = format!(
            "{{\"clockObservationOrdinal\":{},\"connectionSequence\":\"{}\",\"errorClass\":{},\"monoNs\":{},\"previousRecordHash\":\"{}\",\"producerEpoch\":\"{}\",\"schema\":\"edge-source-connection/v1\",\"sequence\":\"{}\",\"state\":\"{}\",\"transition\":\"{}\"}}",
            optional(record.clock_observation_ordinal),
            record.connection_sequence,
            error,
            optional(record.mono_ns),
            Self::hex(record.previous_record_hash),
            record.producer_epoch,
            record.connection_sequence,
            transition,
            transition,
        );
        Self::authority("edge-source-connection/v1", &json)
    }

    /// Hashes the exact S2 ten-field cutoff object.
    pub fn cutoff(record: &ProducerEpochCutoffV1) -> B256 {
        let json = format!(
            "{{\"cutoffClockObservationOrdinal\":\"{}\",\"lastAdmittedBlinkGeneration\":\"{}\",\"lastAdmittedSourceGeneration\":\"{}\",\"lastAdmittedWireOrdinal\":\"{}\",\"lastCandidateSequence\":\"{}\",\"lastCoverageSequence\":\"{}\",\"lastPendingSnapshotSequence\":\"{}\",\"latchMonoNs\":\"{}\",\"producerEpoch\":\"{}\"}}",
            record.cutoff_clock_observation_ordinal,
            record.last_admitted_blink_generation,
            record.last_admitted_source_generation,
            record.last_admitted_wire_ordinal,
            record.last_candidate_sequence,
            record.last_coverage_sequence,
            record.last_pending_snapshot_sequence,
            record.latch_mono_ns,
            record.producer_epoch,
        );
        Self::authority("edge-producer-epoch-cutoff/v1", &json)
    }
}

/// Optional recorder façade used by feature-gated production call sites.
#[derive(Debug, Clone, Copy)]
pub struct EdgeMeasurementRecorderHandleV1;

impl EdgeMeasurementRecorderHandleV1 {
    /// Records a connection transition only when installed.
    pub fn connection_transition(self, transition: SourceConnectionTransitionV1) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.connection_transition(transition);
        }
    }

    /// Samples wire bytes only when installed.
    pub fn observe_wire(self, bytes: &[u8]) -> Option<EpochAdmissionTokenV1> {
        EdgeMeasurementGlobal::installed()?.observe_wire(bytes)
    }

    /// Records fixed-cadence anchors only when installed.
    pub fn record_due_anchor(self) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.record_due_anchor();
        }
    }

    /// Returns whether an installed recorder has crossed authority cutoff.
    pub fn cutoff_latched(self) -> bool {
        EdgeMeasurementGlobal::installed().is_some_and(|recorder| recorder.cutoff_latched())
    }
    /// Completes decode only when installed.
    pub fn decoded_flashblock(
        self,
        admission: EpochAdmissionTokenV1,
        flashblock: &Flashblock,
    ) -> Option<u64> {
        EdgeMeasurementGlobal::installed()?.decoded_flashblock(admission, flashblock)
    }

    /// Terminalizes a sampled authority wire whose bytes did not decode.
    pub fn decode_rejected(self, admission: EpochAdmissionTokenV1) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.decode_rejected(admission);
        }
    }

    /// Records an excluded post-cutoff route entering or failing the actor mailbox.
    pub fn post_cutoff_actor_enqueue(self, admission: EpochAdmissionTokenV1, succeeded: bool) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.post_cutoff_actor_enqueue(admission, succeeded);
        }
    }

    /// Records excluded post-cutoff actor delivery.
    pub fn post_cutoff_actor_delivered(self, admission: EpochAdmissionTokenV1) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.post_cutoff_actor_delivered(admission);
        }
    }

    /// Records the actor mailbox enqueue result.
    pub fn actor_enqueue(self, source_generation: u64, succeeded: bool) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.actor_enqueue(source_generation, succeeded);
        }
    }

    /// Records actor delivery.
    pub fn actor_delivered(self, source_generation: u64) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.actor_delivered(source_generation);
        }
    }

    /// Reserves state handoff accounting before the queue can expose the item.
    pub fn begin_state_handoff(self, key: DecodedFlashblockKeyV1) -> Option<u64> {
        EdgeMeasurementGlobal::installed()?.begin_state_handoff(key)
    }

    /// Terminalizes a reserved handoff after the unchanged queue rejects it.
    pub fn state_handoff_failed(self, generation: u64) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.state_handoff_failed(generation);
        }
    }

    /// Terminalizes an excluded post-cutoff route after state handoff failure.
    pub fn post_cutoff_state_handoff_failed(self, key: DecodedFlashblockKeyV1) {
        if let Some(recorder) = EdgeMeasurementGlobal::installed() {
            recorder.post_cutoff_state_handoff_failed(key);
        }
    }

    /// Takes a generation only when installed.
    pub fn take_source_generation(self, flashblock: &Flashblock) -> Option<u64> {
        EdgeMeasurementGlobal::installed()?.take_source_generation(flashblock)
    }
}

/// Narrow CLI registry façade; feature-compiled but uninstalled is a no-op.
#[derive(Debug, Clone, Copy)]
pub struct EdgeMeasurementRegistryHandleV2;

impl EdgeMeasurementRegistryHandleV2 {
    /// Records a CLI receipt when installed.
    pub fn cli_received(
        self,
        pending: &Arc<PendingBlocks>,
    ) -> Result<Option<PendingSnapshotMetadataV2>, CliRegistryLookupFailed> {
        let Some(recorder) = EdgeMeasurementGlobal::installed() else {
            return Ok(None);
        };
        Ok(recorder.registry().cli_received(pending)?.ok())
    }

    /// Records lag attribution when installed.
    pub fn cli_lagged(self, count: u64) -> Result<(), PendingRegistryError> {
        let Some(recorder) = EdgeMeasurementGlobal::installed() else {
            return Ok(());
        };
        recorder.registry().cli_lagged(count)
    }

    /// Records close attribution when installed.
    pub fn cli_closed(self) -> Result<(), PendingRegistryError> {
        let Some(recorder) = EdgeMeasurementGlobal::installed() else {
            return Ok(());
        };
        recorder.registry().cli_closed()
    }

    /// Records cancellation attribution when installed.
    pub fn cli_cancelled(self) -> Result<(), PendingRegistryError> {
        let Some(recorder) = EdgeMeasurementGlobal::installed() else {
            return Ok(());
        };
        recorder.registry().cli_cancelled()
    }
}

/// Accessor for the explicitly installed process-wide recorder.
#[derive(Debug, Clone, Copy)]
pub struct EdgeMeasurementGlobal;

impl EdgeMeasurementGlobal {
    fn cell() -> &'static OnceLock<Arc<EdgeMeasurementRecorderV1>> {
        static RECORDER: OnceLock<Arc<EdgeMeasurementRecorderV1>> = OnceLock::new();
        &RECORDER
    }

    /// Installs one validated nonzero producer epoch.
    pub fn install(
        config: EdgeMeasurementInstallConfigV1,
    ) -> Result<Arc<EdgeMeasurementRecorderV1>, EdgeMeasurementInstallErrorV1> {
        if Self::cell().get().is_some() {
            return Err(EdgeMeasurementInstallErrorV1::ConflictingInstall);
        }
        let recorder = EdgeMeasurementRecorderV1::new(config)?;
        Self::cell()
            .set(Arc::clone(&recorder))
            .map_err(|_| EdgeMeasurementInstallErrorV1::ConflictingInstall)?;
        Ok(recorder)
    }

    /// Returns the no-op-until-installed production façade.
    pub const fn recorder() -> EdgeMeasurementRecorderHandleV1 {
        EdgeMeasurementRecorderHandleV1
    }

    /// Returns the installed recorder for coordinator finalization.
    pub fn installed() -> Option<Arc<EdgeMeasurementRecorderV1>> {
        Self::cell().get().map(Arc::clone)
    }

    /// Returns the no-op-until-installed CLI registry façade.
    pub const fn registry_handle() -> EdgeMeasurementRegistryHandleV2 {
        EdgeMeasurementRegistryHandleV2
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc as StdArc, Barrier, mpsc},
        thread,
        time::Duration,
    };

    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::{Address, Bloom, Bytes, U256, hex};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata,
    };

    use super::*;

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
    fn test_recorder(epoch: u64) -> Arc<EdgeMeasurementRecorderV1> {
        EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(epoch).expect("nonzero test epoch"),
            event_queue_capacity: 64,
            active_state_capacity: 64,
            pending_registry_capacity: 64,
        })
        .expect("Linux recorder")
    }

    #[test]
    fn wire_digest_uses_sha256_and_checked_ordinals() {
        let recorder = test_recorder(7);
        let first = recorder.observe_wire(b"").expect("first observation");
        let second = recorder.observe_wire(b"base").expect("second observation");
        assert_eq!(first.observation.clock_observation_ordinal, 1);
        assert_eq!(second.observation.clock_observation_ordinal, 2);
        assert_eq!(
            first.observation.wire_digest,
            B256::from_slice(
                &hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                    .expect("valid vector")
            )
        );
    }

    #[test]
    fn due_anchor_uses_shared_ordinal_and_advances_exact_cadence() {
        let recorder = test_recorder(8);
        let EdgeEventDrainStatusV1::Event(startup) = recorder.try_recv_event() else {
            panic!("startup anchor");
        };
        assert!(matches!(
            startup.as_ref(),
            EdgeSourceEventV1::ClockAnchor(ClockAnchorRecordV1 {
                anchor_sequence: 0,
                startup: true,
                ..
            })
        ));
        recorder.ack_event_durable().expect("startup durable");

        let (_, Some(due_mono_ns)) =
            EdgeMeasurementRecorderV1::raw_clock(ClockIdV1::Monotonic, false)
        else {
            panic!("Linux monotonic clock");
        };
        {
            let mut state = recorder.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.next_anchor_due_mono_ns = due_mono_ns;
        }
        recorder.record_due_anchor();

        let EdgeEventDrainStatusV1::Event(periodic) = recorder.try_recv_event() else {
            panic!("periodic anchor");
        };
        let EdgeSourceEventV1::ClockAnchor(periodic) = periodic.as_ref() else {
            panic!("clock anchor");
        };
        assert!(!periodic.startup);
        assert_eq!(periodic.anchor_sequence, 1);
        assert_eq!(periodic.observation.clock_observation_ordinal, 1);
        assert_eq!(periodic.due_mono_ns, due_mono_ns);
        let state = recorder.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(state.next_anchor_due_mono_ns, due_mono_ns + EDGE_ANCHOR_CADENCE_NS_V1);
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
    fn same_arc_repeated_publish_uses_distinct_sequences_and_pointer_fifo() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(9, 4);
        let first = registry.register(&pending, Some(12));
        let second = registry.register(&pending, Some(13));
        assert_eq!(first.pending_snapshot_sequence, Some(0));
        assert_eq!(second.pending_snapshot_sequence, Some(1));
        assert_eq!(first.disposition, PendingRegistrationDispositionV2::Succeeded);
        assert_eq!(second.disposition, PendingRegistrationDispositionV2::Succeeded);

        registry.record_send(first, Some(1)).expect("first published");
        registry.record_send(second, Some(1)).expect("second published");
        assert_eq!(
            registry
                .cli_received(&pending)
                .expect("first FIFO lookup")
                .expect("authority")
                .source_generation,
            Some(12)
        );
        assert_eq!(
            registry
                .cli_received(&pending)
                .expect("second FIFO lookup before first ack")
                .expect("authority")
                .source_generation,
            Some(13)
        );
        registry.ack_terminal_durable(0).expect("first durable ack");
        registry.ack_terminal_durable(1).expect("second durable ack");

        assert_eq!(registry.verify_final_seal(), Ok(()));
        assert_eq!(registry.snapshot().sets.cli_received_lookup_succeeded, BTreeSet::from([0, 1]));
    }

    #[test]
    fn nonadvanced_passthrough_is_distinct_from_registration_failure() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(10, 4);
        registry.begin_unregistered_send();
        registry
            .record_unregistered_send(PendingSendJournalMarkerV2::PassthroughNonAdvanced, Some(1))
            .expect("passthrough disposition");
        assert_eq!(
            registry.cli_received(&pending),
            Ok(Err(PendingSendJournalMarkerV2::PassthroughNonAdvanced))
        );
        registry.begin_unregistered_send();
        registry
            .record_unregistered_send(
                PendingSendJournalMarkerV2::PostCutoffAdvancedNonAuthority,
                Some(1),
            )
            .expect("postcutoff disposition");
        assert_eq!(
            registry.cli_received(&pending),
            Ok(Err(PendingSendJournalMarkerV2::PostCutoffAdvancedNonAuthority,))
        );
        assert_eq!(
            registry.snapshot().unregistered_send_records,
            vec![
                (
                    PendingSendJournalMarkerV2::PassthroughNonAdvanced,
                    PendingSendDispositionV2::Published { receiver_count: 1 },
                ),
                (
                    PendingSendJournalMarkerV2::PostCutoffAdvancedNonAuthority,
                    PendingSendDispositionV2::Published { receiver_count: 1 },
                ),
            ]
        );
    }

    #[test]
    fn cli_lookup_waits_until_send_accounting_is_conserved() {
        let pending = test_pending_blocks();
        let registry = StdArc::new(PendingMetadataRegistryV2::new(10, 4));
        let attempt = registry.register(&pending, Some(14));
        let barrier = StdArc::new(Barrier::new(2));
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let lookup_registry = StdArc::clone(&registry);
        let lookup_pending = Arc::clone(&pending);
        let lookup_barrier = StdArc::clone(&barrier);
        let lookup = thread::spawn(move || {
            lookup_barrier.wait();
            result_sender
                .send(lookup_registry.cli_received(&lookup_pending))
                .expect("result receiver");
        });

        barrier.wait();
        assert!(matches!(
            result_receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        registry.record_send(attempt, Some(1)).expect("published send");
        let metadata = result_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("lookup unblocked")
            .expect("identity lookup")
            .expect("authority metadata");
        lookup.join().expect("lookup thread");
        assert_eq!(metadata.identity.pending_snapshot_sequence, 0);
        assert_eq!(metadata.identity.arc_pointer_identity, Arc::as_ptr(&pending) as usize);
        registry.ack_terminal_durable(0).expect("durable terminal");

        let sets = registry.snapshot().sets;
        assert_eq!(sets.advanced_with_snapshot, BTreeSet::from([0]));
        assert_eq!(sets.registration_succeeded, BTreeSet::from([0]));
        assert_eq!(sets.send_published, BTreeSet::from([0]));
        assert_eq!(sets.cli_received_lookup_succeeded, BTreeSet::from([0]));
        assert!(sets.pending_delivery_final.is_empty());
        assert_eq!(registry.verify_final_seal(), Ok(()));
    }

    #[test]
    fn distinct_arcs_with_same_public_digest_are_healthy() {
        let first_pending = test_pending_blocks();
        let second_pending = test_pending_blocks();
        assert!(!Arc::ptr_eq(&first_pending, &second_pending));
        assert_eq!(
            PendingMetadataRegistryV2::pending_public_subset_digest_v1(&first_pending),
            PendingMetadataRegistryV2::pending_public_subset_digest_v1(&second_pending)
        );
        let registry = PendingMetadataRegistryV2::new(3, 4);
        let first = registry.register(&first_pending, None);
        let second = registry.register(&second_pending, None);
        assert_eq!(first.disposition, PendingRegistrationDispositionV2::Succeeded);
        assert_eq!(second.disposition, PendingRegistrationDispositionV2::Succeeded);
        registry.record_send(first, None).expect("first no receivers");
        registry.record_send(second, None).expect("second no receivers");
        registry.ack_terminal_durable(0).expect("first ack");
        registry.ack_terminal_durable(1).expect("second ack");
        assert_eq!(registry.verify_final_seal(), Ok(()));
    }

    #[test]
    fn checked_overflow_latches_named_poison_without_clamping() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(1, 4);
        {
            let (mut state, _) = registry.lock_state();
            state.next_sequence = u64::MAX;
            state.counters.advanced_with_snapshot = u64::MAX;
        }
        let attempt = registry.register(&pending, None);
        assert_eq!(attempt.pending_snapshot_sequence, None);
        assert_eq!(
            attempt.disposition,
            PendingRegistrationDispositionV2::Failed(
                PendingRegistrationFailure::PendingSnapshotSequenceOverflow
            )
        );
        let (state, _) = registry.lock_state();
        assert_eq!(state.counters.advanced_with_snapshot, u64::MAX);
        assert!(state.poisons.contains(&PendingRegistryPoisonV2::AccountingOverflow(
            PendingAccountingFieldV2::AdvancedWithSnapshot
        )));
    }

    #[test]
    fn failed_registration_published_cli_ok_is_lookup_failed() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(4, 1);
        let occupying = registry.register(&pending, None);
        registry.record_send(occupying, None).expect("occupying terminal");
        let failed = registry.register(&pending, Some(8));
        assert_eq!(
            failed.disposition,
            PendingRegistrationDispositionV2::Failed(
                PendingRegistrationFailure::PendingRegistryCapacityOverflow
            )
        );
        registry.ack_terminal_durable(0).expect("release successful entry");
        registry.record_send(failed, Some(1)).expect("failed registration still published");
        let failure = registry.cli_received(&pending).expect_err("lookup cannot succeed");
        assert_eq!(
            failure.reason,
            CliRegistryLookupFailureReason::RegistrationFailed(
                PendingRegistrationFailure::PendingRegistryCapacityOverflow
            )
        );
        registry.ack_terminal_durable(1).expect("failed terminal ack");
        let sets = registry.snapshot().sets;
        assert_eq!(sets.failed_registration_published, BTreeSet::from([1]));
        assert_eq!(sets.failed_reg_cli_registry_lookup_failed, BTreeSet::from([1]));
        assert!(sets.failed_reg_cli_received_lookup_succeeded.is_empty());
    }

    #[test]
    fn failed_registration_no_receivers_has_distinct_terminal() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(5, 0);
        let failed = registry.register(&pending, None);
        registry.record_send(failed, None).expect("no-receiver terminal accepted");
        assert_eq!(
            registry.terminal_records()[0].terminal,
            PendingCliTerminalV2::RegistrationFailedNoReceivers
        );
        assert_eq!(registry.snapshot().sets.registration_failed_no_receivers, BTreeSet::from([0]));
        registry.ack_terminal_durable(0).expect("failed no-receiver ack");
    }

    #[test]
    fn lag_preserves_mixed_registration_intersections() {
        let first = test_pending_blocks();
        let second = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(6, 1);
        let succeeded = registry.register(&first, None);
        let failed = registry.register(&second, None);
        registry.record_send(succeeded, Some(1)).expect("success published");
        registry.record_send(failed, Some(1)).expect("failure published");
        registry.cli_lagged(2).expect("exact lag range");
        registry.ack_terminal_durable(0).expect("first ack");
        registry.ack_terminal_durable(1).expect("second ack");
        let sets = registry.snapshot().sets;
        assert_eq!(sets.cli_lagged_attributed, BTreeSet::from([0, 1]));
        assert_eq!(sets.failed_reg_cli_lagged_attributed, BTreeSet::from([1]));
    }

    #[test]
    fn closed_and_cancelled_terminalize_exact_remaining_ranges() {
        let closed_pending = test_pending_blocks();
        let closed = PendingMetadataRegistryV2::new(7, 2);
        let attempt = closed.register(&closed_pending, None);
        closed.record_send(attempt, Some(1)).expect("published");
        closed.cli_closed().expect("closed range");
        closed.ack_terminal_durable(0).expect("closed ack");
        assert_eq!(closed.snapshot().sets.cli_closed_attributed, BTreeSet::from([0]));

        let cancelled_pending = test_pending_blocks();
        let cancelled = PendingMetadataRegistryV2::new(8, 2);
        let attempt = cancelled.register(&cancelled_pending, None);
        cancelled.record_send(attempt, Some(1)).expect("published");
        cancelled.cli_cancelled().expect("cancelled range");
        cancelled.ack_terminal_durable(0).expect("cancelled ack");
        assert_eq!(cancelled.snapshot().sets.cli_cancelled_attributed, BTreeSet::from([0]));
    }

    #[test]
    fn coverage_cursor_distinguishes_empty_from_sequence_zero() {
        let registry = PendingMetadataRegistryV2::new(81, 1);
        let empty = registry.snapshot();
        assert_eq!(empty.coverage_count, 0);
        assert_eq!(empty.last_coverage_sequence, None);

        let pending = test_pending_blocks();
        let attempt = registry.register(&pending, None);
        registry.record_send(attempt, None).expect("terminal accepted");
        let one = registry.snapshot();
        assert_eq!(one.coverage_count, 1);
        assert_eq!(one.last_coverage_sequence, Some(0));
        assert_eq!(registry.terminal_records()[0].coverage_sequence, 0);
        registry.ack_terminal_durable(0).expect("coverage durable");
    }
    #[test]
    fn final_seal_requires_ack_and_cleanup_order_is_exact() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(9, 2);
        let attempt = registry.register(&pending, None);
        registry.record_send(attempt, Some(1)).expect("published");
        let metadata = registry.cli_received(&pending).expect("lookup").expect("authority send");
        assert_eq!(metadata.identity.pending_snapshot_sequence, 0);
        assert_eq!(
            registry.verify_final_seal(),
            Err(PendingFinalSealErrorV2::DurabilityAckPending)
        );
        assert_eq!(
            registry.cleanup_events(),
            vec![
                PendingCleanupEventV2::TerminalAppended(0),
                PendingCleanupEventV2::CoverageQueueAccepted(0),
            ]
        );
        assert_eq!(registry.snapshot().primary_pending, 1);
        assert_eq!(registry.snapshot().secondary_pending, 1);

        registry.ack_terminal_durable(0).expect("durability ack");
        assert_eq!(
            registry.cleanup_events(),
            vec![
                PendingCleanupEventV2::TerminalAppended(0),
                PendingCleanupEventV2::CoverageQueueAccepted(0),
                PendingCleanupEventV2::DurabilityAcknowledged(0),
                PendingCleanupEventV2::SecondaryRemoved(0),
                PendingCleanupEventV2::PrimaryRemoved(0),
                PendingCleanupEventV2::RetainedWeakDropped(0),
            ]
        );
        assert_eq!(registry.verify_final_seal(), Ok(()));
    }

    #[test]
    fn final_seal_rejects_counter_set_and_coverage_cursor_mismatch() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(91, 2);
        let attempt = registry.register(&pending, None);
        registry.record_send(attempt, None).expect("terminal accepted");
        registry.ack_terminal_durable(0).expect("terminal durable");
        {
            let (mut state, _) = registry.lock_state();
            state.counters.send_no_receivers = 0;
        }
        assert_eq!(registry.verify_final_seal(), Err(PendingFinalSealErrorV2::SequenceSetMismatch));

        let coverage = PendingMetadataRegistryV2::new(92, 2);
        let attempt = coverage.register(&pending, None);
        coverage.record_send(attempt, None).expect("terminal accepted");
        coverage.ack_terminal_durable(0).expect("terminal durable");
        {
            let (mut state, _) = coverage.lock_state();
            state.next_coverage_sequence = 2;
        }
        assert_eq!(coverage.verify_final_seal(), Err(PendingFinalSealErrorV2::SequenceSetMismatch));
    }

    #[test]
    fn decoded_source_generation_is_joined_fifo_to_processor_admission() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(3);
        let observation = recorder.observe_wire(b"frame").expect("wire observation");
        assert_eq!(recorder.decoded_flashblock(observation, &flashblock), Some(0));
        assert_eq!(recorder.take_source_generation(&flashblock), Some(0));
        assert_eq!(recorder.take_source_generation(&flashblock), None);
    }
    #[test]
    fn connection_records_preserve_direct_and_backoff_paths() {
        let recorder = test_recorder(1);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::ConnectFailure);
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffCompleted);
        recorder
            .connection_transition(SourceConnectionTransitionV1::BackoffReconnectAttemptStarted);
        assert_eq!(recorder.connection_records().len(), 6);
    }

    #[test]
    fn cutoff_during_connect_does_not_cancel_production_or_admit_more_authority() {
        let recorder = test_recorder(31);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        let records_at_cutoff = recorder.connection_records();
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        recorder.connection_transition(SourceConnectionTransitionV1::ConnectFailure);
        let post_cutoff = recorder.observe_wire(b"production-continues").expect("explicit route");

        assert_eq!(post_cutoff.route, EpochRouteV1::PostCutoffNonAuthority);
        assert_eq!(recorder.connection_records(), records_at_cutoff);
        assert!(!records_at_cutoff.iter().any(
            |record| record.transition == SourceConnectionTransitionV1::OwnerShutdownRequested
        ));
        assert_eq!(
            records_at_cutoff
                .iter()
                .filter(|record| {
                    record.transition == SourceConnectionTransitionV1::ConnectionTaskExited
                })
                .count(),
            1
        );
        assert_eq!(
            records_at_cutoff.last().map(|record| record.transition),
            Some(SourceConnectionTransitionV1::ConnectionTaskExited)
        );
    }

    #[test]
    fn cutoff_during_established_read_closes_measurement_authority_once() {
        let recorder = test_recorder(32);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        let bounds = ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        };
        let first = recorder.latch_cutoff(bounds);
        let records_at_cutoff = recorder.connection_records();
        let second = recorder.latch_cutoff(bounds);
        recorder.connection_transition(SourceConnectionTransitionV1::ControlPingReceived);
        recorder.connection_transition(SourceConnectionTransitionV1::EstablishedClosedByReadError);

        assert_eq!(first, second);
        assert_eq!(recorder.connection_records(), records_at_cutoff);
        assert_eq!(
            records_at_cutoff
                .iter()
                .filter(|record| {
                    record.transition == SourceConnectionTransitionV1::EstablishedClosedByCutoff
                })
                .count(),
            1
        );
    }

    #[test]
    fn cutoff_during_backoff_does_not_complete_or_change_reconnect_flow() {
        let recorder = test_recorder(33);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::ConnectFailure);
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffStarted);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        let records_at_cutoff = recorder.connection_records();
        recorder.connection_transition(SourceConnectionTransitionV1::BackoffCompleted);
        recorder
            .connection_transition(SourceConnectionTransitionV1::BackoffReconnectAttemptStarted);

        assert_eq!(recorder.connection_records(), records_at_cutoff);
        assert!(!records_at_cutoff.iter().any(|record| {
            matches!(
                record.transition,
                SourceConnectionTransitionV1::BackoffCompleted
                    | SourceConnectionTransitionV1::BackoffReconnectAttemptStarted
                    | SourceConnectionTransitionV1::OwnerShutdownRequested
            )
        }));
    }
    #[test]
    fn payload_first_hash_matches_s2_node_crypto_vector() {
        let record = PayloadFirstObservationV1 {
            key: PayloadFirstKeyV1 {
                producer_epoch: 1,
                block_number: 2,
                payload_id: [1, 2, 3, 4, 5, 6, 7, 8],
            },
            source_generation: 0,
            observation: WireObservationV1 {
                clock_observation_ordinal: 3,
                utc_status: ClockStatusV1::Ok,
                utc_ns: Some(4),
                mono_status: ClockStatusV1::Ok,
                mono_ns: Some(5),
                wire_digest: B256::repeat_byte(6),
            },
            boot_id: *b"00000000-0000-0000-0000-000000000000",
            realtime_resolution_ns: 7,
            monotonic_resolution_ns: 8,
            record_sequence: 9,
            previous_record_hash: B256::repeat_byte(10),
            record_hash: B256::ZERO,
        };
        assert_eq!(
            AuthorityRecordHasherV1::payload_first(&record),
            B256::from_slice(
                &hex::decode("08377302a9b43569b925303091a6bcaeee0456f463b174ac4bb0c325b0c5833e")
                    .expect("valid S2 Node crypto vector")
            )
        );
    }
    #[test]
    fn cutoff_hash_matches_s2_node_crypto_vector() {
        let record = ProducerEpochCutoffV1 {
            producer_epoch: 1,
            cutoff_clock_observation_ordinal: 2,
            last_admitted_wire_ordinal: 3,
            last_admitted_source_generation: 4,
            last_admitted_blink_generation: 5,
            last_pending_snapshot_sequence: 6,
            last_coverage_sequence: 7,
            last_candidate_sequence: 8,
            latch_mono_ns: 90_071_992_547_409_930,
            record_hash: B256::ZERO,
        };
        assert_eq!(
            AuthorityRecordHasherV1::cutoff(&record),
            B256::from_slice(
                &hex::decode("41c32b1694d1d01067233e5be9dcf59f90a6b1247d5f47b0cd2efb5d86e24b67")
                    .expect("valid S2 vector")
            )
        );
    }

    #[test]
    fn bounded_queue_full_poison_does_not_block_transition() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(11).expect("nonzero"),
            event_queue_capacity: 1,
            active_state_capacity: 4,
            pending_registry_capacity: 4,
        })
        .expect("Linux recorder");
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        let state = recorder.state.lock().expect("state");
        assert!(state.poisons.contains(&EdgeMeasurementPoisonV1::EventQueueFull));
        assert_eq!(state.next_connection_sequence, 2);
    }

    #[test]
    fn uninstalled_global_is_explicit_noop_handle() {
        if EdgeMeasurementGlobal::installed().is_none() {
            assert!(EdgeMeasurementGlobal::recorder().observe_wire(b"frame").is_none());
            assert_eq!(EdgeMeasurementGlobal::registry_handle().cli_closed(), Ok(()));
        }
    }
    #[test]
    fn cache_cutoff_fails_closed_without_synthetic_terminal() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(21);
        let admission = recorder.observe_wire(b"cached").expect("wire observation");
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), Some(0));
        assert_eq!(recorder.take_source_generation(&flashblock), Some(0));
        recorder.observe_cache_wait(0, ProcessorBaseDispositionV1::CachedAwaitCanonical);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });

        let mut products = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event {
                products.push(product);
            }
        }
        assert!(products.is_empty());
        let state = recorder.state.lock().expect("state");
        assert!(!state.cache_pending.is_empty());
        assert!(!state.source_generation_contexts.is_empty());
        assert!(state.poisons.contains(&EdgeMeasurementPoisonV1::CacheDrainIncomplete));
    }

    #[test]
    fn only_owner_resolved_generations_receive_terminal_products() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(22);
        for bytes in [b"direct".as_slice(), b"replaced", b"unresolved"] {
            let admission = recorder.observe_wire(bytes).expect("wire observation");
            recorder.decoded_flashblock(admission, &flashblock).expect("generation");
            recorder.take_source_generation(&flashblock).expect("processor admission");
        }
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: 0,
            base_disposition: ProcessorBaseDispositionV1::UnchangedDuplicateExact,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        recorder.observe_cache_wait(1, ProcessorBaseDispositionV1::CachedAwaitPredecessor);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: 1,
            base_disposition: ProcessorBaseDispositionV1::CacheReplacedOldGeneration,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        recorder.observe_cache_wait(2, ProcessorBaseDispositionV1::CachedAwaitCanonical);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });

        let mut terminal_generations = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event {
                terminal_generations.push(product.source_generation);
            }
        }
        terminal_generations.sort_unstable();
        assert_eq!(terminal_generations, vec![0, 1]);
        let state = recorder.state.lock().expect("state");
        assert_eq!(state.processor_terminal_count, 2);
        assert_eq!(state.cache_pending.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(state.source_generation_contexts.contains_key(&2));
    }
    #[test]
    fn resolved_cache_product_preserves_nested_processor_disposition() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(23);
        let admission = recorder.observe_wire(b"resolved").expect("wire observation");
        recorder.decoded_flashblock(admission, &flashblock).expect("generation");
        recorder.take_source_generation(&flashblock).expect("processor admission");
        recorder.observe_cache_wait(0, ProcessorBaseDispositionV1::CachedAwaitCanonical);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: 0,
            base_disposition: ProcessorBaseDispositionV1::CacheResolvedToProcessor,
            observer_disposition: ProcessorObserverDispositionV1::Delivered,
            publish_disposition: ProcessorPublishDispositionV1::Published(1),
            pending_snapshot_sequence: Some(7),
            processor_error_reason: None,
            cache_resolved_final_disposition: Some(ProcessorBaseDispositionV1::AdvancedInitialBase),
        });

        let product = loop {
            if let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event()
                && let EdgeSourceEventV1::Processor(product) = *event
            {
                break product;
            }
        };
        assert_eq!(
            product.cache_resolved_final_disposition,
            Some(ProcessorBaseDispositionV1::AdvancedInitialBase)
        );
        assert_eq!(product.pending_snapshot_sequence, Some(7));
    }
    #[test]
    fn producer_handoffs_are_accounted_before_consumers_and_do_not_enter_h3() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(101);
        let admission = recorder.observe_wire(b"ordered").expect("wire");
        let generation =
            recorder.decoded_flashblock(admission, &flashblock).expect("decoded generation");

        recorder.actor_enqueue(generation, true);
        recorder.actor_delivered(generation);
        assert_eq!(
            recorder.begin_state_handoff(DecodedFlashblockKeyV1::from_flashblock(&flashblock)),
            Some(generation)
        );
        assert_eq!(recorder.take_source_generation(&flashblock), Some(generation));
        assert!(recorder.connection_records().is_empty());
    }

    #[test]
    fn cache_drain_claim_excludes_cutoff_terminalization() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(102);
        let admission = recorder.observe_wire(b"cache-race").expect("wire");
        let generation =
            recorder.decoded_flashblock(admission, &flashblock).expect("decoded generation");
        recorder.actor_enqueue(generation, true);
        recorder.actor_delivered(generation);
        recorder.begin_state_handoff(DecodedFlashblockKeyV1::from_flashblock(&flashblock));
        recorder.take_source_generation(&flashblock);
        recorder.observe_cache_wait(generation, ProcessorBaseDispositionV1::CachedAwaitCanonical);

        assert!(recorder.claim_cache_resolution(generation));
        recorder.prepare_cutoff();
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: generation,
            base_disposition: ProcessorBaseDispositionV1::CacheResolvedToProcessor,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: Some(
                ProcessorBaseDispositionV1::UnchangedDuplicateExact,
            ),
        });
        let mut products = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event {
                products.push(product);
            }
            recorder.ack_event_durable().expect("event durable");
        }
        assert!(recorder.cutoff_drain_complete());
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 17,
            last_candidate_sequence: 0,
        });
        assert_eq!(products.len(), 1);
        assert_eq!(
            products[0].base_disposition,
            ProcessorBaseDispositionV1::CacheResolvedToProcessor
        );
        let state = recorder.state.lock().expect("state");
        assert_eq!(
            state.cutoff.expect("cutoff").last_coverage_sequence,
            state.next_coverage_sequence.saturating_sub(1)
        );
    }

    #[test]
    fn h3_records_exact_direct_backoff_halfclose_ping_sequence_without_drop() {
        let recorder = test_recorder(103);
        let transitions = [
            SourceConnectionTransitionV1::OwnerStart,
            SourceConnectionTransitionV1::InitialConnectAttemptStarted,
            SourceConnectionTransitionV1::Established,
            SourceConnectionTransitionV1::ReadHalfClosedWaitingForControl,
            SourceConnectionTransitionV1::OutgoingPingDue,
            SourceConnectionTransitionV1::OutgoingPingWritten,
            SourceConnectionTransitionV1::OutgoingPingWrittenWhileReadHalfClosed,
            SourceConnectionTransitionV1::OutgoingPingDue,
            SourceConnectionTransitionV1::NoPongTimeout,
            SourceConnectionTransitionV1::EstablishedClosedByNoPong,
            SourceConnectionTransitionV1::BackoffStarted,
            SourceConnectionTransitionV1::BackoffCompleted,
            SourceConnectionTransitionV1::BackoffReconnectAttemptStarted,
            SourceConnectionTransitionV1::Established,
            SourceConnectionTransitionV1::CloseFrameReceived,
            SourceConnectionTransitionV1::EstablishedClosedByClose,
            SourceConnectionTransitionV1::DirectReconnectAttemptStarted,
            SourceConnectionTransitionV1::Established,
            SourceConnectionTransitionV1::ReadError,
            SourceConnectionTransitionV1::EstablishedClosedByReadError,
            SourceConnectionTransitionV1::DirectReconnectAttemptStarted,
            SourceConnectionTransitionV1::Established,
            SourceConnectionTransitionV1::OutgoingPingDue,
            SourceConnectionTransitionV1::PingWriteFailure,
            SourceConnectionTransitionV1::EstablishedClosedByPingWriteFailure,
            SourceConnectionTransitionV1::BackoffStarted,
            SourceConnectionTransitionV1::BackoffCompleted,
            SourceConnectionTransitionV1::BackoffReconnectAttemptStarted,
            SourceConnectionTransitionV1::Established,
        ];
        for transition in transitions {
            recorder.connection_transition(transition);
        }
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });

        let records = recorder.connection_records();
        let accepted: Vec<_> = records.iter().map(|record| record.transition).collect();
        let mut expected = transitions.to_vec();
        expected.extend([
            SourceConnectionTransitionV1::EstablishedClosedByCutoff,
            SourceConnectionTransitionV1::AuthorityCutoffLatched,
            SourceConnectionTransitionV1::ConnectionTaskExited,
        ]);
        assert_eq!(accepted, expected);
        assert_eq!(u64::try_from(records.len()), Ok(recorder.source_final_counters().3));
        assert!(records.iter().enumerate().all(|(index, record)| {
            u64::try_from(index) == Ok(record.connection_sequence)
                && record.clock_observation_ordinal.is_some()
                && record.mono_ns.is_some()
                && record.record_hash == AuthorityRecordHasherV1::connection(record)
        }));
        let established = records
            .iter()
            .filter(|record| record.transition == SourceConnectionTransitionV1::Established)
            .count();
        let closed = records
            .iter()
            .filter(|record| {
                matches!(
                    record.transition,
                    SourceConnectionTransitionV1::EstablishedClosedByClose
                        | SourceConnectionTransitionV1::EstablishedClosedByReadError
                        | SourceConnectionTransitionV1::EstablishedClosedByNoPong
                        | SourceConnectionTransitionV1::EstablishedClosedByPingWriteFailure
                        | SourceConnectionTransitionV1::EstablishedClosedByCutoff
                )
            })
            .count();
        assert_eq!(established, closed);
    }

    #[test]
    fn h3_rejects_duplicate_half_close_ping_without_due_and_unsolicited_pong() {
        for invalid in [
            SourceConnectionTransitionV1::ReadHalfClosedWaitingForControl,
            SourceConnectionTransitionV1::OutgoingPingWritten,
            SourceConnectionTransitionV1::ControlPongReceived,
        ] {
            let recorder = test_recorder(201);
            recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
            recorder
                .connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
            recorder.connection_transition(SourceConnectionTransitionV1::Established);
            if invalid == SourceConnectionTransitionV1::ReadHalfClosedWaitingForControl {
                recorder.connection_transition(invalid);
            }
            recorder.connection_transition(invalid);
            assert!(
                recorder
                    .state
                    .lock()
                    .expect("state")
                    .poisons
                    .contains(&EdgeMeasurementPoisonV1::ConnectionTransitionConflict)
            );
        }
    }

    #[test]
    fn authority_admission_survives_decode_after_cutoff_fence() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(202);
        let admission = recorder.observe_wire(b"before-fence").expect("admission");
        recorder.prepare_cutoff();
        assert_eq!(admission.route, EpochRouteV1::Authority);
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), Some(0));
    }

    #[test]
    fn post_cutoff_route_emits_explicit_exclusion_through_processor() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let key = DecodedFlashblockKeyV1::from_flashblock(&flashblock);
        let recorder = test_recorder(203);
        recorder.prepare_cutoff();
        let admission = recorder.observe_wire(b"excluded").expect("excluded admission");
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), None);
        recorder.post_cutoff_actor_enqueue(admission, true);
        recorder.post_cutoff_actor_delivered(admission);
        assert_eq!(recorder.begin_state_handoff(key), None);
        assert_eq!(recorder.take_source_generation(&flashblock), None);

        let mut transitions = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Coverage(record) = *event {
                transitions.push(record.transition);
            }
        }
        assert!(transitions.ends_with(&[
            WireLifecycleTransitionV1::PostCutoffActorEnqueued,
            WireLifecycleTransitionV1::PostCutoffActorDelivered,
            WireLifecycleTransitionV1::PostCutoffStateHandedOff,
            WireLifecycleTransitionV1::PostCutoffProcessorExcluded,
        ]));
    }
}
