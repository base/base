//! Feature-private edge measurement state for the flashblocks producer.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
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
/// Owner-reviewed cumulative terminal capacity covering 72 hours at twice the measured 5 Hz rate.
pub const PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2: usize = 72 * 60 * 60 * 10;
/// Fixed bound for diagnostic-only registry history.
const PENDING_DIAGNOSTIC_RING_CAPACITY_V2: usize = 64;

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
    /// The owner-reviewed cumulative terminal capacity was exhausted.
    TerminalRecordCapacityOverflow,
    /// Registry storage could not reserve space before mutating terminal state.
    TerminalRecordAllocationFailed,
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
/// Reason a terminal could not enter the durable H2 queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingTerminalExclusionReasonV2 {
    /// The owner-reviewed cumulative terminal capacity was exhausted.
    Capacity,
    /// Storage for the durable terminal record could not be reserved.
    Allocation,
}

/// Exact recorder cleanup receipt for an H2 terminal excluded before durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingTerminalExclusionV2 {
    /// Immutable snapshot metadata whose recorder bindings must be released.
    metadata: PendingSnapshotMetadataV2,
    /// Exact CLI terminal that was accounted by H2.
    terminal: PendingCliTerminalV2,
    /// Operational exclusion reason.
    reason: PendingTerminalExclusionReasonV2,
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

/// Sparse exact sequence bitmap with at most one tree entry per 64-sequence word.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingSequenceBitmapV2 {
    words: BTreeMap<u64, u64>,
    len: usize,
}

impl PendingSequenceBitmapV2 {
    fn insert(&mut self, sequence: u64) -> bool {
        let word = sequence / 64;
        let mask = 1_u64 << (sequence % 64);
        let entry = self.words.entry(word).or_default();
        if *entry & mask != 0 {
            return false;
        }
        *entry |= mask;
        self.len += 1;
        true
    }

    fn remove(&mut self, sequence: &u64) -> bool {
        let word = *sequence / 64;
        let mask = 1_u64 << (*sequence % 64);
        let Some(entry) = self.words.get_mut(&word) else {
            return false;
        };
        if *entry & mask == 0 {
            return false;
        }
        *entry &= !mask;
        self.len -= 1;
        if *entry == 0 {
            self.words.remove(&word);
        }
        true
    }

    fn contains(&self, sequence: &u64) -> bool {
        self.words
            .get(&(*sequence / 64))
            .is_some_and(|word| *word & (1_u64 << (*sequence % 64)) != 0)
    }

    /// Returns the exact number of represented sequences.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no sequence is represented.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.words.iter().flat_map(|(word_index, word)| {
            let base = word_index * 64;
            (0..64).filter_map(move |bit| (word & (1_u64 << bit) != 0).then_some(base + bit))
        })
    }
}

#[cfg(test)]
impl PartialEq<BTreeSet<u64>> for PendingSequenceBitmapV2 {
    fn eq(&self, other: &BTreeSet<u64>) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter().copied())
    }
}

/// Exact sequence sets used by every H2 conservation equation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingRegistrySequenceSetsV2 {
    /// All allocated advanced snapshots.
    pub advanced_with_snapshot: PendingSequenceBitmapV2,
    /// Registration successes.
    pub registration_succeeded: PendingSequenceBitmapV2,
    /// Registration failures.
    pub registration_failed: PendingSequenceBitmapV2,
    /// Successful registrations subsequently published.
    pub registered_published: PendingSequenceBitmapV2,
    /// Successful registrations with no receivers.
    pub registered_no_receivers: PendingSequenceBitmapV2,
    /// Failed registrations subsequently published.
    pub failed_registration_published: PendingSequenceBitmapV2,
    /// Failed registrations with no receivers.
    pub failed_registration_no_receivers: PendingSequenceBitmapV2,
    /// All published sends.
    pub send_published: PendingSequenceBitmapV2,
    /// All no-receiver sends.
    pub send_no_receivers: PendingSequenceBitmapV2,
    /// CLI lookup successes.
    pub cli_received_lookup_succeeded: PendingSequenceBitmapV2,
    /// CLI lookup failures.
    pub cli_registry_lookup_failed: PendingSequenceBitmapV2,
    /// Lag-attributed sequences.
    pub cli_lagged_attributed: PendingSequenceBitmapV2,
    /// Close-attributed sequences.
    pub cli_closed_attributed: PendingSequenceBitmapV2,
    /// Cancel-attributed sequences.
    pub cli_cancelled_attributed: PendingSequenceBitmapV2,
    /// Published sequences lacking a CLI terminal.
    pub pending_delivery_final: PendingSequenceBitmapV2,
    /// All CLI `Ok` receipts.
    pub cli_ok_received: PendingSequenceBitmapV2,
    /// Snapshot records installed before measurement lookup.
    pub snapshot_records_installed: PendingSequenceBitmapV2,
    /// Failed registrations ending in lookup failure.
    pub failed_reg_cli_registry_lookup_failed: PendingSequenceBitmapV2,
    /// Failed registrations attributed to lag.
    pub failed_reg_cli_lagged_attributed: PendingSequenceBitmapV2,
    /// Failed registrations attributed to close.
    pub failed_reg_cli_closed_attributed: PendingSequenceBitmapV2,
    /// Failed registrations attributed to cancellation.
    pub failed_reg_cli_cancelled_attributed: PendingSequenceBitmapV2,
    /// Failed registrations lacking a terminal.
    pub failed_reg_pending_final: PendingSequenceBitmapV2,
    /// Failed registrations with no receivers.
    pub registration_failed_no_receivers: PendingSequenceBitmapV2,
    /// Forbidden failed-registration lookup-success intersection.
    pub failed_reg_cli_received_lookup_succeeded: PendingSequenceBitmapV2,
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
    /// A durability acknowledgement did not match the bounded FIFO head.
    DurabilityAckOrderMismatch(u64),
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
    /// Exact cumulative number of non-authority send dispositions.
    pub unregistered_send_count: u64,
    /// Publications terminalized without a durable terminal record because capacity was exhausted.
    pub terminal_record_capacity_missing: u64,
    /// Publications terminalized without a durable terminal record because allocation failed.
    pub terminal_record_allocation_missing: u64,
    /// Exact cleanup receipts for terminals that can never receive durable H2.
    terminal_exclusions: VecDeque<PendingTerminalExclusionV2>,
    /// Visible disposition for every non-authority broadcast send attempt.
    pub unregistered_send_records: Vec<(PendingSendJournalMarkerV2, PendingSendDispositionV2)>,
    /// Terminal records accepted by the coverage queue.
    pub terminal_records: Vec<PendingTerminalRecordV2>,
    /// Pending-snapshot sequence to terminal-record position for exact logarithmic lookup.
    pub terminal_record_index: HashMap<u64, usize>,
    /// FIFO `(coverage sequence, pending snapshot sequence)` bindings awaiting durability.
    pub durability_pending: VecDeque<(u64, u64)>,
    /// Number of ordered durability acknowledgements accepted.
    pub durability_acked: usize,
    /// Fixed diagnostic ring of cleanup order evidence.
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
    terminal_record_capacity: usize,
    state: Mutex<PendingRegistryStateV2>,
    send_recorded: Condvar,
}

impl PendingMetadataRegistryV2 {
    /// Creates a registry with the owner-reviewed cumulative terminal bound.
    pub fn new(producer_epoch: u64, capacity: usize) -> Self {
        Self::new_bounded(producer_epoch, capacity, PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2)
    }

    /// Creates a registry with explicit already-validated live and cumulative bounds.
    pub fn new_bounded(
        producer_epoch: u64,
        capacity: usize,
        terminal_record_capacity: usize,
    ) -> Self {
        Self {
            producer_epoch,
            capacity,
            terminal_record_capacity,
            state: Mutex::new(PendingRegistryStateV2 {
                next_sequence: 0,
                next_coverage_sequence: 0,
                primary: BTreeMap::new(),
                secondary: HashMap::new(),
                published: VecDeque::new(),
                send_journal: VecDeque::new(),
                unregistered_send_inflight: 0,
                unregistered_send_count: 0,
                terminal_record_capacity_missing: 0,
                terminal_record_allocation_missing: 0,
                terminal_exclusions: VecDeque::with_capacity(capacity),
                unregistered_send_records: Vec::new(),
                terminal_records: Vec::new(),
                terminal_record_index: HashMap::new(),
                durability_pending: VecDeque::new(),
                durability_acked: 0,
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
        let cumulative_sequence_capacity =
            self.terminal_record_capacity.saturating_add(self.capacity);
        if usize::try_from(state.next_sequence)
            .map_or(true, |sequence| sequence >= cumulative_sequence_capacity)
        {
            let failure =
                match Self::increment(&mut state, PendingAccountingFieldV2::RegistrationFailed) {
                    Ok(()) => PendingRegistrationFailure::PendingRegistryCapacityOverflow,
                    Err(field) => PendingRegistrationFailure::PendingAccountingOverflow(field),
                };
            return PendingRegistrationAttemptV2 {
                pending_snapshot_sequence: None,
                disposition: PendingRegistrationDispositionV2::Failed(failure),
            };
        }
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

        if state.primary.len() >= self.capacity {
            if let Err(field) =
                Self::increment(&mut state, PendingAccountingFieldV2::RegistrationFailed)
            {
                Self::poison(&mut state, PendingRegistryPoisonV2::AccountingOverflow(field));
            }
            Self::insert(&mut state, sequence, |sets| &mut sets.registration_failed);
            return PendingRegistrationAttemptV2 {
                pending_snapshot_sequence: Some(sequence),
                disposition: PendingRegistrationDispositionV2::Failed(
                    PendingRegistrationFailure::PendingRegistryCapacityOverflow,
                ),
            };
        }
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
            PendingRegistrationDispositionV2::Failed(reason) => {
                if matches!(
                    reason,
                    PendingRegistrationFailure::PendingSnapshotSequenceOverflow
                        | PendingRegistrationFailure::PendingAccountingOverflow(_)
                        | PendingRegistrationFailure::PendingRegistryLockPoisoned
                        | PendingRegistrationFailure::PendingPointerBindingConflict
                ) {
                    state.poisoned = true;
                }
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
        state.unregistered_send_count = state.unregistered_send_count.checked_add(1).ok_or(
            PendingRegistryError::AccountingOverflow(PendingAccountingFieldV2::SendPublished),
        )?;
        Self::push_diagnostic(&mut state.unregistered_send_records, (marker, send));
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
        if matches!(
            attempt.disposition,
            PendingRegistrationDispositionV2::Failed(
                PendingRegistrationFailure::PendingRegistryCapacityOverflow
            )
        ) && !state.primary.contains_key(&sequence)
        {
            if receiver_count.is_some() {
                state.send_journal.push_back(
                    PendingSendJournalEntryV2::RegistrationFailedWithoutSequence(
                        PendingRegistrationFailure::PendingRegistryCapacityOverflow,
                    ),
                );
            }
            drop(state);
            self.send_recorded.notify_all();
            return Ok(());
        }
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
            if matches!(
                reason,
                CliRegistryLookupFailureReason::MissingPrimaryEntry
                    | CliRegistryLookupFailureReason::PendingPointerBindingConflict
                    | CliRegistryLookupFailureReason::PendingArcIdentityMismatch
                    | CliRegistryLookupFailureReason::PendingPublicSubsetCorruption
                    | CliRegistryLookupFailureReason::PendingAccountingOverflow(_)
            ) {
                state.poisoned = true;
            }
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
            Self::poison(
                &mut state,
                PendingRegistryPoisonV2::DurabilityAckOrderMismatch(coverage_sequence),
            );
            return Err(PendingRegistryError::DurabilityAckOrderMismatch);
        };
        if queued_coverage != coverage_sequence {
            Self::poison(
                &mut state,
                PendingRegistryPoisonV2::DurabilityAckOrderMismatch(coverage_sequence),
            );
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
                    PendingRegistryPoisonV2::DurabilityAckOrderMismatch(pending_sequence),
                );
                return Err(PendingRegistryError::DurabilityAckOrderMismatch);
            }
            sequences.pop_front();
            Some(sequences.is_empty())
        } else {
            None
        };

        state.durability_pending.pop_front();
        state.durability_acked = state
            .durability_acked
            .checked_add(1)
            .ok_or(PendingRegistryError::CoverageSequenceOverflow)?;
        Self::push_cleanup(
            &mut state,
            PendingCleanupEventV2::DurabilityAcknowledged(coverage_sequence),
        );
        if let Some(remove_secondary_key) = remove_secondary_key {
            Self::push_cleanup(
                &mut state,
                PendingCleanupEventV2::SecondaryRemoved(pending_sequence),
            );
            if remove_secondary_key {
                state.secondary.remove(&pointer);
            }
        }
        let entry = state
            .primary
            .remove(&pending_sequence)
            .ok_or(PendingRegistryError::MissingPrimaryEntry)?;
        Self::push_cleanup(&mut state, PendingCleanupEventV2::PrimaryRemoved(pending_sequence));
        drop(entry.retained_weak);
        Self::push_cleanup(
            &mut state,
            PendingCleanupEventV2::RetainedWeakDropped(pending_sequence),
        );
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
            unregistered_send_count: state.unregistered_send_count,
            terminal_record_capacity_missing: state.terminal_record_capacity_missing,
            terminal_record_allocation_missing: state.terminal_record_allocation_missing,
            coverage_queue_pending_ack: state.durability_pending.len(),
            durability_acked: state.durability_acked,
            terminal_records: state.terminal_records.len(),
            coverage_count: state.next_coverage_sequence,
            last_coverage_sequence: state.next_coverage_sequence.checked_sub(1),
            last_pending_snapshot_sequence: state.next_sequence.checked_sub(1),
            poisoned: state.poisoned || was_poisoned,
        }
    }

    #[cfg(test)]
    /// Returns terminal records at or after `start_coverage_sequence` in acceptance order.
    ///
    /// Only the unread suffix is cloned.
    pub fn terminal_records_from(
        &self,
        start_coverage_sequence: u64,
    ) -> Vec<PendingTerminalRecordV2> {
        let state = self.lock_state().0;
        let start = usize::try_from(start_coverage_sequence)
            .unwrap_or(usize::MAX)
            .min(state.terminal_records.len());
        state.terminal_records[start..].to_vec()
    }

    /// Returns the exact terminal for one pending snapshot sequence in logarithmic time.
    pub fn terminal_record(
        &self,
        pending_snapshot_sequence: u64,
    ) -> Option<PendingTerminalRecordV2> {
        let state = self.lock_state().0;
        let index = *state.terminal_record_index.get(&pending_snapshot_sequence)?;
        state.terminal_records.get(index).copied()
    }

    /// Takes one exact H2 exclusion once for recorder-owned cleanup.
    fn take_terminal_exclusion(&self) -> Option<PendingTerminalExclusionV2> {
        self.lock_state().0.terminal_exclusions.pop_front()
    }

    #[cfg(test)]
    /// Returns cleanup-order evidence.
    pub fn cleanup_events(&self) -> Vec<PendingCleanupEventV2> {
        self.lock_state().0.cleanup_events.clone()
    }

