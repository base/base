use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use alloy_primitives::{Address, B256, U256, U512, aliases::I512};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// Persisted admission schema version, deliberately independent of the edge economics schema.
pub const ADMISSION_SCHEMA_VERSION_V1: &str = "base-mev/t4a-admission/v1";
/// Default bounded queue capacity.
pub const ADMISSION_QUEUE_CAPACITY_V1: usize = 4_096;
/// Default maximum segment size before a reconciled rotation boundary.
pub const ADMISSION_MAX_SEGMENT_BYTES_V1: u64 = 64 * 1024 * 1024;

/// Exporter construction or durable-close failure.
#[derive(Debug, Error)]
pub enum AdmissionExporterErrorV1 {
    /// An identifier, capacity, or path component was invalid.
    #[error("invalid admission exporter configuration")]
    InvalidConfig,
    /// A filesystem operation failed.
    #[error("admission exporter I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The writer worker panicked.
    #[error("admission exporter worker panicked")]
    WorkerPanicked,
}

/// Immutable persisted-exporter configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionExporterConfigV1 {
    /// Already-reviewed parent directory; the exporter creates only its run directory and segments.
    pub output_root: PathBuf,
    /// Run identity used as one safe path component and in every record.
    pub run_id: String,
    /// Boot identity used as one safe filename component and in every record.
    pub boot_id: String,
    /// Bounded non-blocking producer queue capacity.
    pub queue_capacity: usize,
    /// Maximum reconciled segment size.
    pub max_segment_bytes: u64,
}

impl AdmissionExporterConfigV1 {
    /// Builds a validated configuration with production bounds.
    pub fn new(
        output_root: PathBuf,
        run_id: String,
        boot_id: String,
    ) -> Result<Self, AdmissionExporterErrorV1> {
        let config = Self {
            output_root,
            run_id,
            boot_id,
            queue_capacity: ADMISSION_QUEUE_CAPACITY_V1,
            max_segment_bytes: ADMISSION_MAX_SEGMENT_BYTES_V1,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validates path components and finite queue/rotation bounds.
    pub fn validate(&self) -> Result<(), AdmissionExporterErrorV1> {
        if !Self::safe_component(&self.run_id)
            || !Self::safe_component(&self.boot_id)
            || self.queue_capacity == 0
            || self.max_segment_bytes == 0
            || self.output_root.as_os_str().is_empty()
        {
            return Err(AdmissionExporterErrorV1::InvalidConfig);
        }
        Ok(())
    }

    fn safe_component(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }
}

/// Connection lifecycle event recorded even when no credential is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionConnectionReasonV1 {
    /// Blink credential input was absent or invalid.
    CredentialAbsent,
    /// A connection attempt failed before subscription.
    ConnectFailed,
    /// Blink subscription became live.
    Connected,
    /// A previously live subscription disconnected.
    Disconnected,
}

/// Closed economics disposition contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EconomicDispositionV1 {
    /// Analysis did not reach route economics.
    NotReached,
    /// No modeled route existed.
    NoRoute,
    /// Best modeled gross was nonpositive and cost authority was available.
    GrossNonpositive,
    /// Gross was modeled but total-cost authority was unavailable.
    AuthorityUnavailable,
    /// Modeled expected EV was nonpositive.
    EvNonpositive,
    /// Modeled expected EV was strictly positive.
    EvPositive,
}

/// Typed missing-key source; read-plan and changed-but-not-read storage are intentionally distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingKeyKindV1 {
    /// Required account balance was absent.
    AccountBalance,
    /// Required account nonce was absent.
    AccountNonce,
    /// Storage declared by the immutable read plan was absent.
    ReadPlanStorage,
    /// Victim execution changed storage that the read plan did not declare.
    ChangedButNotReadStorage,
}

/// Full typed terminal taxonomy used by persisted admission records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionTerminalReasonV1 {
    /// Raw notification could not be decoded.
    DecodeInvalid,
    /// Transaction hash did not match its bytes.
    HashInvalid,
    /// Transaction envelope type was unsupported or mismatched.
    TypeInvalid,
    /// Transaction chain identifier mismatched.
    ChainInvalid,
    /// Recovered signer mismatched.
    SignerInvalid,
    /// Frame exceeded its admission age.
    StaleFrame,
    /// Pending parent or header coherence mismatched.
    ParentHeaderMismatch,
    /// Snapshot authority changed.
    AuthorityMismatch,
    /// EVM transact returned an error.
    EvmTransactError,
    /// EVM execution reverted or halted.
    EvmRevert,
    /// Victim changed contract code.
    CodeChange,
    /// Victim delta exceeded the account cap.
    DeltaAccountCap,
    /// Victim delta exceeded the storage cap.
    DeltaStorageCap,
    /// Strict-cohort evidence was missing.
    MissingKey,
    /// Pool runtime code hash mismatched.
    PoolCodeHashMismatch,
    /// Pool-state preparation failed.
    PreparationError,
    /// V3 coverage was incomplete.
    V3Coverage,
    /// Deadline elapsed.
    Deadline,
    /// A newer frame superseded this frame.
    Cancelled,
    /// Shutdown closed an in-flight frame.
    Shutdown,
    /// No dirty pool intersected the universe.
    NoDirtyPool,
    /// No route existed.
    NoRoute,
    /// Best modeled gross was nonpositive.
    GrossNonpositive,
    /// Economics authority was unavailable.
    EconomicsAuthorityUnavailable,
    /// Modeled EV was nonpositive.
    EvNonpositive,
    /// Modeled EV was positive.
    EvPositive,
    /// T4b rejected the candidate.
    T4bReject,
    /// T4d rejected the candidate.
    T4dReject,
    /// Downstream handoff rejected the candidate.
    HandoffReject,
    /// T4e durably persisted the candidate.
    T4ePersisted,
    /// Internal processing failed closed.
    InternalFailure,
    /// Runtime admission was already closed.
    SlotClosed,
    /// Checked generation assignment overflowed.
    GenerationOverflow,
}