    /// Returns cutoff readiness from live/pending state only.
    ///
    /// Historical sequence conservation is intentionally verified once by
    /// `verify_final_seal`; this predicate is safe for the 10ms cutoff poll.
    pub fn cutoff_drain_ready(&self) -> bool {
        let (state, was_poisoned) = self.lock_state();
        !was_poisoned
            && !state.poisoned
            && state.poisons.is_empty()
            && state.sets.pending_delivery_final.is_empty()
            && state.sets.failed_reg_pending_final.is_empty()
            && state.durability_pending.is_empty()
            && state.terminal_records.len() == state.durability_acked
            && state.terminal_exclusions.is_empty()
            && state.primary.is_empty()
            && state.secondary.is_empty()
            && state.published.is_empty()
            && state.send_journal.is_empty()
            && state.unregistered_send_inflight == 0
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
        let terminal_index_matches = state.terminal_record_index.len()
            == state.terminal_records.len()
            && state.terminal_record_index.iter().all(|(pending_sequence, index)| {
                state.terminal_records.get(*index).is_some_and(|record| {
                    record.metadata.identity.pending_snapshot_sequence == *pending_sequence
                })
            });
        if !coverage_cursor_matches || !terminal_index_matches {
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
            || state.terminal_records.len() != state.durability_acked
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

    fn exclude_terminal_record_locked(
        state: &mut PendingRegistryStateV2,
        sequence: u64,
        terminal: PendingCliTerminalV2,
        capacity_exhausted: bool,
    ) -> Result<(), PendingRegistryError> {
        let next_missing = if capacity_exhausted {
            state.terminal_record_capacity_missing.checked_add(1)
        } else {
            state.terminal_record_allocation_missing.checked_add(1)
        };
        let Some(next_missing) = next_missing else {
            Self::poison(state, PendingRegistryPoisonV2::SequenceOverflow);
            return Err(PendingRegistryError::CoverageSequenceOverflow);
        };
        let entry =
            state.primary.remove(&sequence).ok_or(PendingRegistryError::MissingPrimaryEntry)?;
        if entry.registration == PendingRegistrationDispositionV2::Succeeded {
            let pointer = entry.metadata.identity.arc_pointer_identity;
            let sequences = state
                .secondary
                .get_mut(&pointer)
                .ok_or(PendingRegistryError::MissingPrimaryEntry)?;
            sequences.retain(|candidate| *candidate != sequence);
            if sequences.is_empty() {
                state.secondary.remove(&pointer);
            }
        }
        state.sets.pending_delivery_final.remove(&sequence);
        state.sets.failed_reg_pending_final.remove(&sequence);
        match terminal {
            PendingCliTerminalV2::CliLagged => {
                Self::insert(state, sequence, |sets| &mut sets.cli_lagged_attributed);
                if matches!(entry.registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_lagged_attributed
                    });
                }
            }
            PendingCliTerminalV2::CliClosed => {
                Self::insert(state, sequence, |sets| &mut sets.cli_closed_attributed);
                if matches!(entry.registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_closed_attributed
                    });
                }
            }
            PendingCliTerminalV2::CliCancelled => {
                Self::insert(state, sequence, |sets| &mut sets.cli_cancelled_attributed);
                if matches!(entry.registration, PendingRegistrationDispositionV2::Failed(_)) {
                    Self::insert(state, sequence, |sets| {
                        &mut sets.failed_reg_cli_cancelled_attributed
                    });
                }
            }
            _ => {}
        }
        state.terminal_exclusions.push_back(PendingTerminalExclusionV2 {
            metadata: entry.metadata,
            terminal,
            reason: if capacity_exhausted {
                PendingTerminalExclusionReasonV2::Capacity
            } else {
                PendingTerminalExclusionReasonV2::Allocation
            },
        });
        if capacity_exhausted {
            state.terminal_record_capacity_missing = next_missing;
        } else {
            state.terminal_record_allocation_missing = next_missing;
        }
        drop(entry.retained_weak);
        Ok(())
    }

    /// Appends an exact terminal and queue acceptance without removing identity state.
    pub fn terminalize_locked(
        &self,
        state: &mut PendingRegistryStateV2,
        sequence: u64,
        terminal: PendingCliTerminalV2,
    ) -> Result<(), PendingRegistryError> {
        let (metadata, registration, send) = {
            let Some(entry) = state.primary.get(&sequence) else {
                state.poisoned = true;
                return Err(PendingRegistryError::MissingPrimaryEntry);
            };
            if entry.terminal.is_some() || state.terminal_record_index.contains_key(&sequence) {
                Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
                return Err(PendingRegistryError::DuplicateTerminal);
            }
            (
                entry.metadata,
                entry.registration,
                entry.send.ok_or(PendingRegistryError::MissingPrimaryEntry)?,
            )
        };
        if state.terminal_records.len() >= self.terminal_record_capacity {
            return Self::exclude_terminal_record_locked(state, sequence, terminal, true);
        }
        let next_coverage_sequence =
            state.next_coverage_sequence.checked_add(1).ok_or_else(|| {
                Self::poison(state, PendingRegistryPoisonV2::SequenceOverflow);
                PendingRegistryError::CoverageSequenceOverflow
            })?;
        if state.terminal_records.try_reserve(1).is_err()
            || state.terminal_record_index.try_reserve(1).is_err()
            || state.durability_pending.try_reserve(1).is_err()
        {
            return Self::exclude_terminal_record_locked(state, sequence, terminal, false);
        }
        state
            .primary
            .get_mut(&sequence)
            .expect("entry checked while registry lock is held")
            .terminal = Some(terminal);
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
        state.next_coverage_sequence = next_coverage_sequence;
        Self::push_cleanup(state, PendingCleanupEventV2::TerminalAppended(sequence));
        let terminal_index = state.terminal_records.len();
        if state.terminal_record_index.insert(sequence, terminal_index).is_some() {
            Self::poison(state, PendingRegistryPoisonV2::BindingConflict(sequence));
            return Err(PendingRegistryError::DuplicateTerminal);
        }
        state.terminal_records.push(PendingTerminalRecordV2 {
            coverage_sequence,
            metadata,
            registration,
            send,
            terminal,
        });
        state.durability_pending.push_back((coverage_sequence, sequence));
        Self::push_cleanup(state, PendingCleanupEventV2::CoverageQueueAccepted(coverage_sequence));
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
        select: impl FnOnce(&mut PendingRegistrySequenceSetsV2) -> &mut PendingSequenceBitmapV2,
    ) {
        if !select(&mut state.sets).insert(sequence) {
            Self::poison(state, PendingRegistryPoisonV2::DuplicateSequence(sequence));
        }
    }

    fn poison(state: &mut PendingRegistryStateV2, poison: PendingRegistryPoisonV2) {
        state.poisoned = true;
        Self::push_diagnostic(&mut state.poisons, poison);
    }
    fn push_cleanup(state: &mut PendingRegistryStateV2, event: PendingCleanupEventV2) {
        Self::push_diagnostic(&mut state.cleanup_events, event);
    }

    fn push_diagnostic<T>(events: &mut Vec<T>, event: T) {
        if events.len() == PENDING_DIAGNOSTIC_RING_CAPACITY_V2 {
            events.remove(0);
        }
        events.push(event);
    }

    fn partition(
        whole: &PendingSequenceBitmapV2,
        parts: &[&PendingSequenceBitmapV2],
    ) -> Result<(), PendingFinalSealErrorV2> {
        let mut union = PendingSequenceBitmapV2::default();
        for part in parts {
            for sequence in part.iter() {
                if !union.insert(sequence) {
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
        recorded: &PendingSequenceBitmapV2,
        left: &PendingSequenceBitmapV2,
        right: &PendingSequenceBitmapV2,
    ) -> Result<(), PendingFinalSealErrorV2> {
        let mut expected = PendingSequenceBitmapV2::default();
        for sequence in left.iter().filter(|sequence| right.contains(sequence)) {
            expected.insert(sequence);
        }
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
    /// Exact cumulative number of non-authority send dispositions.
    pub unregistered_send_count: u64,
    /// Publications released after terminal-record capacity exhaustion.
    pub terminal_record_capacity_missing: u64,
    /// Publications released after terminal-record allocation exhaustion.
    pub terminal_record_allocation_missing: u64,
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
    /// Owner-reviewed cumulative terminal-record capacity.
    pub terminal_record_capacity: usize,
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

/// Merged S2 terminal vocabulary for the authoritative source-coverage ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCoverageTerminalV3 {
    /// Wire bytes failed decode.
    DecodeRejected,
    /// Actor enqueue closed before delivery.
    ActorEnqueueClosed,
    /// Actor mailbox closed while a pending item was retained.
    ActorMailboxClosedWithPending,
    /// State queue closed before processor ownership.
    StateQueueClosed,
    /// State processor queue closed while a pending item was retained.
    StateProcessorQueueClosedWithPending,
    /// Cached input resolved to processor ownership.
    CacheResolved,
    /// Cached input was replaced.
    CacheReplaced,
    /// Cached input was evicted.
    CacheEvicted,
    /// Cache rejected the input.
    CacheRejected,
    /// Cache ownership remained unresolved at cutoff.
    CachedUnresolvedAtCutoff,
    /// Processor emitted its terminal product.
    ProcessorProduct,
    /// CLI lookup succeeded.
    CliReceivedLookupSucceeded,
    /// CLI registry lookup failed.
    CliRegistryLookupFailed,
    /// CLI lagged the terminal.
    CliLagged,
    /// CLI closed the terminal.
    CliClosed,
    /// CLI cancelled the terminal.
    CliCancelled,
    /// Publication had no receivers.
    NoReceivers,
    /// Post-cutoff traffic was routed without authority.
    CutoffRouted,
}

impl SourceCoverageTerminalV3 {
    /// Exact merged S2 wire label.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::DecodeRejected => "DecodeRejected",
            Self::ActorEnqueueClosed => "ActorEnqueueClosed",
            Self::ActorMailboxClosedWithPending => "ActorMailboxClosedWithPending",
            Self::StateQueueClosed => "StateQueueClosed",
            Self::StateProcessorQueueClosedWithPending => "StateProcessorQueueClosedWithPending",
            Self::CacheResolved => "CacheResolved",
            Self::CacheReplaced => "CacheReplaced",
            Self::CacheEvicted => "CacheEvicted",
            Self::CacheRejected => "CacheRejected",
            Self::CachedUnresolvedAtCutoff => "CachedUnresolvedAtCutoff",
            Self::ProcessorProduct => "ProcessorProduct",
            Self::CliReceivedLookupSucceeded => "CliReceivedLookupSucceeded",
            Self::CliRegistryLookupFailed => "CliRegistryLookupFailed",
            Self::CliLagged => "CliLagged",
            Self::CliClosed => "CliClosed",
            Self::CliCancelled => "CliCancelled",
            Self::NoReceivers => "NoReceivers",
            Self::CutoffRouted => "CutoffRouted",
        }
    }
}

/// Truthful terminal evidence awaiting the CLI-owned strict S2 chain envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTerminalCoverageV3 {
    /// Producer epoch.
    pub producer_epoch: u64,
    /// Sequence in the authority or exclusion terminal ledger.
    pub coverage_sequence: u64,
    /// Route fixed at wire admission.
    pub route: EpochRouteV1,
    /// Source generation when one was allocated.
    pub source_generation: Option<u64>,
    /// Exact terminal class.
    pub terminal: SourceCoverageTerminalV3,
    /// Hash of the terminal evidence.
    pub terminal_hash: B256,
    /// Payload-first authority hash when available.
    pub payload_first_record_hash: Option<B256>,
    /// Pending snapshot sequence when applicable.
    pub pending_snapshot_sequence: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct SourceTerminalEvidenceV3 {
    route: EpochRouteV1,
    source_generation: Option<u64>,
    terminal: SourceCoverageTerminalV3,
    terminal_hash: B256,
    payload_first_record_hash: Option<B256>,
    pending_snapshot_sequence: Option<u64>,
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
    /// Strict terminal evidence for the merged S2 source-coverage ledger.
    TerminalCoverage(SourceTerminalCoverageV3),
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
    /// Processor cutoff terminal.
    ProcessorUnresolvedAtCutoff,
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
            Self::ProcessorUnresolvedAtCutoff => "ProcessorUnresolvedAtCutoff",
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
    /// A checked missing-evidence counter overflowed.
    MissingEvidenceCountOverflow(EdgeMeasurementMissingEvidenceReasonV1),
    /// A durable event acknowledgement had no matching enqueued event.
    EventDurabilityAckUnderflow,
    /// Payload-first binding conflicted.
    PayloadFirstBindingConflict,
    /// A source generation reached a hook without its original identity.
    MissingSourceIdentity,
    /// More than one terminal product was attempted for a source generation.
    DuplicateProcessorTerminal,
    /// Checked terminal product count overflowed.
    ProcessorTerminalCountOverflow,
    /// The recorder mutex was recovered after a panic.
    RecorderLockPoisoned,
    /// The checked nonfatal authority decode-reject count overflowed.
    DecodeRejectedCountOverflow,
    /// A wire lifecycle transition was missing, duplicated, or out of order.
    WireLifecycleConflict,
    /// H3 received a duplicate, impossible, or unhandled transition.
    ConnectionTransitionConflict,
}

/// Named operational gaps that do not make the internally consistent ledger untrustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMeasurementMissingEvidenceReasonV1 {
    /// Active state exceeded its configured cap.
    ActiveStateCapacityOverflow,
    /// The bounded writer queue was full.
    EventQueueFull,
    /// The bounded writer queue was closed.
    EventQueueClosed,
    /// Cutoff was requested before a named source bound existed.
    CutoffBoundMissing,
    /// The coordinator could not durably complete an operational step.
    CoordinatorFailure,
    /// Index zero arrived after a higher index.
    PayloadIndexZeroLate,
    /// Canonical close or cutoff found a payload missing index zero.
    PayloadIndexZeroMissing,
    /// A source hook could not recover the original identity.
    MissingSourceIdentity,
    /// A contained observer panic omitted observer evidence.
    ObserverPanicked,
    /// Cache ownership remained unresolved at cutoff.
    CacheDrainIncomplete,
    /// A cache-owned generation remained unresolved at the drain deadline.
    CutoffDeadlineCacheGeneration,
    /// A processor-owned generation remained unresolved at the drain deadline.
    CutoffDeadlineProcessorGeneration,
    /// A processor completed after the cutoff deadline already claimed its generation.
    LateProcessorCompletion,
    /// H2 terminal capacity excluded a recorder binding from durable completion.
    TerminalRecordCapacityExcluded,
    /// H2 terminal allocation excluded a recorder binding from durable completion.
    TerminalRecordAllocationExcluded,
}