/// Queue disposition persisted on every terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionQueueOutcomeV1 {
    /// Every message needed for this event entered the exporter queue.
    Accepted,
    /// At least one exporter message was dropped; the segment is invalid.
    Dropped,
}

/// Immutable non-secret frame identity retained until terminalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionFrameV1 {
    /// Victim transaction hash; raw transaction bytes are never retained.
    pub victim_tx_hash: B256,
    /// Recovered sender.
    pub sender: Address,
    /// Feed-authored block number.
    pub block_number: u64,
    /// Feed-authored flashblock index.
    pub flashblock_index: u64,
    /// Immutable registry digest.
    pub registry_digest: B256,
    /// Optional cohort version.
    pub cohort_version: Option<String>,
    /// Wall-clock observation time in Unix milliseconds.
    pub observed_at_ms: u64,
    /// Fixed admission deadline in milliseconds.
    pub deadline_ms: u64,
}

/// Economics fields with disposition-specific validation and no numeric fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionEconomicsV1 {
    /// Closed disposition.
    pub disposition: EconomicDispositionV1,
    /// Signed best modeled gross decimal.
    pub best_modeled_gross_profit_wei_signed: Option<String>,
    /// Modeled retained value decimal.
    pub modeled_retained_value_wei: Option<String>,
    /// Modeled total-cost decimal.
    pub modeled_total_cost_wei: Option<String>,
    /// Signed modeled expected-EV decimal.
    pub modeled_expected_ev_wei_signed: Option<String>,
    /// Canonical nonnegative shortfall decimal.
    pub gross_shortfall_to_positive_wei: Option<String>,
}

impl AdmissionEconomicsV1 {
    /// Constructs a typed not-reached disposition.
    pub const fn not_reached() -> Self {
        Self {
            disposition: EconomicDispositionV1::NotReached,
            best_modeled_gross_profit_wei_signed: None,
            modeled_retained_value_wei: None,
            modeled_total_cost_wei: None,
            modeled_expected_ev_wei_signed: None,
            gross_shortfall_to_positive_wei: None,
        }
    }

    /// Constructs a typed no-route disposition.
    pub fn no_route() -> Self {
        Self { disposition: EconomicDispositionV1::NoRoute, ..Self::not_reached() }
    }

    /// Preserves signed gross when cost authority was unavailable.
    pub fn authority_unavailable(gross: I512) -> Self {
        Self {
            disposition: EconomicDispositionV1::AuthorityUnavailable,
            best_modeled_gross_profit_wei_signed: Some(gross.to_string()),
            modeled_retained_value_wei: None,
            modeled_total_cost_wei: None,
            modeled_expected_ev_wei_signed: None,
            gross_shortfall_to_positive_wei: None,
        }
    }

    /// Constructs a gross-nonpositive record with the canonical strict-positive shortfall.
    pub fn gross_nonpositive(
        gross: I512,
        total_cost: U256,
    ) -> Result<Self, AdmissionExporterErrorV1> {
        Ok(Self {
            disposition: EconomicDispositionV1::GrossNonpositive,
            best_modeled_gross_profit_wei_signed: Some(gross.to_string()),
            modeled_retained_value_wei: None,
            modeled_total_cost_wei: Some(total_cost.to_string()),
            modeled_expected_ev_wei_signed: None,
            gross_shortfall_to_positive_wei: Some(Self::shortfall(gross, total_cost)?.to_string()),
        })
    }

    /// Constructs a complete EV disposition.
    pub fn ev(
        gross: I512,
        retained: U256,
        total_cost: U256,
        expected_ev: I512,
    ) -> Result<Self, AdmissionExporterErrorV1> {
        Ok(Self {
            disposition: if expected_ev.is_positive() {
                EconomicDispositionV1::EvPositive
            } else {
                EconomicDispositionV1::EvNonpositive
            },
            best_modeled_gross_profit_wei_signed: Some(gross.to_string()),
            modeled_retained_value_wei: Some(retained.to_string()),
            modeled_total_cost_wei: Some(total_cost.to_string()),
            modeled_expected_ev_wei_signed: Some(expected_ev.to_string()),
            gross_shortfall_to_positive_wei: Some(Self::shortfall(gross, total_cost)?.to_string()),
        })
    }