/// Bounded exact counters for coordinator-supplied static reason names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EdgeCoordinatorMissingEvidenceCountsV1 {
    entries: Vec<(&'static str, u64)>,
}
impl EdgeCoordinatorMissingEvidenceCountsV1 {
    fn record(&mut self, reason: &'static str) -> Result<(), ()> {
        if let Some((_, count)) = self.entries.iter_mut().find(|(name, _)| *name == reason) {
            *count = count.checked_add(1).ok_or(())?;
            return Ok(());
        }
        if self.entries.len() == EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1 {
            return Err(());
        }
        self.entries.push((reason, 1));
        Ok(())
    }

    /// Returns all exact static reason names and cumulative counts.
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        self.entries.clone()
    }

    /// Returns the exact cumulative count for one static reason name.
    pub fn count(&self, reason: &'static str) -> u64 {
        self.entries
            .iter()
            .find_map(|(name, count)| (*name == reason).then_some(*count))
            .unwrap_or(0)
    }
}

/// Exact cumulative missing-evidence accounting with bounded diagnostic samples.
#[derive(Debug, Default, Clone)]
pub struct EdgeMeasurementMissingEvidenceV1 {
    /// Exact total number of operational evidence gaps.
    pub total: u64,
    /// Active-state capacity exclusions.
    pub active_state_capacity_overflow: u64,
    /// Writer queue-full exclusions.
    pub event_queue_full: u64,
    /// Writer queue-closed exclusions.
    pub event_queue_closed: u64,
    /// Missing cutoff-bound exclusions.
    pub cutoff_bound_missing: u64,
    /// Coordinator operational-failure exclusions.
    pub coordinator_failure: u64,
    /// Late index-zero exclusions.
    pub payload_index_zero_late: u64,
    /// Missing index-zero exclusions.
    pub payload_index_zero_missing: u64,
    /// Missing source-identity exclusions.
    pub missing_source_identity: u64,
    /// Observer-panic exclusions.
    pub observer_panicked: u64,
    /// Incomplete cache-drain exclusions.
    pub cache_drain_incomplete: u64,
    /// Cache-owner deadline exclusions.
    pub cutoff_deadline_cache_generation: u64,
    /// Processor-owner deadline exclusions.
    pub cutoff_deadline_processor_generation: u64,
    /// Processor completions observed after deadline ownership.
    pub late_processor_completion: u64,
    /// Recorder bindings released after H2 terminal-capacity exclusion.
    pub terminal_record_capacity_excluded: u64,
    /// Recorder bindings released after H2 terminal-allocation exclusion.
    pub terminal_record_allocation_excluded: u64,
    /// Bounded exact coordinator reason-name counters.
    pub coordinator_reasons: EdgeCoordinatorMissingEvidenceCountsV1,
    samples: Vec<EdgeMeasurementMissingEvidenceReasonV1>,
}
impl EdgeMeasurementMissingEvidenceV1 {
    fn record(
        &mut self,
        reason: EdgeMeasurementMissingEvidenceReasonV1,
    ) -> Result<(), EdgeMeasurementMissingEvidenceReasonV1> {
        let count = match reason {
            EdgeMeasurementMissingEvidenceReasonV1::ActiveStateCapacityOverflow => {
                &mut self.active_state_capacity_overflow
            }
            EdgeMeasurementMissingEvidenceReasonV1::EventQueueFull => &mut self.event_queue_full,
            EdgeMeasurementMissingEvidenceReasonV1::EventQueueClosed => {
                &mut self.event_queue_closed
            }
            EdgeMeasurementMissingEvidenceReasonV1::CutoffBoundMissing => {
                &mut self.cutoff_bound_missing
            }
            EdgeMeasurementMissingEvidenceReasonV1::CoordinatorFailure => {
                &mut self.coordinator_failure
            }
            EdgeMeasurementMissingEvidenceReasonV1::PayloadIndexZeroLate => {
                &mut self.payload_index_zero_late
            }
            EdgeMeasurementMissingEvidenceReasonV1::PayloadIndexZeroMissing => {
                &mut self.payload_index_zero_missing
            }
            EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity => {
                &mut self.missing_source_identity
            }
            EdgeMeasurementMissingEvidenceReasonV1::ObserverPanicked => &mut self.observer_panicked,
            EdgeMeasurementMissingEvidenceReasonV1::CacheDrainIncomplete => {
                &mut self.cache_drain_incomplete
            }
            EdgeMeasurementMissingEvidenceReasonV1::CutoffDeadlineCacheGeneration => {
                &mut self.cutoff_deadline_cache_generation
            }
            EdgeMeasurementMissingEvidenceReasonV1::CutoffDeadlineProcessorGeneration => {
                &mut self.cutoff_deadline_processor_generation
            }
            EdgeMeasurementMissingEvidenceReasonV1::LateProcessorCompletion => {
                &mut self.late_processor_completion
            }
            EdgeMeasurementMissingEvidenceReasonV1::TerminalRecordCapacityExcluded => {
                &mut self.terminal_record_capacity_excluded
            }
            EdgeMeasurementMissingEvidenceReasonV1::TerminalRecordAllocationExcluded => {
                &mut self.terminal_record_allocation_excluded
            }
        };
        let next_count = count.checked_add(1).ok_or(reason)?;
        let next_total = self.total.checked_add(1).ok_or(reason)?;
        *count = next_count;
        self.total = next_total;
        if self.samples.len() == EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1 {
            self.samples.remove(0);
        }
        self.samples.push(reason);
        Ok(())
    }

    #[cfg(test)]
    const fn sample_len(&self) -> usize {
        self.samples.len()
    }
}

/// Fixed maximum number of recorder poison diagnostic samples.
const EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1: usize = 64;

/// Sticky exact poison accounting with bounded diagnostic samples.
#[derive(Debug, Default)]
pub struct EdgeMeasurementPoisonsV1 {
    samples: Vec<EdgeMeasurementPoisonV1>,
    total: usize,
}

impl EdgeMeasurementPoisonsV1 {
    fn push(&mut self, poison: EdgeMeasurementPoisonV1) {
        self.total = self.total.saturating_add(1);
        if self.samples.len() == EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1 {
            self.samples.remove(0);
        }
        self.samples.push(poison);
    }

    #[cfg(test)]
    fn contains(&self, poison: &EdgeMeasurementPoisonV1) -> bool {
        self.samples.contains(poison)
    }

    const fn is_empty(&self) -> bool {
        self.total == 0
    }