    /// Returns `max(0, 4*(total_cost+1)-gross)` without narrowing arithmetic.
    pub fn shortfall(gross: I512, total_cost: U256) -> Result<U256, AdmissionExporterErrorV1> {
        let threshold = total_cost
            .checked_add(U256::from(1))
            .and_then(|value| value.checked_mul(U256::from(4)))
            .ok_or(AdmissionExporterErrorV1::InvalidConfig)?;
        let difference = I512::from_raw(U512::from(threshold))
            .checked_sub(gross)
            .ok_or(AdmissionExporterErrorV1::InvalidConfig)?;
        if !difference.is_positive() {
            return Ok(U256::ZERO);
        }
        U256::checked_from_limbs_slice(difference.into_raw().as_limbs())
            .ok_or(AdmissionExporterErrorV1::InvalidConfig)
    }
}

/// Terminal details supplied by the runtime without raw transaction or state values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionTerminalV1 {
    /// Last completed pipeline stage.
    pub stage: String,
    /// Typed terminal reason.
    pub reason: AdmissionTerminalReasonV1,
    /// Dirty pool count, when known.
    pub dirty_pool_count: Option<u32>,
    /// Whether at least one dirty pool belonged to the immutable universe.
    pub in_universe_dirty: Option<bool>,
    /// Whether route analysis completed.
    pub analysis_completed: bool,
    /// Typed economics fields.
    pub economics: AdmissionEconomicsV1,
    /// Optional missing-key source.
    pub missing_key_kind: Option<MissingKeyKindV1>,
    /// Optional missing-key address.
    pub missing_key_address: Option<Address>,
    /// Optional missing storage slot.
    pub missing_storage_slot: Option<U256>,
}

impl AdmissionTerminalV1 {
    /// Constructs a terminal before economics were reached.
    pub fn before_economics(stage: impl Into<String>, reason: AdmissionTerminalReasonV1) -> Self {
        Self {
            stage: stage.into(),
            reason,
            dirty_pool_count: None,
            in_universe_dirty: None,
            analysis_completed: false,
            economics: AdmissionEconomicsV1::not_reached(),
            missing_key_kind: None,
            missing_key_address: None,
            missing_storage_slot: None,
        }
    }
}

#[derive(Debug)]
enum AdmissionMessageV1 {
    Connection { sequence: u64, reason: AdmissionConnectionReasonV1, observed_at_ms: u64 },
    Received { sequence: u64, generation: u64, frame: AdmissionFrameV1 },
    Terminal { generation: u64, terminal: AdmissionTerminalV1 },
    Close,
}

/// Non-blocking producer handle and sole durable writer owner.
#[derive(Debug)]
pub struct AdmissionExporterV1 {
    sender: SyncSender<AdmissionMessageV1>,
    next_sequence: AtomicU64,
    queue_accepted: Arc<AtomicU64>,
    queue_dropped: Arc<AtomicU64>,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<Result<(), AdmissionExporterErrorV1>>>>,
}

impl AdmissionExporterV1 {
    /// Creates the reviewed run directory, opens the first 0600 no-follow segment, and starts the writer.
    pub fn start(config: AdmissionExporterConfigV1) -> Result<Arc<Self>, AdmissionExporterErrorV1> {
        config.validate()?;
        let run_directory = Self::prepare_run_directory(&config)?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let queue_accepted = Arc::new(AtomicU64::new(0));
        let queue_dropped = Arc::new(AtomicU64::new(0));
        let worker_accepted = Arc::clone(&queue_accepted);
        let worker_dropped = Arc::clone(&queue_dropped);
        let worker =
            thread::Builder::new().name("t4a-admission-writer".to_owned()).spawn(move || {
                AdmissionWriterV1::new(config, run_directory, worker_accepted, worker_dropped)?
                    .run(receiver)
            })?;
        Ok(Arc::new(Self {
            sender,
            next_sequence: AtomicU64::new(0),
            queue_accepted,
            queue_dropped,
            closed: AtomicBool::new(false),
            worker: Mutex::new(Some(worker)),
        }))
    }

    /// Records a connection lifecycle transition.
    pub fn record_connection(&self, reason: AdmissionConnectionReasonV1, observed_at_ms: u64) {
        let Some(sequence) = self.allocate_sequence() else { return };
        self.enqueue(AdmissionMessageV1::Connection { sequence, reason, observed_at_ms });
    }

    /// Starts exactly-one accounting for one checked runtime generation.
    pub fn record_received(&self, generation: u64, frame: AdmissionFrameV1) {
        let Some(sequence) = self.allocate_sequence() else { return };
        self.enqueue(AdmissionMessageV1::Received { sequence, generation, frame });
    }

    /// Completes one generation. Duplicate or unknown terminals invalidate the segment in the writer.
    pub fn record_terminal(&self, generation: u64, terminal: AdmissionTerminalV1) {
        self.enqueue(AdmissionMessageV1::Terminal { generation, terminal });
    }

    /// Returns accepted and dropped queue-message counts.
    pub fn queue_counts(&self) -> (u64, u64) {
        (self.queue_accepted.load(Ordering::Acquire), self.queue_dropped.load(Ordering::Acquire))
    }

    /// Closes in-flight frames with typed shutdown terminals and durably joins the writer once.
    pub fn close(&self) -> Result<(), AdmissionExporterErrorV1> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.sender
                .send(AdmissionMessageV1::Close)
                .map_err(|_| AdmissionExporterErrorV1::WorkerPanicked)?;
        }
        let handle =
            self.worker.lock().map_err(|_| AdmissionExporterErrorV1::WorkerPanicked)?.take();
        if let Some(handle) = handle {
            handle.join().map_err(|_| AdmissionExporterErrorV1::WorkerPanicked)??;
        }
        Ok(())
    }

    fn allocate_sequence(&self) -> Option<u64> {
        self.next_sequence
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| value.checked_add(1))
            .map_or_else(
                |_| {
                    self.queue_dropped.fetch_add(1, Ordering::Relaxed);
                    None
                },
                Some,
            )
    }

    fn enqueue(&self, message: AdmissionMessageV1) {
        if self.closed.load(Ordering::Acquire) {
            self.queue_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match self.sender.try_send(message) {
            Ok(()) => {
                self.queue_accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.queue_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn prepare_run_directory(
        config: &AdmissionExporterConfigV1,
    ) -> Result<PathBuf, AdmissionExporterErrorV1> {
        let root_metadata = fs::symlink_metadata(&config.output_root)?;
        if !root_metadata.file_type().is_dir()
            || root_metadata.file_type().is_symlink()
            || root_metadata.permissions().mode() & 0o077 != 0
        {
            return Err(AdmissionExporterErrorV1::InvalidConfig);
        }
        let run_directory = config.output_root.join(&config.run_id);
        match fs::symlink_metadata(&run_directory) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    return Err(AdmissionExporterErrorV1::InvalidConfig);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&run_directory)?;
                fs::set_permissions(&run_directory, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(run_directory)
    }
}

impl Drop for AdmissionExporterV1 {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.sender.send(AdmissionMessageV1::Close);
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(handle) = worker.take()
        {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionRecordV1<'a> {
    schema_version: &'static str,
    run_id: &'a str,
    boot_id: &'a str,
    sequence: u64,
    observed_at_ms: u64,
    event_type: &'static str,
    terminal_reason: AdmissionConnectionReasonV1,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalRecordV1<'a> {
    schema_version: &'static str,
    run_id: &'a str,
    boot_id: &'a str,
    sequence: u64,
    observed_at_ms: u64,
    victim_tx_hash: B256,
    sender: Address,
    block_number: u64,
    flashblock_index: u64,
    registry_digest: B256,
    cohort_version: &'a Option<String>,
    stage: &'a str,
    terminal_reason: AdmissionTerminalReasonV1,
    dirty_pool_count: Option<u32>,
    in_universe_dirty: Option<bool>,
    analysis_completed: bool,
    economic_disposition: EconomicDispositionV1,
    best_modeled_gross_profit_wei_signed: &'a Option<String>,
    modeled_retained_value_wei: &'a Option<String>,
    modeled_total_cost_wei: &'a Option<String>,
    modeled_expected_ev_wei_signed: &'a Option<String>,
    gross_shortfall_to_positive_wei: &'a Option<String>,
    missing_key_kind: Option<MissingKeyKindV1>,
    missing_key_address: Option<Address>,
    missing_storage_slot: Option<U256>,
    queue_outcome: AdmissionQueueOutcomeV1,
    deadline_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FooterRecordV1<'a> {
    schema_version: &'static str,
    run_id: &'a str,
    boot_id: &'a str,
    event_type: &'static str,
    segment: u64,
    received: u64,
    terminal_events: u64,
    terminal_reason_histogram: BTreeMap<AdmissionTerminalReasonV1, u64>,
    in_flight_at_close: u64,
    exporter_queue_accepted: u64,
    exporter_queue_dropped: u64,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    sequence_gaps: u64,
    sequence_duplicates: u64,
    reconciliation_mismatch: bool,
    valid: bool,
}

#[derive(Debug)]
struct PendingFrameV1 {
    sequence: u64,
    frame: AdmissionFrameV1,
    terminal: Option<AdmissionTerminalV1>,
}

#[derive(Debug)]
struct AdmissionWriterV1 {
    config: AdmissionExporterConfigV1,
    run_directory: PathBuf,
    queue_accepted: Arc<AtomicU64>,
    queue_dropped: Arc<AtomicU64>,
    segment: u64,
    file: BufWriter<File>,
    bytes_written: u64,
    pending: BTreeMap<u64, PendingFrameV1>,
    seen_sequences: BTreeSet<u64>,
    received: u64,
    terminal_events: u64,
    histogram: BTreeMap<AdmissionTerminalReasonV1, u64>,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    sequence_gaps: u64,
    sequence_duplicates: u64,
    unknown_terminals: u64,
}

impl AdmissionWriterV1 {
    fn new(
        config: AdmissionExporterConfigV1,
        run_directory: PathBuf,
        queue_accepted: Arc<AtomicU64>,
        queue_dropped: Arc<AtomicU64>,
    ) -> Result<Self, AdmissionExporterErrorV1> {
        let file = Self::open_segment(&config, &run_directory, 0)?;
        Ok(Self {
            config,
            run_directory,
            queue_accepted,
            queue_dropped,
            segment: 0,
            file: BufWriter::new(file),
            bytes_written: 0,
            pending: BTreeMap::new(),
            seen_sequences: BTreeSet::new(),
            received: 0,
            terminal_events: 0,
            histogram: BTreeMap::new(),
            first_sequence: None,
            last_sequence: None,
            sequence_gaps: 0,
            sequence_duplicates: 0,
            unknown_terminals: 0,
        })
    }

    fn run(
        mut self,
        receiver: Receiver<AdmissionMessageV1>,
    ) -> Result<(), AdmissionExporterErrorV1> {
        while let Ok(message) = receiver.recv() {
            match message {
                AdmissionMessageV1::Connection { sequence, reason, observed_at_ms } => {
                    self.observe_sequence(sequence);
                    let record = ConnectionRecordV1 {
                        schema_version: ADMISSION_SCHEMA_VERSION_V1,
                        run_id: &self.config.run_id,
                        boot_id: &self.config.boot_id,
                        sequence,
                        observed_at_ms,
                        event_type: "connection",
                        terminal_reason: reason,
                    };
                    Self::write_json(&mut self.file, &mut self.bytes_written, &record)?;
                }
                AdmissionMessageV1::Received { sequence, generation, frame } => {
                    self.observe_sequence(sequence);
                    self.received = self.received.saturating_add(1);
                    if self
                        .pending
                        .insert(generation, PendingFrameV1 { sequence, frame, terminal: None })
                        .is_some()
                    {
                        self.sequence_duplicates = self.sequence_duplicates.saturating_add(1);
                    }
                    self.flush_completed()?;
                }
                AdmissionMessageV1::Terminal { generation, terminal } => {
                    match self.pending.get_mut(&generation) {
                        Some(pending) if pending.terminal.is_none() => {
                            pending.terminal = Some(terminal)
                        }
                        Some(_) | None => {
                            self.unknown_terminals = self.unknown_terminals.saturating_add(1)
                        }
                    }
                    self.flush_completed()?;
                }
                AdmissionMessageV1::Close => break,
            }
        }
        for pending in self.pending.values_mut() {
            if pending.terminal.is_none() {
                pending.terminal = Some(AdmissionTerminalV1::before_economics(
                    "shutdown",
                    AdmissionTerminalReasonV1::Shutdown,
                ));
            }
        }
        self.flush_completed()?;
        self.finish_segment()
    }

    fn flush_completed(&mut self) -> Result<(), AdmissionExporterErrorV1> {
        loop {
            let Some((&generation, first)) = self.pending.first_key_value() else {
                break;
            };
            if first.terminal.is_none() {
                break;
            }
            let Some(mut pending) = self.pending.remove(&generation) else {
                break;
            };
            let Some(terminal) = pending.terminal.take() else {
                break;
            };
            let queue_outcome = if self.queue_dropped.load(Ordering::Acquire) == 0 {
                AdmissionQueueOutcomeV1::Accepted
            } else {
                AdmissionQueueOutcomeV1::Dropped
            };
            let record = TerminalRecordV1 {
                schema_version: ADMISSION_SCHEMA_VERSION_V1,
                run_id: &self.config.run_id,
                boot_id: &self.config.boot_id,
                sequence: pending.sequence,
                observed_at_ms: pending.frame.observed_at_ms,
                victim_tx_hash: pending.frame.victim_tx_hash,
                sender: pending.frame.sender,
                block_number: pending.frame.block_number,
                flashblock_index: pending.frame.flashblock_index,
                registry_digest: pending.frame.registry_digest,
                cohort_version: &pending.frame.cohort_version,
                stage: &terminal.stage,
                terminal_reason: terminal.reason,
                dirty_pool_count: terminal.dirty_pool_count,
                in_universe_dirty: terminal.in_universe_dirty,
                analysis_completed: terminal.analysis_completed,
                economic_disposition: terminal.economics.disposition,
                best_modeled_gross_profit_wei_signed: &terminal
                    .economics
                    .best_modeled_gross_profit_wei_signed,
                modeled_retained_value_wei: &terminal.economics.modeled_retained_value_wei,
                modeled_total_cost_wei: &terminal.economics.modeled_total_cost_wei,
                modeled_expected_ev_wei_signed: &terminal.economics.modeled_expected_ev_wei_signed,
                gross_shortfall_to_positive_wei: &terminal
                    .economics
                    .gross_shortfall_to_positive_wei,
                missing_key_kind: terminal.missing_key_kind,
                missing_key_address: terminal.missing_key_address,
                missing_storage_slot: terminal.missing_storage_slot,
                queue_outcome,
                deadline_ms: pending.frame.deadline_ms,
            };
            Self::write_json(&mut self.file, &mut self.bytes_written, &record)?;
            self.terminal_events = self.terminal_events.saturating_add(1);
            *self.histogram.entry(terminal.reason).or_default() += 1;
        }
        if self.pending.is_empty() && self.bytes_written >= self.config.max_segment_bytes {
            self.finish_segment()?;
            self.segment = self.segment.saturating_add(1);
            self.file = BufWriter::new(Self::open_segment(
                &self.config,
                &self.run_directory,
                self.segment,
            )?);
            self.bytes_written = 0;
            self.received = 0;
            self.terminal_events = 0;
            self.histogram.clear();
            self.first_sequence = None;
            self.last_sequence = None;
            self.sequence_gaps = 0;
            self.sequence_duplicates = 0;
            self.unknown_terminals = 0;
        }
        Ok(())
    }

    fn observe_sequence(&mut self, sequence: u64) {
        if !self.seen_sequences.insert(sequence) {
            self.sequence_duplicates = self.sequence_duplicates.saturating_add(1);
        }
        if let Some(last) = self.last_sequence
            && sequence > last.saturating_add(1)
        {
            self.sequence_gaps = self.sequence_gaps.saturating_add(sequence - last - 1);
        }
        self.first_sequence.get_or_insert(sequence);
        self.last_sequence = Some(self.last_sequence.map_or(sequence, |last| last.max(sequence)));
    }

    fn write_json<T: Serialize>(
        file: &mut BufWriter<File>,
        bytes_written: &mut u64,
        value: &T,
    ) -> Result<(), AdmissionExporterErrorV1> {
        let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line)?;
        *bytes_written = bytes_written.saturating_add(line.len() as u64);
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<(), AdmissionExporterErrorV1> {
        let in_flight_at_close = self.pending.len() as u64;
        let reconciliation_mismatch =
            self.received != self.terminal_events.saturating_add(in_flight_at_close);
        let dropped = self.queue_dropped.load(Ordering::Acquire);
        let valid = dropped == 0
            && self.sequence_gaps == 0
            && self.sequence_duplicates == 0
            && self.unknown_terminals == 0
            && !reconciliation_mismatch;
        let footer = FooterRecordV1 {
            schema_version: ADMISSION_SCHEMA_VERSION_V1,
            run_id: &self.config.run_id,
            boot_id: &self.config.boot_id,
            event_type: "footer",
            segment: self.segment,
            received: self.received,
            terminal_events: self.terminal_events,
            terminal_reason_histogram: self.histogram.clone(),
            in_flight_at_close,
            exporter_queue_accepted: self.queue_accepted.load(Ordering::Acquire),
            exporter_queue_dropped: dropped,
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            sequence_gaps: self.sequence_gaps,
            sequence_duplicates: self.sequence_duplicates.saturating_add(self.unknown_terminals),
            reconciliation_mismatch,
            valid,
        };
        Self::write_json(&mut self.file, &mut self.bytes_written, &footer)?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        self.run_directory_sync()?;
        Ok(())
    }

    fn open_segment(
        config: &AdmissionExporterConfigV1,
        run_directory: &Path,
        segment: u64,
    ) -> Result<File, AdmissionExporterErrorV1> {
        let path = run_directory.join(format!("{}-{segment:06}.jsonl", config.boot_id));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0o400_000)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(AdmissionExporterErrorV1::InvalidConfig);
        }
        Ok(file)
    }

    fn run_directory_sync(&self) -> Result<(), AdmissionExporterErrorV1> {
        File::open(&self.run_directory)?.sync_all()?;
        Ok(())
    }
}

/// Recomputes segment reconciliation, sequence, queue, and disposition/null invariants.
pub fn validate_admission_segment_v1(path: &Path) -> Result<bool, AdmissionExporterErrorV1> {
    fn canonical_decimal(value: &Value, signed: bool) -> bool {
        let Some(value) = value.as_str() else { return false };
        let digits = if signed { value.strip_prefix('-').unwrap_or(value) } else { value };
        !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'))
            && (!value.starts_with('-') || digits != "0")
    }

    fn economics_valid(object: &serde_json::Map<String, Value>) -> bool {
        let gross = &object["bestModeledGrossProfitWeiSigned"];
        let retained = &object["modeledRetainedValueWei"];
        let cost = &object["modeledTotalCostWei"];
        let ev = &object["modeledExpectedEvWeiSigned"];
        let shortfall = &object["grossShortfallToPositiveWei"];
        match object.get("economicDisposition").and_then(Value::as_str) {
            Some("not_reached" | "no_route") => {
                gross.is_null()
                    && retained.is_null()
                    && cost.is_null()
                    && ev.is_null()
                    && shortfall.is_null()
            }
            Some("authority_unavailable") => {
                canonical_decimal(gross, true)
                    && retained.is_null()
                    && cost.is_null()
                    && ev.is_null()
                    && shortfall.is_null()
            }
            Some("gross_nonpositive") => {
                canonical_decimal(gross, true)
                    && retained.is_null()
                    && canonical_decimal(cost, false)
                    && ev.is_null()
                    && canonical_decimal(shortfall, false)
            }
            Some("ev_nonpositive" | "ev_positive") => {
                canonical_decimal(gross, true)
                    && canonical_decimal(retained, false)
                    && canonical_decimal(cost, false)
                    && canonical_decimal(ev, true)
                    && canonical_decimal(shortfall, false)
            }
            _ => false,
        }
    }

    let bytes = fs::read(path)?;
    let mut footer = None;
    let mut terminal_events = 0u64;
    let mut sequences = BTreeSet::new();
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let value: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
        let object = value.as_object().ok_or(AdmissionExporterErrorV1::InvalidConfig)?;
        if object.get("schemaVersion").and_then(Value::as_str) != Some(ADMISSION_SCHEMA_VERSION_V1)
        {
            return Ok(false);
        }
        if object.get("eventType").and_then(Value::as_str) == Some("footer") {
            if footer.replace(value).is_some() {
                return Ok(false);
            }
            continue;
        }
        let Some(sequence) = object.get("sequence").and_then(Value::as_u64) else {
            return Ok(false);
        };
        if !sequences.insert(sequence) {
            return Ok(false);
        }
        if object.contains_key("victimTxHash") {
            terminal_events = terminal_events.saturating_add(1);
            if !economics_valid(object)
                || object.get("queueOutcome").and_then(Value::as_str) != Some("accepted")
            {
                return Ok(false);
            }
        }
    }
    let Some(footer) = footer else { return Ok(false) };
    let Some(object) = footer.as_object() else { return Ok(false) };
    let received = object.get("received").and_then(Value::as_u64);
    let footer_terminals = object.get("terminalEvents").and_then(Value::as_u64);
    let in_flight = object.get("inFlightAtClose").and_then(Value::as_u64);
    let dropped = object.get("exporterQueueDropped").and_then(Value::as_u64);
    let sequence_gaps = object.get("sequenceGaps").and_then(Value::as_u64);
    let sequence_duplicates = object.get("sequenceDuplicates").and_then(Value::as_u64);
    let reconciliation_mismatch = object.get("reconciliationMismatch").and_then(Value::as_bool);
    let contiguous = sequences
        .iter()
        .zip(sequences.iter().skip(1))
        .all(|(left, right)| left.checked_add(1) == Some(*right));
    Ok(object.get("valid").and_then(Value::as_bool) == Some(true)
        && received == footer_terminals.zip(in_flight).map(|(terminal, active)| terminal + active)
        && footer_terminals == Some(terminal_events)
        && dropped == Some(0)
        && sequence_gaps == Some(0)
        && sequence_duplicates == Some(0)
        && reconciliation_mismatch == Some(false)
        && contiguous)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use alloy_primitives::{Address, B256, U256, aliases::I512};

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("base-admission-{name}-{nonce}"));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn frame(sequence: u8) -> AdmissionFrameV1 {
        AdmissionFrameV1 {
            victim_tx_hash: B256::repeat_byte(sequence),
            sender: Address::repeat_byte(sequence),
            block_number: u64::from(sequence),
            flashblock_index: 1,
            registry_digest: B256::repeat_byte(9),
            cohort_version: None,
            observed_at_ms: 1,
            deadline_ms: 250,
        }
    }

    fn records(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn write_records(path: &Path, rows: &[Value]) {
        let mut output = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        output.push('\n');
        fs::write(path, output).unwrap();
    }

    #[test]
    fn shortfall_uses_strict_positive_threshold() {
        assert_eq!(
            AdmissionEconomicsV1::shortfall(I512::try_from(100).unwrap(), U256::from(25)).unwrap(),
            U256::from(4)
        );
        assert_eq!(
            AdmissionEconomicsV1::shortfall(I512::try_from(104).unwrap(), U256::from(25)).unwrap(),
            U256::ZERO
        );
        assert_eq!(
            AdmissionEconomicsV1::shortfall(I512::try_from(-100).unwrap(), U256::from(25)).unwrap(),
            U256::from(204)
        );
    }

    #[test]
    fn exactly_one_and_shutdown_reconcile_with_no_zero_fallback() {
        let root = temp_root("reconcile");
        let exporter = AdmissionExporterV1::start(
            AdmissionExporterConfigV1::new(root.clone(), "run-1".into(), "boot-1".into()).unwrap(),
        )
        .unwrap();
        exporter.record_received(1, frame(1));
        exporter.record_terminal(
            1,
            AdmissionTerminalV1::before_economics("frame", AdmissionTerminalReasonV1::NoDirtyPool),
        );
        exporter.record_received(2, frame(2));
        exporter.close().unwrap();

        let path = root.join("run-1/boot-1-000000.jsonl");
        let rows = records(&path);
        assert_eq!(rows.len(), 3);
        assert!(rows[0]["bestModeledGrossProfitWeiSigned"].is_null());
        assert_eq!(rows[1]["terminalReason"], "shutdown");
        assert_eq!(rows[2]["received"], 2);
        assert_eq!(rows[2]["terminalEvents"], 2);
        assert_eq!(rows[2]["inFlightAtClose"], 0);
        assert_eq!(rows[2]["exporterQueueDropped"], 0);
        assert_eq!(rows[2]["valid"], true);
        assert!(validate_admission_segment_v1(&path).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn offline_validator_rejects_each_campaign_invalidating_mutation() {
        let root = temp_root("mutations");
        let exporter = AdmissionExporterV1::start(
            AdmissionExporterConfigV1::new(root.clone(), "run-m".into(), "boot-m".into()).unwrap(),
        )
        .unwrap();
        exporter.record_received(1, frame(1));
        exporter.record_terminal(
            1,
            AdmissionTerminalV1::before_economics("frame", AdmissionTerminalReasonV1::NoDirtyPool),
        );
        exporter.record_received(2, frame(2));
        exporter.close().unwrap();
        let original_path = root.join("run-m/boot-m-000000.jsonl");
        let original = records(&original_path);
        assert!(validate_admission_segment_v1(&original_path).unwrap());

        type Mutation = (&'static str, Box<dyn Fn(&mut [Value])>);
        let mutations: [Mutation; 6] = [
            ("exactly-one", Box::new(|rows| rows[2]["terminalEvents"] = Value::from(1))),
            ("sequence-gap", Box::new(|rows| rows[1]["sequence"] = Value::from(3))),
            ("queue-drop", Box::new(|rows| rows[2]["exporterQueueDropped"] = Value::from(1))),
            ("in-flight", Box::new(|rows| rows[2]["inFlightAtClose"] = Value::from(1))),
            (
                "required-field",
                Box::new(|rows| {
                    rows[0]["economicDisposition"] = Value::from("authority_unavailable")
                }),
            ),
            (
                "zero-fallback",
                Box::new(|rows| rows[0]["bestModeledGrossProfitWeiSigned"] = Value::from("0")),
            ),
        ];
        for (name, mutate) in mutations {
            let mut rows = original.clone();
            mutate(&mut rows);
            let path = root.join(format!("{name}.jsonl"));
            write_records(&path, &rows);
            assert!(!validate_admission_segment_v1(&path).unwrap(), "{name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_terminal_invalidates_segment() {
        let root = temp_root("duplicate");
        let exporter = AdmissionExporterV1::start(
            AdmissionExporterConfigV1::new(root.clone(), "run-2".into(), "boot-2".into()).unwrap(),
        )
        .unwrap();
        exporter.record_received(1, frame(1));
        let terminal = AdmissionTerminalV1::before_economics(
            "frame",
            AdmissionTerminalReasonV1::AuthorityMismatch,
        );
        exporter.record_terminal(1, terminal.clone());
        exporter.record_terminal(1, terminal);
        exporter.close().unwrap();
        let rows = records(&root.join("run-2/boot-2-000000.jsonl"));
        assert_eq!(rows.last().unwrap()["valid"], false);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sequence_gap_invalidates_segment() {
        let root = temp_root("gap");
        let mut config =
            AdmissionExporterConfigV1::new(root.clone(), "run-3".into(), "boot-3".into()).unwrap();
        config.queue_capacity = 1;
        let exporter = AdmissionExporterV1::start(config).unwrap();
        for generation in 0..20_000 {
            exporter.record_received(generation, frame((generation % 255) as u8));
        }
        exporter.close().unwrap();
        let rows = records(&root.join("run-3/boot-3-000000.jsonl"));
        assert!(rows.last().unwrap()["exporterQueueDropped"].as_u64().unwrap() > 0);
        assert_eq!(rows.last().unwrap()["valid"], false);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_are_private_regular_and_symlink_root_is_rejected() {
        let root = temp_root("mode");
        let exporter = AdmissionExporterV1::start(
            AdmissionExporterConfigV1::new(root.clone(), "run-4".into(), "boot-4".into()).unwrap(),
        )
        .unwrap();
        exporter.close().unwrap();
        let path = root.join("run-4/boot-4-000000.jsonl");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(root).unwrap();
    }
}