    const fn len(&self) -> usize {
        self.total
    }
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
    /// Number of live decoded generations retaining each payload binding.
    payload_generation_refs: HashMap<PayloadFirstKeyV1, usize>,
    /// Payloads with explicit canonical/cutoff close evidence awaiting their final generation.
    payload_close_pending: BTreeSet<PayloadFirstKeyV1>,
    /// Payloads first seen above index zero.
    pub payload_without_index_zero: BTreeSet<PayloadFirstKeyV1>,
    /// FIFO source generations awaiting processor admission by structural key.
    pub decoded_source_generations: HashMap<DecodedFlashblockKeyV1, VecDeque<u64>>,
    source_generation_contexts: HashMap<u64, SourceGenerationContextV1>,
    cache_pending: BTreeMap<u64, ProcessorBaseDispositionV1>,
    processor_terminal_count: u64,
    snapshot_products: BTreeMap<u64, ProcessorLifecycleProductV1>,
    payload_first_by_generation: BTreeMap<u64, PayloadFirstObservationV1>,
    snapshot_wire_ordinals: BTreeMap<u64, (u64, u64)>,
    snapshot_evidence_captured: HashSet<u64>,
    /// Generations atomically claimed by cutoff deadline ownership.
    deadline_terminalized_generations: BTreeSet<u64>,
    /// Pending sequences explicitly excluded from durable H2.
    excluded_pending_snapshot_sequences: BTreeSet<u64>,
    wire_lifecycle: BTreeMap<u64, WireLifecyclePhaseV1>,
    generation_wire_ordinals: BTreeMap<u64, u64>,
    authority_wire_terminals: u64,
    authority_decode_rejected: u64,
    next_coverage_sequence: u64,
    next_post_cutoff_wire_ordinal: u64,
    last_coverage_hash: B256,
    coverage_record_count: u64,
    next_excluded_coverage_sequence: u64,
    last_excluded_coverage_hash: B256,
    excluded_coverage_record_count: u64,
    next_terminal_coverage_sequence: u64,
    next_excluded_terminal_coverage_sequence: u64,
    post_cutoff_routes: HashMap<DecodedFlashblockKeyV1, VecDeque<EpochAdmissionTokenV1>>,
    cutoff_fence: bool,
    connection_phase: ConnectionPhaseV1,
    /// H3 phase represented by the retained count/hash chain.
    connection_recorded_phase: ConnectionPhaseV1,
    connection_record_count: u64,
    connection_established_count: u64,
    connection_closed_count: u64,
    last_connection_record: Option<SourceConnectionRecordV1>,
    previous_connection_record: Option<SourceConnectionRecordV1>,
    #[cfg(test)]
    /// Ordered bounded connection transitions.
    pub connection_records: VecDeque<SourceConnectionRecordV1>,
    /// Last connection hash.
    pub last_connection_hash: B256,
    /// Named poison observations.
    pub poisons: EdgeMeasurementPoisonsV1,
    /// Named, checked operational missing-evidence accounting.
    pub missing_evidence: EdgeMeasurementMissingEvidenceV1,
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
    #[cfg(feature = "test-utils")]
    deterministic_clock: bool,
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
        Self::new_inner(
            config,
            #[cfg(feature = "test-utils")]
            false,
        )
    }

    /// Creates a recorder with fixed clock and boot evidence for deterministic production-path
    /// fixture regeneration. This surface is absent from production builds.
    #[cfg(feature = "test-utils")]
    pub fn new_deterministic_test(
        config: EdgeMeasurementInstallConfigV1,
    ) -> Result<Arc<Self>, EdgeMeasurementInstallErrorV1> {
        Self::new_inner(config, true)
    }

    fn new_inner(
        config: EdgeMeasurementInstallConfigV1,
        #[cfg(feature = "test-utils")] deterministic_clock: bool,
    ) -> Result<Arc<Self>, EdgeMeasurementInstallErrorV1> {
        if config.event_queue_capacity == 0
            || config.active_state_capacity == 0
            || config.pending_registry_capacity == 0
            || config.terminal_record_capacity == 0
        {
            return Err(EdgeMeasurementInstallErrorV1::ZeroCapacity);
        }
        if config.event_queue_capacity > EDGE_EVENT_QUEUE_CAPACITY_MAX_V1
            || config.active_state_capacity > EDGE_ACTIVE_STATE_CAPACITY_MAX_V1
            || config.pending_registry_capacity > PENDING_REGISTRY_CAPACITY_V2
            || config.terminal_record_capacity > PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2
        {
            return Err(EdgeMeasurementInstallErrorV1::CapacityTooLarge);
        }
        #[cfg(feature = "test-utils")]
        let (boot_id, realtime_resolution_ns, monotonic_resolution_ns) = if deterministic_clock {
            (*b"00000000-0000-0000-0000-000000000001", 1, 1)
        } else {
            Self::host_clock_identity()?
        };
        #[cfg(not(feature = "test-utils"))]
        let (boot_id, realtime_resolution_ns, monotonic_resolution_ns) =
            Self::host_clock_identity()?;
        let (event_sender, event_receiver) = sync_channel(config.event_queue_capacity);
        let recorder = Arc::new(Self {
            config,
            boot_id,
            realtime_resolution_ns,
            monotonic_resolution_ns,
            #[cfg(feature = "test-utils")]
            deterministic_clock,
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
                payload_generation_refs: HashMap::new(),
                payload_close_pending: BTreeSet::new(),
                payload_without_index_zero: BTreeSet::new(),
                decoded_source_generations: HashMap::new(),
                source_generation_contexts: HashMap::new(),
                cache_pending: BTreeMap::new(),
                processor_terminal_count: 0,
                snapshot_products: BTreeMap::new(),
                payload_first_by_generation: BTreeMap::new(),
                snapshot_wire_ordinals: BTreeMap::new(),
                snapshot_evidence_captured: HashSet::new(),
                deadline_terminalized_generations: BTreeSet::new(),
                excluded_pending_snapshot_sequences: BTreeSet::new(),
                wire_lifecycle: BTreeMap::new(),
                generation_wire_ordinals: BTreeMap::new(),
                authority_wire_terminals: 0,
                authority_decode_rejected: 0,
                next_coverage_sequence: 0,
                next_post_cutoff_wire_ordinal: 0,
                last_coverage_hash: B256::ZERO,
                coverage_record_count: 0,
                next_excluded_coverage_sequence: 0,
                last_excluded_coverage_hash: B256::ZERO,
                excluded_coverage_record_count: 0,
                next_terminal_coverage_sequence: 0,
                next_excluded_terminal_coverage_sequence: 0,
                post_cutoff_routes: HashMap::new(),
                cutoff_fence: false,
                connection_phase: ConnectionPhaseV1::New,
                connection_recorded_phase: ConnectionPhaseV1::New,
                connection_record_count: 0,
                connection_established_count: 0,
                connection_closed_count: 0,
                last_connection_record: None,
                previous_connection_record: None,
                #[cfg(test)]
                connection_records: VecDeque::new(),
                last_connection_hash: B256::ZERO,
                poisons: EdgeMeasurementPoisonsV1::default(),
                missing_evidence: EdgeMeasurementMissingEvidenceV1::default(),
                cutoff: None,
                event_pending_ack: 0,
            }),
            registry: Arc::new(PendingMetadataRegistryV2::new_bounded(
                config.producer_epoch.get(),
                config.pending_registry_capacity,
                config.terminal_record_capacity,
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

    fn host_clock_identity() -> Result<([u8; 36], u64, u64), EdgeMeasurementInstallErrorV1> {
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
        Ok((
            boot_id,
            Self::resolution(ClockIdV1::Realtime)?,
            Self::resolution(ClockIdV1::Monotonic)?,
        ))
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

    fn sample_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
    ) -> Option<WireObservationV1> {
        let ordinal = state.next_clock_ordinal;
        state.next_clock_ordinal = match ordinal.checked_add(1) {
            Some(next) => next,
            None => {
                state.poisons.push(EdgeMeasurementPoisonV1::ClockOrdinalOverflow);
                return None;
            }
        };
        // Authority order is fixed: realtime is always called before monotonic.
        #[cfg(feature = "test-utils")]
        let ((utc_status, utc_ns), (mono_status, mono_ns)) = if self.deterministic_clock {
            let offset = ordinal.checked_mul(1_000).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::ClockOrdinalOverflow);
                u64::MAX
            });
            (
                (ClockStatusV1::Ok, 1_700_000_000_000_000_000_u64.checked_add(offset)),
                (ClockStatusV1::Ok, 1_000_000_000_u64.checked_add(offset)),
            )
        } else {
            (
                Self::raw_clock(ClockIdV1::Realtime, false),
                Self::raw_clock(ClockIdV1::Monotonic, false),
            )
        };
        #[cfg(not(feature = "test-utils"))]
        let ((utc_status, utc_ns), (mono_status, mono_ns)) = (
            Self::raw_clock(ClockIdV1::Realtime, false),
            Self::raw_clock(ClockIdV1::Monotonic, false),
        );
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
        let Some(mut observation) = self.sample_locked(state) else {
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
    fn catch_up_anchors_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        through_mono_ns: u64,
    ) {
        while through_mono_ns >= state.next_anchor_due_mono_ns {
            let previous_due = state.next_anchor_due_mono_ns;
            self.record_anchor_locked(state, false);
            if state.next_anchor_due_mono_ns == previous_due {
                break;
            }
        }
    }

    fn record_missing_evidence(
        state: &mut EdgeMeasurementRecorderStateV1,
        reason: EdgeMeasurementMissingEvidenceReasonV1,
    ) {
        if state.missing_evidence.record(reason).is_err() {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingEvidenceCountOverflow(reason));
        }
    }

    fn exclude_unpersisted_event(
        state: &mut EdgeMeasurementRecorderStateV1,
        event: EdgeSourceEventV1,
    ) {
        match event {
            EdgeSourceEventV1::PayloadFirst(record)
                if state.last_payload_first_hash == record.record_hash =>
            {
                state.next_payload_first_sequence = record.record_sequence;
                state.last_payload_first_hash = record.previous_record_hash;
                if state.payload_first.get(&record.key) == Some(&record) {
                    state.payload_first.remove(&record.key);
                }
            }
            EdgeSourceEventV1::Connection(record)
                if state.last_connection_hash == record.record_hash =>
            {
                state.next_connection_sequence = record.connection_sequence;
                state.last_connection_hash = record.previous_record_hash;
                if let Some(ordinal) = record.clock_observation_ordinal
                    && ordinal.checked_add(1) == Some(state.next_clock_ordinal)
                {
                    state.next_clock_ordinal = ordinal;
                }
                state.connection_record_count = state.connection_record_count.saturating_sub(1);
                if record.transition == SourceConnectionTransitionV1::Established {
                    state.connection_established_count =
                        state.connection_established_count.saturating_sub(1);
                }
                if matches!(
                    record.transition,
                    SourceConnectionTransitionV1::EstablishedClosedByClose
                        | SourceConnectionTransitionV1::EstablishedClosedByReadError
                        | SourceConnectionTransitionV1::EstablishedClosedByNoPong
                        | SourceConnectionTransitionV1::EstablishedClosedByPingWriteFailure
                        | SourceConnectionTransitionV1::EstablishedClosedByCutoff
                        | SourceConnectionTransitionV1::EstablishedClosedByShutdown
                ) {
                    state.connection_closed_count = state.connection_closed_count.saturating_sub(1);
                }
                state.last_connection_record = state.previous_connection_record;
                #[cfg(test)]
                {
                    state.connection_records.pop_back();
                }
            }
            EdgeSourceEventV1::ClockAnchor(record)
                if state.last_anchor_hash == record.record_hash =>
            {
                state.next_anchor_sequence = record.anchor_sequence;
                state.next_clock_ordinal = record.observation.clock_observation_ordinal;
                state.last_anchor_hash = record.previous_anchor_hash;
                state.next_anchor_due_mono_ns = record.due_mono_ns;
            }
            EdgeSourceEventV1::Coverage(record) => match record.route {
                EpochRouteV1::Authority if state.last_coverage_hash == record.record_hash => {
                    state.next_coverage_sequence = record.coverage_sequence;
                    state.coverage_record_count = state.coverage_record_count.saturating_sub(1);
                    state.last_coverage_hash = record.previous_record_hash;
                }
                EpochRouteV1::PostCutoffNonAuthority
                    if state.last_excluded_coverage_hash == record.record_hash =>
                {
                    state.next_excluded_coverage_sequence = record.coverage_sequence;
                    state.excluded_coverage_record_count =
                        state.excluded_coverage_record_count.saturating_sub(1);
                    state.last_excluded_coverage_hash = record.previous_record_hash;
                }
                _ => {}
            },
            EdgeSourceEventV1::TerminalCoverage(record) => match record.route {
                EpochRouteV1::Authority => {
                    state.next_terminal_coverage_sequence = record.coverage_sequence;
                }
                EpochRouteV1::PostCutoffNonAuthority => {
                    state.next_excluded_terminal_coverage_sequence = record.coverage_sequence;
                }
            },
            _ => {}
        }
    }

    fn enqueue(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        event: EdgeSourceEventV1,
    ) -> bool {
        match self.event_sender.try_send(event) {
            Ok(()) => {
                state.event_pending_ack =
                    state.event_pending_ack.checked_add(1).unwrap_or_else(|| {
                        state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
                        u64::MAX
                    });
                true
            }
            Err(TrySendError::Full(event)) => {
                Self::exclude_unpersisted_event(state, event);
                Self::record_missing_evidence(
                    state,
                    EdgeMeasurementMissingEvidenceReasonV1::EventQueueFull,
                );
                false
            }
            Err(TrySendError::Disconnected(event)) => {
                Self::exclude_unpersisted_event(state, event);
                Self::record_missing_evidence(
                    state,
                    EdgeMeasurementMissingEvidenceReasonV1::EventQueueClosed,
                );
                false
            }
        }
    }

    fn terminal_evidence_hash(
        &self,
        wire_ordinal: u64,
        source_generation: Option<u64>,
        terminal: SourceCoverageTerminalV3,
    ) -> B256 {
        let json = format!(
            "{{\"producerEpoch\":\"{}\",\"sourceGeneration\":{},\"terminal\":\"{}\",\"wireOrdinal\":\"{}\"}}",
            self.config.producer_epoch,
            source_generation
                .map(|value| format!("\"{value}\""))
                .unwrap_or_else(|| "null".to_owned()),
            terminal.wire_name(),
            wire_ordinal,
        );
        AuthorityRecordHasherV1::authority("edge-source-terminal-evidence/v1", &json)
    }

    fn terminal_coverage_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        evidence: SourceTerminalEvidenceV3,
    ) {
        let SourceTerminalEvidenceV3 {
            route,
            source_generation,
            terminal,
            terminal_hash,
            payload_first_record_hash,
            pending_snapshot_sequence,
        } = evidence;
        let sequence = match route {
            EpochRouteV1::Authority => state.next_terminal_coverage_sequence,
            EpochRouteV1::PostCutoffNonAuthority => state.next_excluded_terminal_coverage_sequence,
        };
        let Some(next) = sequence.checked_add(1) else {
            state.poisons.push(EdgeMeasurementPoisonV1::RecordSequenceOverflow);
            return;
        };
        match route {
            EpochRouteV1::Authority => state.next_terminal_coverage_sequence = next,
            EpochRouteV1::PostCutoffNonAuthority => {
                state.next_excluded_terminal_coverage_sequence = next;
            }
        }
        self.enqueue(
            state,
            EdgeSourceEventV1::TerminalCoverage(SourceTerminalCoverageV3 {
                producer_epoch: self.config.producer_epoch.get(),
                coverage_sequence: sequence,
                route,
                source_generation,
                terminal,
                terminal_hash,
                payload_first_record_hash,
                pending_snapshot_sequence,
            }),
        );
    }

    fn payload_hash_locked(
        state: &EdgeMeasurementRecorderStateV1,
        source_generation: u64,
    ) -> Option<B256> {
        let context = state.source_generation_contexts.get(&source_generation)?;
        state.payload_first.get(&context.payload_first_key).map(|record| record.record_hash)
    }

    fn exclude_active_generation_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        wire_ordinal: u64,
        source_generation: u64,
    ) {
        let terminal = SourceCoverageTerminalV3::ActorMailboxClosedWithPending;
        let terminal_hash =
            self.terminal_evidence_hash(wire_ordinal, Some(source_generation), terminal);
        let payload_first_record_hash = Self::payload_hash_locked(state, source_generation);
        self.terminal_coverage_locked(
            state,
            SourceTerminalEvidenceV3 {
                route: EpochRouteV1::Authority,
                source_generation: Some(source_generation),
                terminal,
                terminal_hash,
                payload_first_record_hash,
                pending_snapshot_sequence: None,
            },
        );
        self.remove_generation_locked(state, source_generation);
        self.terminalize_wire_locked(
            state,
            wire_ordinal,
            Some(source_generation),
            WireLifecycleTransitionV1::ActorEnqueueFailed,
        );
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

    /// Returns O(1) readiness for every admitted authority route and registry terminal.
    pub fn cutoff_drain_status(&self) -> Result<bool, EdgeSourceFinalSealErrorV1> {
        self.reconcile_terminal_exclusions();
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisons.push(EdgeMeasurementPoisonV1::RecorderLockPoisoned);
                return Err(EdgeSourceFinalSealErrorV1::Poisoned);
            }
        };
        if !state.poisons.is_empty() {
            return Err(EdgeSourceFinalSealErrorV1::Poisoned);
        }
        Ok(state.wire_lifecycle.is_empty()
            && state.cache_pending.is_empty()
            && state.snapshot_products.is_empty()
            && state.payload_first_by_generation.is_empty()
            && state.snapshot_wire_ordinals.is_empty()
            && state.event_pending_ack == 0
            && self.registry.cutoff_drain_ready())
    }

    fn reconcile_terminal_exclusions(&self) {
        while let Some(exclusion) = self.registry.take_terminal_exclusion() {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let sequence = exclusion.metadata.identity.pending_snapshot_sequence;
            state.excluded_pending_snapshot_sequences.insert(sequence);
            state.snapshot_evidence_captured.remove(&sequence);
            state.snapshot_products.remove(&sequence);
            let wire_binding = state.snapshot_wire_ordinals.remove(&sequence);
            if let Some(source_generation) = exclusion.metadata.source_generation {
                state.payload_first_by_generation.remove(&source_generation);
                if state.source_generation_contexts.contains_key(&source_generation) {
                    self.remove_generation_locked(&mut state, source_generation);
                }
                if let Some((joined_generation, wire_ordinal)) = wire_binding
                    && joined_generation == source_generation
                    && state.wire_lifecycle.contains_key(&wire_ordinal)
                {
                    self.terminalize_wire_locked(
                        &mut state,
                        wire_ordinal,
                        Some(source_generation),
                        WireLifecycleTransitionV1::ProcessorTerminal,
                    );
                }
            }
            Self::record_missing_evidence(
                &mut state,
                match exclusion.reason {
                    PendingTerminalExclusionReasonV2::Capacity => {
                        EdgeMeasurementMissingEvidenceReasonV1::TerminalRecordCapacityExcluded
                    }
                    PendingTerminalExclusionReasonV2::Allocation => {
                        EdgeMeasurementMissingEvidenceReasonV1::TerminalRecordAllocationExcluded
                    }
                },
            );
        }
    }

    /// Returns whether every admitted authority route and registry terminal is durably drained.
    pub fn cutoff_drain_complete(&self) -> bool {
        matches!(self.cutoff_drain_status(), Ok(true))
    }
    /// Terminalizes every unresolved generation with its truthful owner at cutoff.
    ///
    /// This API is called by the coordinator after its bounded drain deadline.
    pub fn record_cutoff_drain_deadline(&self) -> Vec<(u64, bool)> {
        let unresolved = {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut unresolved: Vec<_> = state
                .source_generation_contexts
                .keys()
                .copied()
                .map(|generation| (generation, state.cache_pending.contains_key(&generation)))
                .collect();
            unresolved.sort_unstable_by_key(|(generation, _)| *generation);
            for (generation, _) in unresolved.iter().copied() {
                state.deadline_terminalized_generations.insert(generation);
            }
            if unresolved.iter().any(|(_, cache_owned)| *cache_owned) {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::CacheDrainIncomplete,
                );
            }
            for (_, cache_owned) in unresolved.iter().copied() {
                Self::record_missing_evidence(
                    &mut state,
                    if cache_owned {
                        EdgeMeasurementMissingEvidenceReasonV1::CutoffDeadlineCacheGeneration
                    } else {
                        EdgeMeasurementMissingEvidenceReasonV1::CutoffDeadlineProcessorGeneration
                    },
                );
            }
            let unresolved_generations: BTreeSet<_> =
                unresolved.iter().map(|(generation, _)| *generation).collect();
            for generations in state.decoded_source_generations.values_mut() {
                generations.retain(|generation| !unresolved_generations.contains(generation));
            }
            state.decoded_source_generations.retain(|_, generations| !generations.is_empty());
            unresolved
        };
        for (source_generation, cache_owned) in unresolved.iter().copied() {
            self.record_generation_product(ProcessorTerminalInputV1 {
                source_generation,
                base_disposition: if cache_owned {
                    ProcessorBaseDispositionV1::CachedUnresolvedAtCutoff
                } else {
                    ProcessorBaseDispositionV1::ProcessorUnresolvedAtCutoff
                },
                observer_disposition: ProcessorObserverDispositionV1::Absent,
                publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
                pending_snapshot_sequence: None,
                processor_error_reason: None,
                cache_resolved_final_disposition: None,
            });
        }
        unresolved
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
            self.catch_up_anchors_locked(&mut state, current_mono_ns);
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
        let Some(mut observation) = self.sample_locked(&mut state) else {
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
        if let Some(mono_ns) = observation.mono_ns {
            self.catch_up_anchors_locked(&mut state, mono_ns);
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
        let terminal = SourceCoverageTerminalV3::DecodeRejected;
        let terminal_hash = self.terminal_evidence_hash(admission.wire_ordinal, None, terminal);
        self.terminal_coverage_locked(
            &mut state,
            SourceTerminalEvidenceV3 {
                route: EpochRouteV1::Authority,
                source_generation: None,
                terminal,
                terminal_hash,
                payload_first_record_hash: None,
                pending_snapshot_sequence: None,
            },
        );
        state.authority_decode_rejected =
            state.authority_decode_rejected.checked_add(1).unwrap_or_else(|| {
                state.poisons.push(EdgeMeasurementPoisonV1::DecodeRejectedCountOverflow);
                u64::MAX
            });
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
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::ActiveStateCapacityOverflow,
            );
            self.exclude_active_generation_locked(&mut state, admission.wire_ordinal, generation);
            return None;
        }
        state.decoded_source_generations.entry(structural_key).or_default().push_back(generation);
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
        let key = PayloadFirstKeyV1 {
            producer_epoch: self.config.producer_epoch.get(),
            block_number: flashblock.metadata.block_number,
            payload_id: flashblock.payload_id.0.into(),
        };
        let next_refs =
            state.payload_generation_refs.get(&key).copied().unwrap_or_default().checked_add(1);
        let Some(next_refs) = next_refs else {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::ActiveStateCapacityOverflow,
            );
            self.exclude_active_generation_locked(&mut state, admission.wire_ordinal, generation);
            return None;
        };
        state.payload_generation_refs.insert(key, next_refs);
        if flashblock.index == 0 {
            if state.payload_without_index_zero.remove(&key) {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::PayloadIndexZeroLate,
                );
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
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::ActiveStateCapacityOverflow,
                );
                self.exclude_active_generation_locked(
                    &mut state,
                    admission.wire_ordinal,
                    generation,
                );
                return None;
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
            if !self.enqueue(&mut state, EdgeSourceEventV1::PayloadFirst(binding)) {
                self.exclude_active_generation_locked(
                    &mut state,
                    admission.wire_ordinal,
                    generation,
                );
                return None;
            }
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
            let terminal = SourceCoverageTerminalV3::CutoffRouted;
            let terminal_hash = self.terminal_evidence_hash(admission.wire_ordinal, None, terminal);
            self.terminal_coverage_locked(
                &mut state,
                SourceTerminalEvidenceV3 {
                    route: admission.route,
                    source_generation: None,
                    terminal,
                    terminal_hash,
                    payload_first_record_hash: None,
                    pending_snapshot_sequence: None,
                },
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
        if !succeeded {
            let retained_key = state.post_cutoff_routes.iter().find_map(|(key, routes)| {
                routes
                    .iter()
                    .position(|retained| retained.wire_ordinal == admission.wire_ordinal)
                    .map(|position| (*key, position))
            });
            if let Some((key, position)) = retained_key {
                if let Some(routes) = state.post_cutoff_routes.get_mut(&key) {
                    routes.remove(position);
                }
                if state.post_cutoff_routes.get(&key).is_some_and(VecDeque::is_empty) {
                    state.post_cutoff_routes.remove(&key);
                }
            }
            let terminal = SourceCoverageTerminalV3::CutoffRouted;
            let terminal_hash = self.terminal_evidence_hash(admission.wire_ordinal, None, terminal);
            self.terminal_coverage_locked(
                &mut state,
                SourceTerminalEvidenceV3 {
                    route: admission.route,
                    source_generation: None,
                    terminal,
                    terminal_hash,
                    payload_first_record_hash: None,
                    pending_snapshot_sequence: None,
                },
            );
        }
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
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
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
            let terminal = SourceCoverageTerminalV3::ActorEnqueueClosed;
            let terminal_hash =
                self.terminal_evidence_hash(wire_ordinal, Some(source_generation), terminal);
            let payload_hash = Self::payload_hash_locked(&state, source_generation);
            self.terminal_coverage_locked(
                &mut state,
                SourceTerminalEvidenceV3 {
                    route: EpochRouteV1::Authority,
                    source_generation: Some(source_generation),
                    terminal,
                    terminal_hash,
                    payload_first_record_hash: payload_hash,
                    pending_snapshot_sequence: None,
                },
            );
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
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
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
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
            return;
        };
        if state.wire_lifecycle.get(&wire_ordinal)
            != Some(&WireLifecyclePhaseV1::StateHandedOff(generation))
        {
            state.poisons.push(EdgeMeasurementPoisonV1::WireLifecycleConflict);
            return;
        }
        let terminal = SourceCoverageTerminalV3::StateQueueClosed;
        let terminal_hash = self.terminal_evidence_hash(wire_ordinal, Some(generation), terminal);
        let payload_hash = Self::payload_hash_locked(&state, generation);
        self.terminal_coverage_locked(
            &mut state,
            SourceTerminalEvidenceV3 {
                route: EpochRouteV1::Authority,
                source_generation: Some(generation),
                terminal,
                terminal_hash,
                payload_first_record_hash: payload_hash,
                pending_snapshot_sequence: None,
            },
        );
        self.remove_generation_locked(&mut state, generation);
        self.terminalize_wire_locked(
            &mut state,
            wire_ordinal,
            Some(generation),
            WireLifecycleTransitionV1::StateHandoffFailed,
        );
    }

    fn release_payload_generation_locked(
        state: &mut EdgeMeasurementRecorderStateV1,
        key: PayloadFirstKeyV1,
    ) {
        let remaining = match state.payload_generation_refs.get_mut(&key) {
            Some(refs) if *refs > 0 => {
                *refs -= 1;
                Some(*refs)
            }
            _ => {
                Self::record_missing_evidence(
                    state,
                    EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
                );
                None
            }
        };
        if remaining == Some(0) {
            state.payload_generation_refs.remove(&key);
            if state.payload_close_pending.remove(&key) {
                state.payload_first.remove(&key);
            }
        }
    }

    fn remove_generation_locked(
        &self,
        state: &mut EdgeMeasurementRecorderStateV1,
        source_generation: u64,
    ) {
        if let Some(context) = state.source_generation_contexts.remove(&source_generation) {
            if let Some(generations) =
                state.decoded_source_generations.get_mut(&context.structural_key)
            {
                generations.retain(|generation| *generation != source_generation);
                if generations.is_empty() {
                    state.decoded_source_generations.remove(&context.structural_key);
                }
            }
            Self::release_payload_generation_locked(state, context.payload_first_key);
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
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
        }
    }

    /// Closes payload bindings made obsolete by an observed canonical block.
    ///
    /// A binding is removed only after both this close evidence and the final generation terminal.
    pub(crate) fn close_payloads_through(&self, block_number: u64) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys: Vec<_> = state
            .payload_first
            .keys()
            .copied()
            .filter(|key| key.block_number <= block_number)
            .collect();
        for key in keys {
            if state.payload_generation_refs.get(&key).copied().unwrap_or_default() == 0 {
                state.payload_first.remove(&key);
            } else {
                state.payload_close_pending.insert(key);
            }
        }
        let missing: Vec<_> = state
            .payload_without_index_zero
            .iter()
            .copied()
            .filter(|key| key.block_number <= block_number)
            .collect();
        if !missing.is_empty() {
            for _ in &missing {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::PayloadIndexZeroMissing,
                );
            }
        }
        for key in missing {
            state.payload_without_index_zero.remove(&key);
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

    const fn processor_coverage_terminal(
        disposition: ProcessorBaseDispositionV1,
    ) -> SourceCoverageTerminalV3 {
        match disposition {
            ProcessorBaseDispositionV1::CacheResolvedToProcessor => {
                SourceCoverageTerminalV3::CacheResolved
            }
            ProcessorBaseDispositionV1::CacheReplacedOldGeneration => {
                SourceCoverageTerminalV3::CacheReplaced
            }
            ProcessorBaseDispositionV1::CacheEvicted => SourceCoverageTerminalV3::CacheEvicted,
            ProcessorBaseDispositionV1::CacheRejectedAhead
            | ProcessorBaseDispositionV1::MissingFirstUncacheable => {
                SourceCoverageTerminalV3::CacheRejected
            }
            ProcessorBaseDispositionV1::CachedUnresolvedAtCutoff => {
                SourceCoverageTerminalV3::CachedUnresolvedAtCutoff
            }
            ProcessorBaseDispositionV1::ProcessorUnresolvedAtCutoff => {
                SourceCoverageTerminalV3::ProcessorProduct
            }
            _ => SourceCoverageTerminalV3::ProcessorProduct,
        }
    }

    /// Records one terminal product for an admitted source generation.
    pub(crate) fn record_generation_product(&self, input: ProcessorTerminalInputV1) {
        self.reconcile_terminal_exclusions();
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
        let deadline_product = matches!(
            base_disposition,
            ProcessorBaseDispositionV1::CachedUnresolvedAtCutoff
                | ProcessorBaseDispositionV1::ProcessorUnresolvedAtCutoff
        );
        if state.deadline_terminalized_generations.contains(&source_generation) && !deadline_product
        {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::LateProcessorCompletion,
            );
            return;
        }
        if observer_disposition == ProcessorObserverDispositionV1::Panicked {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::ObserverPanicked,
            );
        }
        let wire_ordinal = state.generation_wire_ordinals.get(&source_generation).copied();
        let Some(context) = state.source_generation_contexts.remove(&source_generation) else {
            if state.deadline_terminalized_generations.contains(&source_generation)
                && !deadline_product
            {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::LateProcessorCompletion,
                );
            } else {
                state.poisons.push(EdgeMeasurementPoisonV1::DuplicateProcessorTerminal);
            }
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
            if state.excluded_pending_snapshot_sequences.contains(&sequence) {
                // The exact H2 exclusion receipt already counted and released this binding.
            } else if state.snapshot_products.len() >= self.config.active_state_capacity
                || state.payload_first_by_generation.len() >= self.config.active_state_capacity
                || state.snapshot_wire_ordinals.len() >= self.config.active_state_capacity
                || payload_first.is_none()
            {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::ActiveStateCapacityOverflow,
                );
            } else if let Some(payload_first) = payload_first {
                state.payload_first_by_generation.insert(source_generation, payload_first);
                state.snapshot_products.insert(sequence, product);
                if let Some(wire_ordinal) = wire_ordinal {
                    state
                        .snapshot_wire_ordinals
                        .insert(sequence, (source_generation, wire_ordinal));
                } else {
                    Self::record_missing_evidence(
                        &mut state,
                        EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
                    );
                }
            }
        }
        self.terminal_coverage_locked(
            &mut state,
            SourceTerminalEvidenceV3 {
                route: EpochRouteV1::Authority,
                source_generation: Some(source_generation),
                terminal: Self::processor_coverage_terminal(product.base_disposition),
                terminal_hash: product.structural_terminal_hash,
                payload_first_record_hash: product.payload_first_record_hash,
                pending_snapshot_sequence: product.pending_snapshot_sequence,
            },
        );
        Self::release_payload_generation_locked(&mut state, context.payload_first_key);
        self.enqueue(&mut state, EdgeSourceEventV1::Processor(product));
        if let Some(wire_ordinal) = wire_ordinal {
            self.terminalize_wire_locked(
                &mut state,
                wire_ordinal,
                Some(source_generation),
                WireLifecycleTransitionV1::ProcessorTerminal,
            );
        } else {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
        }
    }
    #[cfg(feature = "test-utils")]
    /// Emits one deterministic processor product through the production terminal path for fixtures.
    pub fn record_deterministic_test_product(
        &self,
        source_generation: u64,
        pending_snapshot_sequence: u64,
    ) {
        self.record_generation_product(ProcessorTerminalInputV1 {
            source_generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedInitialBase,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::Published(1),
            pending_snapshot_sequence: Some(pending_snapshot_sequence),
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
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
                    ping_due,
                    ping_written,
                    control_pong_seen: _,
                },
                Transition::ControlPongReceived,
            ) => Some(Phase::Established {
                read_half_closed,
                ping_due,
                ping_written,
                control_pong_seen: ping_due && ping_written,
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
                | Phase::AwaitingDirectReconnect
                | Phase::Established { .. },
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
        let previous_phase = state.connection_phase;
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
                state.connection_phase = previous_phase;
                return;
            }
        };
        let sample = self.sample_locked(state);
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
        state.previous_connection_record = state.last_connection_record;
        state.last_connection_record = Some(record);
        #[cfg(test)]
        state.connection_records.push_back(record);
        if self.enqueue(state, EdgeSourceEventV1::Connection(record)) {
            state.connection_recorded_phase = next_phase;
        }
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
        let sample = self.sample_locked(&mut state);
        if let Some(mono_ns) = sample.and_then(|observation| observation.mono_ns) {
            self.catch_up_anchors_locked(&mut state, mono_ns);
        }
        let cutoff_clock_observation_ordinal =
            state.next_clock_ordinal.checked_sub(1).unwrap_or_else(|| {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::CutoffBoundMissing,
                );
                0
            });
        let last_admitted_wire_ordinal =
            state.next_wire_ordinal.checked_sub(1).unwrap_or_else(|| {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::CutoffBoundMissing,
                );
                0
            });
        let last_admitted_source_generation =
            state.next_source_generation.checked_sub(1).unwrap_or_else(|| {
                if state.authority_decode_rejected != state.next_wire_ordinal {
                    Self::record_missing_evidence(
                        &mut state,
                        EdgeMeasurementMissingEvidenceReasonV1::CutoffBoundMissing,
                    );
                }
                0
            });
        let last_coverage_sequence =
            state.next_terminal_coverage_sequence.checked_sub(1).unwrap_or_else(|| {
                Self::record_missing_evidence(
                    &mut state,
                    EdgeMeasurementMissingEvidenceReasonV1::CutoffBoundMissing,
                );
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
            last_coverage_sequence,
            last_candidate_sequence: external.last_candidate_sequence,
            latch_mono_ns: sample.and_then(|value| value.mono_ns).unwrap_or(0),
            record_hash: B256::ZERO,
        };
        cutoff.record_hash = AuthorityRecordHasherV1::cutoff(&cutoff);
        state.cutoff = Some(cutoff);
        let payload_index_zero_missing = state.payload_without_index_zero.len();
        for _ in 0..payload_index_zero_missing {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::PayloadIndexZeroMissing,
            );
        }
        state.payload_without_index_zero.clear();
        if !state.cache_pending.is_empty() {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::CacheDrainIncomplete,
            );
        }
        let payload_keys: Vec<_> = state.payload_first.keys().copied().collect();
        for key in payload_keys {
            if state.payload_generation_refs.get(&key).copied().unwrap_or_default() == 0 {
                state.payload_first.remove(&key);
            } else {
                state.payload_close_pending.insert(key);
            }
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
    /// Returns whether consumer evidence was captured before durable terminal cleanup.
    pub fn terminal_durable_ready(&self, terminal: PendingTerminalRecordV2) -> bool {
        terminal.terminal != PendingCliTerminalV2::CliReceivedLookupSucceeded
            || self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot_evidence_captured
                .contains(&terminal.metadata.identity.pending_snapshot_sequence)
    }
    /// Returns the next terminal only when its dependent evidence is ready for persistence.
    ///
    /// At most one fixed-size record is copied per writer poll.
    pub fn next_terminal_durable_record(
        &self,
        coverage_sequence: u64,
    ) -> Option<PendingTerminalRecordV2> {
        self.reconcile_terminal_exclusions();
        let terminal = {
            let state = self.registry.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = usize::try_from(coverage_sequence).ok()?;
            state.terminal_records.get(index).copied()?
        };
        self.terminal_durable_ready(terminal).then_some(terminal)
    }

    /// Records one durably persisted H2 terminal, joins its exact wire, and releases evidence.
    pub fn record_terminal_durable(
        &self,
        terminal: PendingTerminalRecordV2,
        terminal_hash: B256,
    ) -> Result<(), EdgeMeasurementPoisonV1> {
        let sequence = terminal.metadata.identity.pending_snapshot_sequence;
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(source_generation) = terminal.metadata.source_generation else {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
            return Err(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        };
        let Some((joined_generation, wire_ordinal)) =
            state.snapshot_wire_ordinals.get(&sequence).copied()
        else {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
            return Err(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        };
        if joined_generation != source_generation
            || state
                .snapshot_products
                .get(&sequence)
                .is_none_or(|product| product.source_generation != source_generation)
            || !state.payload_first_by_generation.contains_key(&source_generation)
        {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
            return Err(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        }
        if terminal.terminal == PendingCliTerminalV2::CliReceivedLookupSucceeded
            && !state.snapshot_evidence_captured.remove(&sequence)
        {
            Self::record_missing_evidence(
                &mut state,
                EdgeMeasurementMissingEvidenceReasonV1::MissingSourceIdentity,
            );
            return Err(EdgeMeasurementPoisonV1::MissingSourceIdentity);
        }
        let coverage_terminal = match terminal.terminal {
            PendingCliTerminalV2::CliReceivedLookupSucceeded => {
                SourceCoverageTerminalV3::CliReceivedLookupSucceeded
            }
            PendingCliTerminalV2::CliRegistryLookupFailed(_) => {
                SourceCoverageTerminalV3::CliRegistryLookupFailed
            }
            PendingCliTerminalV2::CliLagged => SourceCoverageTerminalV3::CliLagged,
            PendingCliTerminalV2::CliClosed => SourceCoverageTerminalV3::CliClosed,
            PendingCliTerminalV2::CliCancelled => SourceCoverageTerminalV3::CliCancelled,
            PendingCliTerminalV2::NoReceivers
            | PendingCliTerminalV2::RegistrationFailedNoReceivers => {
                SourceCoverageTerminalV3::NoReceivers
            }
        };
        let payload_first_record_hash = state
            .payload_first_by_generation
            .get(&source_generation)
            .map(|record| record.record_hash);
        self.terminal_coverage_locked(
            &mut state,
            SourceTerminalEvidenceV3 {
                route: EpochRouteV1::Authority,
                source_generation: Some(source_generation),
                terminal: coverage_terminal,
                terminal_hash,
                payload_first_record_hash,
                pending_snapshot_sequence: Some(sequence),
            },
        );
        self.coverage_locked(
            &mut state,
            wire_ordinal,
            Some(source_generation),
            EpochRouteV1::Authority,
            WireLifecycleTransitionV1::CliTerminal {
                pending_snapshot_sequence: sequence,
                terminal: terminal.terminal,
            },
        );
        state.snapshot_wire_ordinals.remove(&sequence);
        state.snapshot_products.remove(&sequence);
        state.payload_first_by_generation.remove(&source_generation);
        Ok(())
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
        let Some(pending) = state.event_pending_ack.checked_sub(1) else {
            let poison = EdgeMeasurementPoisonV1::EventDurabilityAckUnderflow;
            state.poisons.push(poison);
            return Err(poison);
        };
        state.event_pending_ack = pending;
        Ok(())
    }

    /// Verifies independent zero-drop source and pending-registry finals.
    pub fn verify_source_final(&self) -> Result<ProducerEpochCutoffV1, EdgeSourceFinalSealErrorV1> {
        self.reconcile_terminal_exclusions();
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.poisons.push(EdgeMeasurementPoisonV1::RecorderLockPoisoned);
                return Err(EdgeSourceFinalSealErrorV1::Poisoned);
            }
        };
        let cutoff = state.cutoff.ok_or(EdgeSourceFinalSealErrorV1::CutoffMissing)?;
        if !state.poisons.is_empty() {
            return Err(EdgeSourceFinalSealErrorV1::Poisoned);
        }
        if state.event_pending_ack != 0 {
            return Err(EdgeSourceFinalSealErrorV1::EventPending);
        }
        if !state.decoded_source_generations.is_empty()
            || !state.payload_without_index_zero.is_empty()
            || !state.payload_first.is_empty()
            || !state.payload_generation_refs.is_empty()
            || !state.payload_close_pending.is_empty()
            || !state.source_generation_contexts.is_empty()
            || !state.generation_wire_ordinals.is_empty()
            || !state.wire_lifecycle.is_empty()
            || !state.cache_pending.is_empty()
            || !state.snapshot_products.is_empty()
            || !state.payload_first_by_generation.is_empty()
            || !state.snapshot_wire_ordinals.is_empty()
            || !state.snapshot_evidence_captured.is_empty()
        {
            return Err(EdgeSourceFinalSealErrorV1::ActiveStatePending);
        }
        let last_connection_valid = state.last_connection_record.map_or(
            state.connection_record_count == 0
                && state.next_connection_sequence == 0
                && state.last_connection_hash == B256::ZERO
                && state.connection_recorded_phase == ConnectionPhaseV1::New,
            |record| {
                state.connection_record_count == state.next_connection_sequence
                    && record.connection_sequence.checked_add(1)
                        == Some(state.next_connection_sequence)
                    && record.record_hash == state.last_connection_hash
                    && state.connection_recorded_phase != ConnectionPhaseV1::New
                    && record.record_hash == AuthorityRecordHasherV1::connection(&record)
            },
        );
        let connection_drop = state.missing_evidence.event_queue_full != 0
            || state.missing_evidence.event_queue_closed != 0;
        let h3_final = last_connection_valid
            && state.connection_phase == ConnectionPhaseV1::Exited
            && (state.connection_established_count == state.connection_closed_count
                || connection_drop);
        let expected_last_coverage_sequence =
            state.next_terminal_coverage_sequence.saturating_sub(1);
        let coverage_final = cutoff.last_coverage_sequence == expected_last_coverage_sequence;
        let source_final = state.authority_wire_terminals == state.next_wire_ordinal;
        if !h3_final || !coverage_final || !source_final {
            return Err(EdgeSourceFinalSealErrorV1::ConnectionFinalInvalid);
        }
        drop(state);
        self.registry.verify_final_seal().map_err(EdgeSourceFinalSealErrorV1::PendingRegistry)?;
        Ok(cutoff)
    }

    /// Returns finite source counters, missing evidence, and pending cardinalities for final artifacts.
    pub fn source_final_counters(&self) -> (u64, u64, u64, u64, u64, u64, u64, usize, u64) {
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
            state.missing_evidence.total,
        )
    }
    /// Returns exact named operational missing-evidence counters.
    pub fn missing_evidence_counts(&self) -> EdgeMeasurementMissingEvidenceV1 {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).missing_evidence.clone()
    }
    /// Returns producer poison and coordinator missing-evidence counts from live recorder state.
    pub fn producer_failure_counts(&self) -> (usize, u64) {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.poisons.len(), state.missing_evidence.coordinator_failure)
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
    /// Returns the cumulative checked count of nonfatal authority decode rejections.
    pub fn authority_decode_rejected_count(&self) -> u64 {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).authority_decode_rejected
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
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let payload_first = state.payload_first_by_generation.get(&source_generation).copied()?;
        let processor = state.snapshot_products.get(&sequence).copied()?;
        if processor.source_generation != source_generation {
            return None;
        }
        let connection = state.last_connection_record?;
        drop(state);
        let registry_terminal = self.registry.terminal_record(sequence)?;
        if registry_terminal.metadata != metadata {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .snapshot_products
            .get(&sequence)
            .is_none_or(|product| product.source_generation != source_generation)
            || !state.payload_first_by_generation.contains_key(&source_generation)
        {
            return None;
        }
        state.snapshot_evidence_captured.insert(sequence);
        Some((metadata, payload_first, processor, connection, registry_terminal))
    }
    /// Counts a named coordinator operational failure without changing production behavior.
    pub fn latch_coordinator_failure(&self, failure: &'static str) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::record_missing_evidence(
            &mut state,
            EdgeMeasurementMissingEvidenceReasonV1::CoordinatorFailure,
        );
        if state.missing_evidence.coordinator_reasons.record(failure).is_err() {
            state.poisons.push(EdgeMeasurementPoisonV1::MissingEvidenceCountOverflow(
                EdgeMeasurementMissingEvidenceReasonV1::CoordinatorFailure,
            ));
        }
    }

    /// Returns bounded exact coordinator missing-evidence reason-name counters.
    pub fn coordinator_missing_evidence_counts(&self) -> EdgeCoordinatorMissingEvidenceCountsV1 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .missing_evidence
            .coordinator_reasons
            .clone()
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
        let mut fields = vec![
            format!("\"anchorKind\":\"{anchor_kind}\""),
            format!("\"anchorSequence\":\"{}\"", record.anchor_sequence),
            format!("\"bootId\":\"{boot_id}\""),
            format!(
                "\"clockObservationOrdinal\":\"{}\"",
                record.observation.clock_observation_ordinal
            ),
            format!("\"clockSourceVersion\":\"{EDGE_CLOCK_SOURCE_VERSION_V1}\""),
            format!("\"disposition\":\"{disposition}\""),
            format!("\"dueMonoNs\":\"{}\"", record.due_mono_ns),
            format!("\"failureEvidence\":{failure}"),
            "\"kind\":\"Anchor\"".to_owned(),
        ];
        if let Some(mono_ns) = record.observation.mono_ns {
            fields.push(format!("\"monoNs\":\"{mono_ns}\""));
        }
        fields.extend([
            format!("\"monotonicResolutionNs\":\"{monotonic_resolution_ns}\""),
            format!("\"pairStatus\":\"{pair_status}\""),
            format!("\"persistenceSequence\":\"{}\"", record.anchor_sequence),
            format!("\"previousAnchorHash\":\"{}\"", Self::hex(record.previous_anchor_hash)),
            format!("\"producerEpoch\":\"{}\"", record.producer_epoch),
            format!("\"realtimeResolutionNs\":\"{realtime_resolution_ns}\""),
            format!("\"sampledMonoNs\":\"{}\"", record.sampled_mono_ns),
            "\"schema\":\"edge-clock-anchor/v1\"".to_owned(),
        ]);
        if let Some(utc_ns) = record.observation.utc_ns {
            fields.push(format!("\"utcNs\":\"{utc_ns}\""));
        }
        let json = format!("{{{}}}", fields.join(","));
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
            terminal_record_capacity: 64,
        })
        .expect("Linux recorder")
    }

    #[test]
    fn installation_rejects_terminal_capacity_outside_reviewed_bounds() {
        let base = EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(69).expect("nonzero epoch"),
            event_queue_capacity: 1,
            active_state_capacity: 1,
            pending_registry_capacity: 1,
            terminal_record_capacity: 0,
        };
        assert!(matches!(
            EdgeMeasurementRecorderV1::new(base),
            Err(EdgeMeasurementInstallErrorV1::ZeroCapacity)
        ));
        assert!(matches!(
            EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
                terminal_record_capacity: PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2 + 1,
                ..base
            }),
            Err(EdgeMeasurementInstallErrorV1::CapacityTooLarge)
        ));
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
    fn authority_decode_reject_is_recorded_and_source_final_remains_valid() {
        let recorder = test_recorder(70);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        let admission = recorder.observe_wire(b"malformed").expect("sampled authority wire");
        recorder.decode_rejected(admission);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });

        let mut decode_terminal_seen = false;
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            decode_terminal_seen |= matches!(
                event.as_ref(),
                EdgeSourceEventV1::TerminalCoverage(SourceTerminalCoverageV3 {
                    terminal: SourceCoverageTerminalV3::DecodeRejected,
                    ..
                })
            );
            recorder.ack_event_durable().expect("event durable");
        }

        assert!(decode_terminal_seen);
        assert_eq!(recorder.authority_decode_rejected_count(), 1);
        assert!(recorder.verify_source_final().is_ok());
    }

    #[test]
    fn mid_block_reconnect_gap_is_counted_and_source_final_remains_valid() {
        let mut flashblock = test_pending_blocks().get_flashblocks().remove(0);
        flashblock.index = 1;
        flashblock.base = None;
        let recorder = test_recorder(71);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        let admission = recorder.observe_wire(b"reconnect-index-one").expect("sampled wire");
        let generation =
            recorder.decoded_flashblock(admission, &flashblock).expect("decoded generation");
        recorder.actor_enqueue(generation, true);
        recorder.actor_delivered(generation);
        assert_eq!(
            recorder.begin_state_handoff(DecodedFlashblockKeyV1::from_flashblock(&flashblock)),
            Some(generation)
        );
        recorder.take_source_generation(&flashblock).expect("processor admission");
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedInitialBase,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        recorder.close_payloads_through(flashblock.metadata.block_number);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("event durable");
        }

        let state = recorder.state.lock().expect("state");
        assert_eq!(state.missing_evidence.payload_index_zero_missing, 1);
        assert!(state.poisons.is_empty(), "{:?}", state.poisons);
        drop(state);
        assert!(recorder.verify_source_final().is_ok());
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
    fn both_failed_anchor_hash_omits_failed_clock_keys() {
        let record = ClockAnchorRecordV1 {
            producer_epoch: 9,
            anchor_sequence: 0,
            observation: WireObservationV1 {
                clock_observation_ordinal: 1,
                utc_status: ClockStatusV1::Failed(ClockFailureV1 { status: -1, errno: 5 }),
                utc_ns: None,
                mono_status: ClockStatusV1::Failed(ClockFailureV1 { status: -1, errno: 6 }),
                mono_ns: None,
                wire_digest: B256::ZERO,
            },
            startup: true,
            due_mono_ns: 2,
            sampled_mono_ns: 2,
            previous_anchor_hash: B256::ZERO,
            record_hash: B256::ZERO,
        };
        let expected_json = concat!(
            "{\"anchorKind\":\"Startup\",\"anchorSequence\":\"0\",",
            "\"bootId\":\"000000000000000000000000000000000000\",",
            "\"clockObservationOrdinal\":\"1\",\"clockSourceVersion\":\"linux-clock-gettime-realtime-monotonic/v1\",",
            "\"disposition\":\"Failed\",\"dueMonoNs\":\"2\",",
            "\"failureEvidence\":\"BothSyscallsFailed\",\"kind\":\"Anchor\",",
            "\"monotonicResolutionNs\":\"4\",\"pairStatus\":\"BothFailed\",",
            "\"persistenceSequence\":\"0\",",
            "\"previousAnchorHash\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
            "\"producerEpoch\":\"9\",\"realtimeResolutionNs\":\"3\",",
            "\"sampledMonoNs\":\"2\",\"schema\":\"edge-clock-anchor/v1\"}"
        );
        assert_eq!(
            AuthorityRecordHasherV1::clock_anchor(&record, [b'0'; 36], 3, 4),
            AuthorityRecordHasherV1::authority("edge-clock-anchor/v1", expected_json)
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
        assert_eq!(registry.snapshot().unregistered_send_count, 2);
    }

    #[test]
    fn non_authority_send_count_is_exact_beyond_diagnostic_ring() {
        let registry = PendingMetadataRegistryV2::new(11, 4);
        for _ in 0..100 {
            registry.begin_unregistered_send();
            registry
                .record_unregistered_send(PendingSendJournalMarkerV2::PassthroughNonAdvanced, None)
                .expect("non-authority disposition");
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.unregistered_send_count, 100);
        assert_eq!(snapshot.unregistered_send_records.len(), PENDING_DIAGNOSTIC_RING_CAPACITY_V2);
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
    fn cutoff_readiness_ignores_history_but_final_seal_conserves_it() {
        let registry = PendingMetadataRegistryV2::new(2, 1);
        {
            let (mut state, _) = registry.lock_state();
            assert!(state.sets.advanced_with_snapshot.insert(7));
        }

        assert!(registry.cutoff_drain_ready());
        assert_eq!(registry.verify_final_seal(), Err(PendingFinalSealErrorV2::SequenceSetMismatch));
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
    fn capacity_overflow_fails_without_retaining_registry_identity() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(4, 1);
        let occupying = registry.register(&pending, None);
        registry.record_send(occupying, None).expect("occupying terminal");
        assert_eq!(Arc::weak_count(&pending), 1);

        let failed = registry.register(&pending, Some(8));
        assert_eq!(
            failed.disposition,
            PendingRegistrationDispositionV2::Failed(
                PendingRegistrationFailure::PendingRegistryCapacityOverflow
            )
        );
        assert_eq!(Arc::weak_count(&pending), 1);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.primary_pending, 1);
        assert_eq!(snapshot.secondary_pending, 1);
        assert!(!snapshot.poisoned);
        assert!(registry.lock_state().0.poisons.is_empty());

        registry.ack_terminal_durable(0).expect("release successful entry");
        registry.record_send(failed, Some(1)).expect("failed send journaled");
        let failure = registry.cli_received(&pending).expect_err("lookup cannot succeed");
        assert_eq!(
            failure.reason,
            CliRegistryLookupFailureReason::RegistrationFailed(
                PendingRegistrationFailure::PendingRegistryCapacityOverflow
            )
        );
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.primary_pending, 0);
        assert_eq!(snapshot.secondary_pending, 0);
        assert_eq!(snapshot.terminal_records, 1);
        assert_eq!(Arc::weak_count(&pending), 0);
    }

    #[test]
    fn zero_capacity_overflow_retains_no_primary_secondary_or_weak() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(5, 0);
        let failed = registry.register(&pending, None);
        registry.record_send(failed, None).expect("failed send recorded");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.primary_pending, 0);
        assert_eq!(snapshot.secondary_pending, 0);
        assert_eq!(snapshot.terminal_records, 0);
        assert_eq!(Arc::weak_count(&pending), 0);
    }

    #[test]
    fn lag_ignores_unretained_capacity_failure_journal_entry() {
        let first = test_pending_blocks();
        let second = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(6, 1);
        let succeeded = registry.register(&first, None);
        let failed = registry.register(&second, None);
        registry.record_send(succeeded, Some(1)).expect("success published");
        registry.record_send(failed, Some(1)).expect("failure published");
        registry.cli_lagged(2).expect("exact lag range");
        registry.ack_terminal_durable(0).expect("successful terminal");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.sets.cli_lagged_attributed, BTreeSet::from([0]));
        assert_eq!(snapshot.primary_pending, 0);
        assert_eq!(snapshot.secondary_pending, 0);
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
        assert_eq!(registry.terminal_records_from(0)[0].coverage_sequence, 0);
        registry.ack_terminal_durable(0).expect("coverage durable");
    }
    #[test]
    fn cumulative_terminal_capacity_is_checked_before_second_allocation_without_poison() {
        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new_bounded(83, 1, 1);

        let first = registry.register(&pending, None);
        registry.record_send(first, None).expect("record at boundary");
        registry.ack_terminal_durable(0).expect("first durable");

        let second = registry.register(&pending, None);
        registry.record_send(second, None).expect("capacity exclusion is terminal");
        let state = registry.lock_state().0;
        assert_eq!(state.terminal_records.len(), 1);
        assert_eq!(state.terminal_record_index.len(), 1);
        assert_eq!(state.durability_acked, 1);
        assert_eq!(state.terminal_record_capacity_missing, 1);
        assert!(state.primary.is_empty());
        assert!(state.secondary.is_empty());
        assert!(state.poisons.is_empty());
        drop(state);
        assert_eq!(registry.verify_final_seal(), Ok(()));
    }
    #[test]
    fn exact_sequence_bitmap_compresses_72_hour_campaign_by_sixty_four() {
        const RECORDS_72H_AT_FIVE_HZ: u64 = 72 * 60 * 60 * 5;
        let mut bitmap = PendingSequenceBitmapV2::default();
        for sequence in 0..RECORDS_72H_AT_FIVE_HZ {
            assert!(bitmap.insert(sequence));
        }
        assert_eq!(
            bitmap.len(),
            usize::try_from(RECORDS_72H_AT_FIVE_HZ).expect("72h fixture fits usize")
        );
        assert_eq!(
            bitmap.words.len(),
            usize::try_from(RECORDS_72H_AT_FIVE_HZ.div_ceil(64)).expect("word count fits usize")
        );
        assert!(bitmap.contains(&(RECORDS_72H_AT_FIVE_HZ - 1)));
    }
    #[test]
    fn terminal_suffix_clone_bytes_depend_only_on_unread_records() {
        const HISTORY_RECORDS: u64 = 2_048;
        const UNREAD_RECORDS: u64 = 7;

        let pending = test_pending_blocks();
        let registry = PendingMetadataRegistryV2::new(82, 1);
        for coverage_sequence in 0..HISTORY_RECORDS {
            let attempt = registry.register(&pending, None);
            registry.record_send(attempt, None).expect("terminal accepted");
            registry.ack_terminal_durable(coverage_sequence).expect("terminal durable");
        }

        let start = HISTORY_RECORDS - UNREAD_RECORDS;
        let unread = registry.terminal_records_from(start);
        let returned_clone_bytes = unread.len() * std::mem::size_of::<PendingTerminalRecordV2>();
        assert_eq!(unread.len(), usize::try_from(UNREAD_RECORDS).expect("small fixture"));
        assert_eq!(
            returned_clone_bytes,
            usize::try_from(UNREAD_RECORDS).expect("small fixture")
                * std::mem::size_of::<PendingTerminalRecordV2>()
        );
        assert!(
            returned_clone_bytes
                < usize::try_from(HISTORY_RECORDS).expect("small fixture")
                    * std::mem::size_of::<PendingTerminalRecordV2>()
        );
        assert_eq!(unread.first().map(|record| record.coverage_sequence), Some(start));
        assert_eq!(
            registry.terminal_record(HISTORY_RECORDS - 1).map(|record| record.coverage_sequence),
            Some(HISTORY_RECORDS - 1)
        );
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
    fn empty_coverage_cutoff_uses_zero_and_latches_named_missing_bound() {
        let recorder = test_recorder(320);
        let cutoff = recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        assert_eq!(cutoff.last_coverage_sequence, 0);
        let state = recorder.state.lock().expect("state");
        assert_eq!(state.missing_evidence.cutoff_bound_missing, 2);
        assert!(state.poisons.is_empty());
        drop(state);
        let (poison_count, coordinator_count) = recorder.producer_failure_counts();
        assert_eq!(poison_count, 0);
        assert_eq!(coordinator_count, 0);
        recorder.latch_coordinator_failure("test-coordinator");
        assert_eq!(recorder.producer_failure_counts(), (poison_count, 1));
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
            terminal_record_capacity: 4,
        })
        .expect("Linux recorder");
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        let state = recorder.state.lock().expect("state");
        assert_eq!(state.missing_evidence.event_queue_full, 2);
        assert_eq!(state.next_connection_sequence, 0);
    }
    #[test]
    fn repeated_queue_full_samples_are_bounded_without_poisoning_final() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(12).expect("nonzero"),
            event_queue_capacity: 1,
            active_state_capacity: 1,
            pending_registry_capacity: 1,
            terminal_record_capacity: 1,
        })
        .expect("Linux recorder");
        let cutoff = ProducerEpochCutoffV1 {
            producer_epoch: 12,
            cutoff_clock_observation_ordinal: 0,
            last_admitted_wire_ordinal: 0,
            last_admitted_source_generation: 0,
            last_admitted_blink_generation: 0,
            last_pending_snapshot_sequence: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
            latch_mono_ns: 0,
            record_hash: B256::ZERO,
        };
        let mut state = recorder.state.lock().expect("state");
        recorder.enqueue(&mut state, EdgeSourceEventV1::Cutoff(cutoff));
        for _ in 0..(EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1 * 3) {
            recorder.enqueue(&mut state, EdgeSourceEventV1::Cutoff(cutoff));
        }
        state.cutoff = Some(cutoff);
        assert_eq!(state.poisons.len(), 0);
        assert_eq!(
            state.missing_evidence.total,
            u64::try_from(EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1 * 3 + 1).expect("count fits")
        );
        assert_eq!(state.missing_evidence.sample_len(), EDGE_MEASUREMENT_POISON_SAMPLE_CAPACITY_V1);
        drop(state);

        assert_eq!(recorder.verify_source_final(), Err(EdgeSourceFinalSealErrorV1::EventPending));
    }
    #[test]
    fn recorder_mutex_poison_is_sticky_and_rejects_readiness_and_final() {
        let recorder = test_recorder(13);
        let poisoner = Arc::clone(&recorder);
        let _ = thread::spawn(move || {
            let _state = poisoner.state.lock().expect("state");
            panic!("poison recorder mutex");
        })
        .join();

        assert_eq!(recorder.cutoff_drain_status(), Err(EdgeSourceFinalSealErrorV1::Poisoned));
        assert_eq!(recorder.verify_source_final(), Err(EdgeSourceFinalSealErrorV1::Poisoned));
        let state = recorder.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(state.poisons.contains(&EdgeMeasurementPoisonV1::RecorderLockPoisoned));
    }

    #[test]
    fn uninstalled_global_is_explicit_noop_handle() {
        if EdgeMeasurementGlobal::installed().is_none() {
            assert!(EdgeMeasurementGlobal::recorder().observe_wire(b"frame").is_none());
            assert_eq!(EdgeMeasurementGlobal::registry_handle().cli_closed(), Ok(()));
        }
    }
    #[test]
    fn cache_cutoff_deadline_emits_real_terminal_and_fails_closed() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(21);
        let admission = recorder.observe_wire(b"cached").expect("wire observation");
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), Some(0));
        recorder.actor_enqueue(0, true);
        recorder.actor_delivered(0);
        assert_eq!(
            recorder.begin_state_handoff(DecodedFlashblockKeyV1::from_flashblock(&flashblock)),
            Some(0)
        );
        assert_eq!(recorder.take_source_generation(&flashblock), Some(0));
        recorder.observe_cache_wait(0, ProcessorBaseDispositionV1::CachedAwaitCanonical);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        assert_eq!(recorder.record_cutoff_drain_deadline(), vec![(0, true)]);

        let mut products = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event {
                products.push(product);
            }
        }
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].source_generation, 0);
        assert_eq!(
            products[0].base_disposition,
            ProcessorBaseDispositionV1::CachedUnresolvedAtCutoff
        );
        let state = recorder.state.lock().expect("state");
        assert!(state.cache_pending.is_empty());
        assert!(state.source_generation_contexts.is_empty());
        assert_eq!(state.processor_terminal_count, 1);
        assert_eq!(state.missing_evidence.cache_drain_incomplete, 2);
        assert_eq!(state.missing_evidence.cutoff_deadline_cache_generation, 1);
        assert!(state.poisons.is_empty());
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
            state.next_terminal_coverage_sequence.saturating_sub(1)
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
    fn h3_rejects_duplicate_half_close_and_ping_without_due() {
        for invalid in [
            SourceConnectionTransitionV1::ReadHalfClosedWaitingForControl,
            SourceConnectionTransitionV1::OutgoingPingWritten,
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
    fn h3_accepts_unsolicited_control_pong_but_keeps_pong_equation_strict() {
        for ping_due in [false, true] {
            let recorder = test_recorder(211 + u64::from(ping_due));
            recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
            recorder
                .connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
            recorder.connection_transition(SourceConnectionTransitionV1::Established);
            if ping_due {
                recorder.connection_transition(SourceConnectionTransitionV1::OutgoingPingDue);
            }
            recorder.connection_transition(SourceConnectionTransitionV1::ControlPongReceived);
            assert!(
                !recorder
                    .state
                    .lock()
                    .expect("state")
                    .poisons
                    .contains(&EdgeMeasurementPoisonV1::ConnectionTransitionConflict)
            );
            recorder.connection_transition(SourceConnectionTransitionV1::PongObserved);
            assert!(
                recorder
                    .state
                    .lock()
                    .expect("state")
                    .poisons
                    .contains(&EdgeMeasurementPoisonV1::ConnectionTransitionConflict)
            );
        }

        let recorder = test_recorder(213);
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        recorder.connection_transition(SourceConnectionTransitionV1::ControlPongReceived);
        recorder.connection_transition(SourceConnectionTransitionV1::OutgoingPingDue);
        recorder.connection_transition(SourceConnectionTransitionV1::OutgoingPingWritten);
        recorder.connection_transition(SourceConnectionTransitionV1::ControlPongReceived);
        recorder.connection_transition(SourceConnectionTransitionV1::PongObserved);
        assert!(
            !recorder
                .state
                .lock()
                .expect("state")
                .poisons
                .contains(&EdgeMeasurementPoisonV1::ConnectionTransitionConflict)
        );
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
    #[test]
    fn failed_post_cutoff_actor_enqueue_releases_retained_route() {
        let flashblock = test_pending_blocks().get_flashblocks().remove(0);
        let recorder = test_recorder(214);
        recorder.prepare_cutoff();
        let admission = recorder.observe_wire(b"excluded-failure").expect("excluded admission");
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), None);
        recorder.post_cutoff_actor_enqueue(admission, false);
        let state = recorder.state.lock().expect("state");
        assert!(state.post_cutoff_routes.is_empty());
        drop(state);

        let mut excluded_terminal = false;
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            excluded_terminal |= matches!(
                *event,
                EdgeSourceEventV1::TerminalCoverage(SourceTerminalCoverageV3 {
                    route: EpochRouteV1::PostCutoffNonAuthority,
                    terminal: SourceCoverageTerminalV3::CutoffRouted,
                    ..
                })
            );
        }
        assert!(excluded_terminal);
    }
    #[test]
    fn index_zero_terminal_preserves_binding_for_later_payload_index() {
        let pending = test_pending_blocks();
        let index_zero = pending.get_flashblocks().remove(0);
        let mut index_one = index_zero.clone();
        index_one.index = 1;
        index_one.base = None;
        let recorder = test_recorder(204);

        let first_admission = recorder.observe_wire(b"index-zero").expect("first wire");
        let first_generation =
            recorder.decoded_flashblock(first_admission, &index_zero).expect("first generation");
        recorder.take_source_generation(&index_zero);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: first_generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedInitialBase,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        let payload_hash = recorder
            .state
            .lock()
            .expect("state")
            .payload_first
            .values()
            .next()
            .expect("binding survives first terminal")
            .record_hash;

        let second_admission = recorder.observe_wire(b"index-one").expect("second wire");
        let second_generation =
            recorder.decoded_flashblock(second_admission, &index_one).expect("second generation");
        recorder.take_source_generation(&index_one);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: second_generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedNextInSequence,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });

        let mut second_product = None;
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event
                && product.source_generation == second_generation
            {
                second_product = Some(product);
            }
        }
        assert_eq!(
            second_product.expect("index-one product").payload_first_record_hash,
            Some(payload_hash)
        );
        recorder.close_payloads_through(index_zero.metadata.block_number);
        let state = recorder.state.lock().expect("state");
        assert!(state.payload_first.is_empty());
        assert!(state.payload_generation_refs.is_empty());
        assert!(state.payload_close_pending.is_empty());
    }
    #[test]
    fn every_durable_h2_terminal_cleans_evidence_and_preserves_wire_join() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(206);
        let admission = recorder.observe_wire(b"wire-join").expect("wire");
        let generation = recorder.decoded_flashblock(admission, &flashblock).expect("generation");
        recorder.take_source_generation(&flashblock);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedInitialBase,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::Published(1),
            pending_snapshot_sequence: Some(0),
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        let (payload, mut product) = {
            let state = recorder.state.lock().expect("state");
            (state.payload_first_by_generation[&generation], state.snapshot_products[&0])
        };
        let terminals = [
            PendingCliTerminalV2::CliLagged,
            PendingCliTerminalV2::CliClosed,
            PendingCliTerminalV2::CliCancelled,
            PendingCliTerminalV2::NoReceivers,
            PendingCliTerminalV2::RegistrationFailedNoReceivers,
        ];
        for (sequence, terminal) in terminals.into_iter().enumerate() {
            let sequence = u64::try_from(sequence).expect("sequence");
            if sequence != 0 {
                product.pending_snapshot_sequence = Some(sequence);
                let mut state = recorder.state.lock().expect("state");
                state.payload_first_by_generation.insert(generation, payload);
                state.snapshot_products.insert(sequence, product);
                state.snapshot_wire_ordinals.insert(sequence, (generation, admission.wire_ordinal));
            }
            recorder
                .record_terminal_durable(
                    PendingTerminalRecordV2 {
                        coverage_sequence: sequence,
                        metadata: PendingSnapshotMetadataV2 {
                            identity: PendingSnapshotIdentityV2 {
                                producer_epoch: 206,
                                pending_snapshot_sequence: sequence,
                                arc_pointer_identity: 1,
                            },
                            source_generation: Some(generation),
                            pending_public_subset_digest_v1: B256::ZERO,
                        },
                        registration: PendingRegistrationDispositionV2::Succeeded,
                        send: PendingSendDispositionV2::NoReceivers,
                        terminal,
                    },
                    B256::with_last_byte(sequence as u8 + 1),
                )
                .expect("durable cleanup");
        }

        let state = recorder.state.lock().expect("state");
        assert!(state.snapshot_products.is_empty());
        assert!(state.payload_first_by_generation.is_empty());
        assert!(state.snapshot_wire_ordinals.is_empty());
        drop(state);
        let mut processor_wire = None;
        let mut cli_wires = Vec::new();
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Coverage(record) = *event {
                match record.transition {
                    WireLifecycleTransitionV1::ProcessorTerminal => {
                        processor_wire = Some(record.wire_ordinal);
                    }
                    WireLifecycleTransitionV1::CliTerminal { .. } => {
                        cli_wires.push(record.wire_ordinal);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(processor_wire, Some(admission.wire_ordinal));
        assert_eq!(cli_wires, vec![admission.wire_ordinal; terminals.len()]);
    }
    #[test]
    fn post_cutoff_exclusion_remains_durable_after_authority_final() {
        let recorder = test_recorder(205);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        let admission = recorder.observe_wire(b"after-authority-final").expect("excluded wire");
        assert_eq!(admission.route, EpochRouteV1::PostCutoffNonAuthority);

        let mut cutoff_seen = false;
        let mut post_cutoff_after_final = false;
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            match *event {
                EdgeSourceEventV1::Cutoff(_) => cutoff_seen = true,
                EdgeSourceEventV1::Coverage(record)
                    if cutoff_seen && record.route == EpochRouteV1::PostCutoffNonAuthority =>
                {
                    post_cutoff_after_final = true;
                }
                _ => {}
            }
            recorder.ack_event_durable().expect("durable event");
        }
        assert!(cutoff_seen);
        assert!(post_cutoff_after_final);
        assert_eq!(recorder.source_final_counters().6, 0);
    }
    #[test]
    fn processor_cutoff_deadline_terminalizes_truthful_owner_and_reaches_final() {
        let pending = test_pending_blocks();
        let flashblock = pending.get_flashblocks().remove(0);
        let recorder = test_recorder(206);
        let admission = recorder.observe_wire(b"processor-owned").expect("wire");
        let generation = recorder.decoded_flashblock(admission, &flashblock).expect("generation");
        recorder.actor_enqueue(generation, true);
        recorder.actor_delivered(generation);
        assert_eq!(
            recorder.begin_state_handoff(DecodedFlashblockKeyV1::from_flashblock(&flashblock)),
            Some(generation)
        );
        assert_eq!(recorder.take_source_generation(&flashblock), Some(generation));
        recorder.prepare_cutoff();

        assert_eq!(recorder.record_cutoff_drain_deadline(), vec![(generation, false)]);
        recorder.record_generation_product(ProcessorTerminalInputV1 {
            source_generation: generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedNextInSequence,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::NotApplicable,
            pending_snapshot_sequence: None,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        });
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        let mut processor_terminal = None;
        while let EdgeEventDrainStatusV1::Event(event) = recorder.try_recv_event() {
            if let EdgeSourceEventV1::Processor(product) = *event {
                processor_terminal = Some(product.base_disposition);
            }
            recorder.ack_event_durable().expect("durable event");
        }

        assert_eq!(
            processor_terminal,
            Some(ProcessorBaseDispositionV1::ProcessorUnresolvedAtCutoff)
        );
        let evidence = recorder.missing_evidence_counts();
        assert_eq!(evidence.cutoff_deadline_processor_generation, 1);
        assert_eq!(evidence.cutoff_deadline_cache_generation, 0);
        assert_eq!(evidence.late_processor_completion, 1);
        assert!(
            !recorder
                .state
                .lock()
                .expect("state")
                .poisons
                .contains(&EdgeMeasurementPoisonV1::DuplicateProcessorTerminal)
        );
        assert!(recorder.verify_source_final().is_ok());
    }

    #[test]
    fn queue_full_excludes_authority_cursor_gap_and_source_final_remains_finite() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(207).expect("nonzero"),
            event_queue_capacity: 1,
            active_state_capacity: 4,
            pending_registry_capacity: 4,
            terminal_record_capacity: 4,
        })
        .expect("Linux recorder");
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        assert_eq!(recorder.source_final_counters().3, 0);
        let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() else {
            panic!("startup anchor");
        };
        recorder.ack_event_durable().expect("startup durable");

        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("durable event");
        }

        let evidence = recorder.missing_evidence_counts();
        assert!(evidence.event_queue_full > 0);
        {
            let state = recorder.state.lock().expect("state");
            assert!(state.poisons.is_empty(), "{:?}", state.poisons);
        }
        let final_result = recorder.verify_source_final();
        assert!(final_result.is_ok(), "{final_result:?}");
    }

    #[test]
    fn queue_closed_excludes_authority_cursor_gap_and_source_final_remains_finite() {
        let recorder = test_recorder(210);
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("startup durable");
        }
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("connection durable");
        }
        let (_replacement_sender, replacement_receiver) = sync_channel(1);
        let old_receiver = {
            let mut receiver = recorder.event_receiver.lock().expect("receiver");
            std::mem::replace(&mut *receiver, replacement_receiver)
        };
        drop(old_receiver);

        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });

        let evidence = recorder.missing_evidence_counts();
        assert!(evidence.event_queue_closed > 0);
        {
            let state = recorder.state.lock().expect("state");
            assert!(state.poisons.is_empty(), "{:?}", state.poisons);
        }
        let final_result = recorder.verify_source_final();
        assert!(final_result.is_ok(), "{final_result:?}");
    }
    #[test]
    fn payload_first_queue_drop_releases_generation_and_reaches_source_final() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(215).expect("nonzero"),
            event_queue_capacity: 1,
            active_state_capacity: 4,
            pending_registry_capacity: 4,
            terminal_record_capacity: 4,
        })
        .expect("Linux recorder");
        let flashblock = test_pending_blocks().get_flashblocks().remove(0);
        let admission = recorder.observe_wire(b"payload-first-drop").expect("wire");
        assert_eq!(recorder.decoded_flashblock(admission, &flashblock), None);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("durable retained event");
        }

        let state = recorder.state.lock().expect("state");
        assert!(state.source_generation_contexts.is_empty());
        assert!(state.generation_wire_ordinals.is_empty());
        assert!(state.wire_lifecycle.is_empty());
        assert!(state.payload_first.is_empty());
        assert!(state.payload_generation_refs.is_empty());
        drop(state);
        assert!(recorder.verify_source_final().is_ok());
    }

    #[test]
    fn connection_queue_drop_rolls_back_h3_chain_without_blocking_final() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(216).expect("nonzero"),
            event_queue_capacity: 1,
            active_state_capacity: 4,
            pending_registry_capacity: 4,
            terminal_record_capacity: 4,
        })
        .expect("Linux recorder");
        recorder.connection_transition(SourceConnectionTransitionV1::OwnerStart);
        recorder.connection_transition(SourceConnectionTransitionV1::InitialConnectAttemptStarted);
        recorder.connection_transition(SourceConnectionTransitionV1::Established);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("durable retained event");
        }

        let state = recorder.state.lock().expect("state");
        assert_eq!(state.connection_phase, ConnectionPhaseV1::Exited);
        assert_eq!(state.connection_recorded_phase, ConnectionPhaseV1::New);
        assert_eq!(state.next_connection_sequence, 0);
        assert_eq!(state.connection_record_count, 0);
        assert_eq!(state.last_connection_hash, B256::ZERO);
        assert!(state.poisons.is_empty(), "{:?}", state.poisons);
        drop(state);
        assert!(recorder.verify_source_final().is_ok());
    }

    #[test]
    fn active_capacity_overflow_terminalizes_the_rejected_wire() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(217).expect("nonzero"),
            event_queue_capacity: 64,
            active_state_capacity: 1,
            pending_registry_capacity: 4,
            terminal_record_capacity: 4,
        })
        .expect("Linux recorder");
        let first = test_pending_blocks().get_flashblocks().remove(0);
        let mut second = first.clone();
        second.index = 1;
        let first_admission = recorder.observe_wire(b"capacity-first").expect("first wire");
        let first_generation =
            recorder.decoded_flashblock(first_admission, &first).expect("first generation");
        let second_admission = recorder.observe_wire(b"capacity-second").expect("second wire");
        assert_eq!(recorder.decoded_flashblock(second_admission, &second), None);
        recorder.actor_enqueue(first_generation, false);
        recorder.close_payloads_through(first.metadata.block_number);
        recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("durable event");
        }

        assert_eq!(recorder.source_final_counters().5, 2);
        assert_eq!(recorder.missing_evidence_counts().active_state_capacity_overflow, 1);
        assert!(recorder.verify_source_final().is_ok());
    }
    #[test]
    fn terminal_exclusion_receipts_release_all_recorder_snapshot_bindings() {
        let recorder = test_recorder(218);
        let pending = test_pending_blocks();
        let first = recorder.registry.register(&pending, Some(7));
        let second = recorder.registry.register(&pending, Some(8));
        let first_sequence = first.pending_snapshot_sequence.expect("first sequence");
        let second_sequence = second.pending_snapshot_sequence.expect("second sequence");
        let payload = PayloadFirstObservationV1 {
            key: PayloadFirstKeyV1 { producer_epoch: 218, block_number: 1, payload_id: [0; 8] },
            source_generation: 7,
            observation: WireObservationV1 {
                clock_observation_ordinal: 0,
                utc_status: ClockStatusV1::Ok,
                utc_ns: Some(1),
                mono_status: ClockStatusV1::Ok,
                mono_ns: Some(1),
                wire_digest: B256::ZERO,
            },
            boot_id: recorder.boot_id,
            realtime_resolution_ns: recorder.realtime_resolution_ns,
            monotonic_resolution_ns: recorder.monotonic_resolution_ns,
            record_sequence: 0,
            previous_record_hash: B256::ZERO,
            record_hash: B256::ZERO,
        };
        let product = |source_generation, pending_snapshot_sequence| ProcessorLifecycleProductV1 {
            producer_epoch: 218,
            source_generation,
            base_disposition: ProcessorBaseDispositionV1::AdvancedInitialBase,
            observer_disposition: ProcessorObserverDispositionV1::Absent,
            publish_disposition: ProcessorPublishDispositionV1::Published(1),
            pending_snapshot_sequence: Some(pending_snapshot_sequence),
            payload_first_record_hash: Some(B256::ZERO),
            structural_terminal_hash: B256::ZERO,
            processor_error_reason: None,
            cache_resolved_final_disposition: None,
        };
        {
            let mut state = recorder.state.lock().expect("state");
            state.snapshot_products.insert(first_sequence, product(7, first_sequence));
            state.snapshot_products.insert(second_sequence, product(8, second_sequence));
            state.payload_first_by_generation.insert(7, payload);
            state
                .payload_first_by_generation
                .insert(8, PayloadFirstObservationV1 { source_generation: 8, ..payload });
            state.snapshot_wire_ordinals.insert(first_sequence, (7, 70));
            state.snapshot_wire_ordinals.insert(second_sequence, (8, 80));
            state.snapshot_evidence_captured.insert(first_sequence);
            state.snapshot_evidence_captured.insert(second_sequence);
        }
        {
            let mut state = recorder.registry.lock_state_checked().expect("registry");
            PendingMetadataRegistryV2::exclude_terminal_record_locked(
                &mut state,
                first_sequence,
                PendingCliTerminalV2::CliClosed,
                true,
            )
            .expect("capacity exclusion");
            PendingMetadataRegistryV2::exclude_terminal_record_locked(
                &mut state,
                second_sequence,
                PendingCliTerminalV2::CliCancelled,
                false,
            )
            .expect("allocation exclusion");
        }

        recorder.reconcile_terminal_exclusions();
        let state = recorder.state.lock().expect("state");
        assert!(state.snapshot_products.is_empty());
        assert!(state.payload_first_by_generation.is_empty());
        assert!(state.snapshot_wire_ordinals.is_empty());
        assert!(state.snapshot_evidence_captured.is_empty());
        assert_eq!(state.missing_evidence.terminal_record_capacity_excluded, 1);
        assert_eq!(state.missing_evidence.terminal_record_allocation_excluded, 1);
    }
    #[test]
    fn durability_ack_underflow_latches_structural_poison() {
        let recorder = test_recorder(208);
        while let EdgeEventDrainStatusV1::Event(_) = recorder.try_recv_event() {
            recorder.ack_event_durable().expect("durable event");
        }
        assert_eq!(
            recorder.ack_event_durable(),
            Err(EdgeMeasurementPoisonV1::EventDurabilityAckUnderflow)
        );
        let state = recorder.state.lock().expect("state");
        assert!(state.poisons.contains(&EdgeMeasurementPoisonV1::EventDurabilityAckUnderflow));
    }

    #[test]
    fn coordinator_reason_names_are_bounded_and_exact() {
        let recorder = test_recorder(209);
        recorder.latch_coordinator_failure("CanonicalWriterFatalDrain");
        recorder.latch_coordinator_failure("CanonicalWriterFatalDrain");
        recorder.latch_coordinator_failure("CutoffDrainDeadlineExceeded");

        let counts = recorder.coordinator_missing_evidence_counts();
        assert_eq!(counts.count("CanonicalWriterFatalDrain"), 2);
        assert_eq!(counts.count("CutoffDrainDeadlineExceeded"), 1);
        assert_eq!(
            counts.snapshot(),
            vec![("CanonicalWriterFatalDrain", 2), ("CutoffDrainDeadlineExceeded", 1),]
        );
    }
}
