#[cfg(feature = "edge-measurement")]
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    num::NonZeroU64,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fmt::Debug,
    sync::{Arc, RwLock},
    time::Instant,
};

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
#[cfg(feature = "edge-measurement")]
use alloy_primitives::TxKind;
use alloy_primitives::{Address, B256};
#[cfg(feature = "edge-measurement")]
use base_flashblocks::{
    ClockAnchorRecordV1, EdgeEventDrainStatusV1, EdgeMeasurementGlobal,
    EdgeMeasurementInstallConfigV1, EdgeMeasurementRecorderV1, EdgeMeasurementRegistryHandleV2,
    EdgeSourceEventV1, EpochRouteV1, PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2,
    PayloadFirstObservationV1, PendingCliTerminalV2, PendingRegistryFinalSummaryV2,
    PendingTerminalRecordV2, ProcessorLifecycleProductV1, ProducerExternalBoundsV1,
    SourceConnectionRecordV1, SourceCoverageRecordV3, SourceTerminalCoverageV3,
    WireLifecycleTransitionV1,
};
use base_flashblocks::{FlashblocksAPI, FlashblocksConfig, FlashblocksState, PendingBlocks};
use base_mev_trader::{
    A1Status, BlinkFeedClient, BlinkIngressConfig, BundleVisitor, MevTraderRuntime,
    MevTraderRuntimeConfig, PayloadVisitor, PendingAccountNonce, PendingSnapshotView, PortError,
    SnapshotHandle, SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl,
    VisitSummary,
};
#[cfg(feature = "edge-measurement")]
use base_mev_trader::{
    AuditedWriteKey, EdgeCandidateDetailV1, EdgeMeasurementDurabilityV1,
    EdgeMeasurementOwnerConfigV1, EdgeMeasurementOwnerV1, EdgeProducerRecordV1,
    EdgeSnapshotEvidenceV1, ExactProtocol, PreparedPoolQuote,
};
#[cfg(feature = "t4b-shadow")]
use base_mev_trader::{
    CandidateAssemblyView, CandidateTxShapeObserver, ShadowLatestSlot, ShadowSubmit, T4bOutcome,
    T4bOutcomeCounters,
};
use base_node_runner::{BaseNodeExtension, BaseNodeRunner, FromExtensionConfig, NodeHooks};
#[cfg(feature = "t4d-shadow")]
use mev_trader_submit::{
    AdapterAwareProofBindings, BridgeError, InstalledSubmissionBridge, SealedUnsignedCandidate,
};
#[cfg(feature = "t4b-shadow")]
use mev_trader_submit::{
    SnapshotFreshnessToken, TxAuthorityAssembler, TxAuthorityError, TxAuthorityNodeError,
    TxAuthorityNodeView, TxAuthorityStateRead, ValidatedUnsignedAtomicTx,
};
#[cfg(feature = "t4b-shadow")]
use reth_provider::{AccountReader, BlockReaderIdExt, BytecodeReader};
use reth_provider::{HeaderProvider, StateProviderBox, StateProviderFactory};
#[cfg(feature = "edge-measurement")]
use serde_json::{Value as JsonValue, json};
use tracing::info;
#[cfg(feature = "edge-measurement")]
fn latch_edge_failure(failure: &'static str) {
    if let Some(recorder) = EdgeMeasurementGlobal::installed() {
        recorder.latch_coordinator_failure(failure);
    }
}
#[cfg(feature = "edge-measurement")]
fn cleanup_snapshot_task_registry(
    registry: &EdgeMeasurementRegistryHandleV2,
    failure: &'static str,
) {
    if registry.cli_cancelled().is_err() {
        latch_edge_failure(failure);
    }
}
#[cfg(feature = "edge-measurement")]
const EDGE_CUTOFF_DRAIN_DEADLINE_V1: Duration = Duration::from_secs(30);
#[cfg(feature = "edge-measurement")]
const EDGE_SHUTDOWN_JOIN_DEADLINE_V1: Duration = Duration::from_secs(30);
#[cfg(feature = "edge-measurement")]
const EDGE_CANDIDATE_DETAIL_DRAIN_BUDGET_V1: usize = 64;

#[cfg(feature = "edge-measurement")]
fn await_edge_cutoff_drain(recorder: &EdgeMeasurementRecorderV1, deadline: Instant) -> bool {
    loop {
        if recorder.cutoff_drain_complete() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[cfg(feature = "edge-measurement")]
#[derive(Debug, Default)]
struct EdgeCutoffLatchV1 {
    started: AtomicBool,
    completed: AtomicBool,
}

#[cfg(feature = "edge-measurement")]
impl EdgeCutoffLatchV1 {
    fn wait_for_completion(&self, deadline: Instant) -> bool {
        while !self.completed.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

#[cfg(feature = "edge-measurement")]
fn latch_edge_cutoff_once(
    latched: &EdgeCutoffLatchV1,
    recorder: &EdgeMeasurementRecorderV1,
    owner: &EdgeMeasurementOwnerV1,
) {
    if latched.started.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        if !latched.wait_for_completion(Instant::now() + EDGE_CUTOFF_TASK_JOIN_DEADLINE_V1) {
            latch_edge_failure("CutoffTaskCompletionTimeout");
        }
        return;
    }
    recorder.prepare_cutoff();
    match owner.prepare_cutoff() {
        Ok((blink_count, candidate_bounds)) => {
            let last_blink = blink_count.saturating_sub(1);
            let drained =
                await_edge_cutoff_drain(recorder, Instant::now() + EDGE_CUTOFF_DRAIN_DEADLINE_V1);
            if !drained {
                let unresolved = recorder.record_cutoff_drain_deadline();
                if unresolved.is_empty() {
                    latch_edge_failure("CutoffDrainDeadlineInsufficientUnresolvedIdentity");
                } else {
                    latch_edge_failure("CutoffDrainDeadlineExceeded");
                }
            }
            let last_candidate = candidate_bounds.last_sequence.unwrap_or(0);
            let last_coverage = recorder.registry().last_coverage_sequence().unwrap_or(0);
            let cutoff = recorder.latch_cutoff(ProducerExternalBoundsV1 {
                last_admitted_blink_generation: last_blink,
                last_coverage_sequence: last_coverage,
                last_candidate_sequence: last_candidate,
            });
            owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
                producer_epoch: cutoff.producer_epoch,
                cutoff_clock_observation_ordinal: cutoff.cutoff_clock_observation_ordinal,
                last_admitted_wire_ordinal: cutoff.last_admitted_wire_ordinal,
                last_admitted_source_generation: cutoff.last_admitted_source_generation,
                last_admitted_blink_generation: cutoff.last_admitted_blink_generation,
                last_pending_snapshot_sequence: cutoff.last_pending_snapshot_sequence,
                last_coverage_sequence: cutoff.last_coverage_sequence,
                last_candidate_sequence: cutoff.last_candidate_sequence,
                latch_mono_ns: cutoff.latch_mono_ns,
            });
        }
        Err(_) => latch_edge_failure("BlinkCursorUnavailableAtCutoff"),
    }
    latched.completed.store(true, Ordering::Release);
}
#[cfg(feature = "edge-measurement")]
enum EdgeShutdownTaskOutcome<T> {
    Completed(T),
    JoinFailed,
    TimedOut,
}

#[cfg(feature = "edge-measurement")]
async fn await_edge_shutdown_task<T>(
    mut handle: tokio::task::JoinHandle<T>,
    deadline: Duration,
) -> EdgeShutdownTaskOutcome<T> {
    match tokio::time::timeout(deadline, &mut handle).await {
        Ok(Ok(output)) => EdgeShutdownTaskOutcome::Completed(output),
        Ok(Err(_)) => EdgeShutdownTaskOutcome::JoinFailed,
        Err(_) => {
            handle.abort();
            EdgeShutdownTaskOutcome::TimedOut
        }
    }
}
#[cfg(feature = "edge-measurement")]
async fn abort_and_await_edge_cutoff_task(handle: Option<tokio::task::JoinHandle<()>>) {
    if let Some(mut handle) = handle {
        handle.abort();
        match tokio::time::timeout(EDGE_CUTOFF_TASK_JOIN_DEADLINE_V1, &mut handle).await {
            Ok(Err(error)) if !error.is_cancelled() => {
                latch_edge_failure("CutoffTaskJoinFailed");
            }
            Err(_) => latch_edge_failure("CutoffTaskJoinTimeout"),
            Ok(_) => {}
        }
    }
}

#[cfg(feature = "edge-measurement")]
const EDGE_SEGMENT_MAX_RECORDS: usize = 1024;
#[cfg(feature = "edge-measurement")]
const EDGE_SEGMENT_MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
#[cfg(feature = "edge-measurement")]
const EDGE_SEGMENT_MAX_FLUSH_LATENCY: Duration = Duration::from_secs(5);
#[cfg(feature = "edge-measurement")]
const EDGE_CUTOFF_TASK_JOIN_DEADLINE_V1: Duration = Duration::from_secs(65);
#[cfg(feature = "edge-measurement")]
const EDGE_CLOCK_OBSERVATION_CAPACITY: usize = 8_192;
#[cfg(feature = "edge-measurement")]
const EDGE_CANDIDATE_JOIN_CAPACITY: usize = 65_536;
#[cfg(feature = "edge-measurement")]
const EDGE_ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[cfg(feature = "edge-measurement")]
#[derive(Clone, Debug)]
struct EdgeSegmentDescriptorV1 {
    filename: String,
    first_sequence: String,
    last_sequence: String,
    record_count: String,
    segment_seal_sha256: String,
    file_sha256: String,
}

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct EdgeRollingLedgerV1 {
    name: &'static str,
    hash_domain: &'static str,
    next_sequence: u64,
    next_segment: u64,
    previous_record_hash: String,
    records: Vec<JsonValue>,
    record_bytes: usize,
    batch_started_at: Option<Instant>,
    descriptors: Vec<EdgeSegmentDescriptorV1>,
}

#[cfg(feature = "edge-measurement")]
impl EdgeRollingLedgerV1 {
    fn new(name: &'static str, hash_domain: &'static str) -> Self {
        Self {
            name,
            hash_domain,
            next_sequence: 0,
            next_segment: 0,
            previous_record_hash: EDGE_ZERO_HASH.to_owned(),
            records: Vec::new(),
            record_bytes: 0,
            batch_started_at: None,
            descriptors: Vec::new(),
        }
    }
}

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct EdgeSealedSegmentV1 {
    descriptor: EdgeSegmentDescriptorV1,
    member_sequences: Vec<u64>,
}

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct EdgeCandidateJoinV1 {
    candidate_sequence: u64,
    coverage_generation: u64,
    connection_sequence: u64,
    connection_record_hash: B256,
    registry_terminal_record_hash: B256,
}
#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct EdgeCanonicalWriterV1 {
    directory: PathBuf,
    directory_handle: Arc<File>,
    provenance: EdgeCliProducerConfigV1,
    recorder: Arc<EdgeMeasurementRecorderV1>,
    owner: Arc<EdgeMeasurementOwnerV1>,
    source_file_sequence: u64,
    producer_record_sequence: u64,
    next_registry_terminal: u64,
    ledgers: BTreeMap<&'static str, EdgeRollingLedgerV1>,
    persisted_artifacts: BTreeMap<String, String>,
    clock_observations: Vec<JsonValue>,
    missing_evidence: BTreeSet<&'static str>,
    registry_segment_sha: Vec<B256>,
    registry_record_sha: Vec<B256>,
    connection_segment_sha: Vec<(u64, B256)>,
    connection_records: Vec<B256>,
    candidate_joins: BTreeMap<u64, EdgeCandidateJoinV1>,
    candidate_validated_count: u64,
    candidate_durable: BTreeSet<u64>,
    registry_h1_segment_sha: BTreeMap<u64, (u64, B256)>,
    candidate_join_exclusions: BTreeSet<u64>,
    source_sequence_by_ledger: BTreeMap<(&'static str, u64), u64>,
    source_durable: BTreeSet<u64>,
    source_durable_ack_cursor: u64,
    registry_pending: BTreeMap<u64, PendingTerminalRecordV2>,
    registry_durable_cursor: u64,
    cutoff_batches_flushed: bool,
    finalized: bool,
}

#[cfg(feature = "edge-measurement")]
impl EdgeCanonicalWriterV1 {
    fn new(
        provenance: EdgeCliProducerConfigV1,
        recorder: Arc<EdgeMeasurementRecorderV1>,
        owner: Arc<EdgeMeasurementOwnerV1>,
    ) -> Self {
        let directory_handle = Arc::clone(&provenance.output_root_handle);
        let directory = PathBuf::from(format!("/proc/self/fd/{}", directory_handle.as_raw_fd()));
        Self {
            directory,
            directory_handle,
            provenance,
            recorder,
            owner,
            source_file_sequence: 0,
            producer_record_sequence: 0,
            next_registry_terminal: 0,
            ledgers: BTreeMap::new(),
            persisted_artifacts: BTreeMap::new(),
            missing_evidence: BTreeSet::new(),
            clock_observations: Vec::new(),
            registry_segment_sha: Vec::new(),
            registry_record_sha: Vec::new(),
            connection_segment_sha: Vec::new(),
            connection_records: Vec::new(),
            candidate_joins: BTreeMap::new(),
            candidate_validated_count: 0,
            candidate_durable: BTreeSet::new(),
            registry_h1_segment_sha: BTreeMap::new(),
            candidate_join_exclusions: BTreeSet::new(),
            source_sequence_by_ledger: BTreeMap::new(),
            source_durable: BTreeSet::new(),
            source_durable_ack_cursor: 0,
            registry_pending: BTreeMap::new(),
            registry_durable_cursor: 0,
            cutoff_batches_flushed: false,
            finalized: false,
        }
    }
    fn append_sequence_hash(
        hashes: &mut Vec<B256>,
        sequence: u64,
        hash: B256,
        mismatch: &'static str,
    ) -> io::Result<()> {
        let index = usize::try_from(sequence)
            .map_err(|_| io::Error::other("sequence does not fit usize"))?;
        if index != hashes.len() {
            return Err(io::Error::other(mismatch));
        }
        hashes.push(hash);
        Ok(())
    }

    fn sequence_hash(hashes: &[B256], sequence: u64) -> Option<B256> {
        usize::try_from(sequence).ok().and_then(|index| hashes.get(index)).copied()
    }

    fn run(mut self) -> io::Result<()> {
        self.directory_handle.sync_all()?;
        loop {
            let progressed = match self.drain_once() {
                Ok(progressed) => progressed,
                Err(error) => {
                    self.recorder.latch_coordinator_failure("CanonicalWriterFatalDrain");
                    return Err(error);
                }
            };
            if self.finalized && !progressed && !self.has_buffered_records() {
                return Ok(());
            }
            if self.recorder.cutoff_sealed()
                && !progressed
                && !self.finalized
                && !self.has_buffered_records()
            {
                match self.recorder.cutoff_drain_status() {
                    Ok(false) => {}
                    Ok(true) => match self.owner.finalization_ready() {
                        Ok(false) => {}
                        Ok(true) => {
                            if let Err(error) = self.finalize() {
                                self.recorder
                                    .latch_coordinator_failure("CanonicalWriterFatalPublication");
                                return Err(error);
                            }
                        }
                        Err(error) => {
                            let failure = if error
                                == base_mev_trader::EdgeProducerError::CutoffDrainDeadline
                            {
                                "BlinkCutoffDrainDeadlineExceeded"
                            } else {
                                "CanonicalWriterFatalBlinkFinalSeal"
                            };
                            self.recorder.latch_coordinator_failure(failure);
                            return Err(io::Error::other("fatal Blink final readiness rejection"));
                        }
                    },
                    Err(_) => {
                        self.recorder.latch_coordinator_failure("CanonicalWriterFatalFinalSeal");
                        return Err(io::Error::other("fatal source drain readiness rejection"));
                    }
                }
            }
            if !progressed {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    fn has_buffered_records(&self) -> bool {
        self.ledgers.values().any(|ledger| !ledger.records.is_empty())
    }

    fn drain_once(&mut self) -> io::Result<bool> {
        let mut progressed = false;
        let flush_cutoff = self.recorder.cutoff_sealed() && !self.cutoff_batches_flushed;
        while let EdgeEventDrainStatusV1::Event(event) = self.recorder.try_recv_event() {
            let connection_sequence = match *event {
                EdgeSourceEventV1::Connection(record) => {
                    Some((record.connection_sequence, record.record_hash))
                }
                _ => None,
            };
            let terminal_coverage_identity = match *event {
                EdgeSourceEventV1::TerminalCoverage(record) => {
                    Some((record.producer_epoch, record.coverage_sequence))
                }
                _ => None,
            };
            let ledger = match *event {
                EdgeSourceEventV1::PayloadFirst(_) => "payload-first",
                EdgeSourceEventV1::Connection(_) => "connection",
                EdgeSourceEventV1::ClockAnchor(_) => "clock",
                EdgeSourceEventV1::TerminalCoverage(record)
                    if record.route == EpochRouteV1::Authority =>
                {
                    "coverage"
                }
                EdgeSourceEventV1::TerminalCoverage(_) => "coverage-postcutoff-excluded",
                EdgeSourceEventV1::Coverage(record)
                    if record.route == EpochRouteV1::PostCutoffNonAuthority =>
                {
                    "postcutoff-diagnostic"
                }
                EdgeSourceEventV1::Coverage(_) => "coverage-detail",
                _ => "source-detail",
            };
            if let EdgeSourceEventV1::ClockAnchor(record) = *event {
                if self.clock_observations.len() < EDGE_CLOCK_OBSERVATION_CAPACITY {
                    self.clock_observations.push(self.clock_anchor_value(record));
                } else {
                    self.missing_evidence.insert("ClockObservationCapacityExcluded");
                    self.recorder.latch_coordinator_failure("ClockObservationCapacityExcluded");
                }
            }
            let value = self.source_event_value(*event);
            let state = value
                .get("terminal")
                .or_else(|| value.get("transition"))
                .or_else(|| value.get("schema"))
                .and_then(JsonValue::as_str)
                .unwrap_or(ledger)
                .to_owned();
            let domain = match ledger {
                "payload-first" => "edge-payload-first-envelope/v1",
                "connection" => "edge-source-connection/v1",
                "clock" => "edge-clock-anchor/v1",
                "coverage" | "coverage-postcutoff-excluded" => "edge-source-coverage/v3",
                "postcutoff-diagnostic" => "edge-postcutoff-diagnostic/v1",
                "coverage-detail" => "edge-source-coverage-diagnostic/v1",
                _ => "edge-source-detail/v1",
            };
            if let Some((connection_sequence, record_hash)) = connection_sequence {
                Self::append_sequence_hash(
                    &mut self.connection_records,
                    connection_sequence,
                    record_hash,
                    "connection record sequence mismatch",
                )?;
            }
            let ledger_sequence = self.ledgers.get(ledger).map_or(0, |ledger| ledger.next_sequence);
            if let Some((producer_epoch, coverage_sequence)) = terminal_coverage_identity
                && (producer_epoch != self.owner.producer_epoch()
                    || coverage_sequence != ledger_sequence)
            {
                return Err(io::Error::other(
                    "strict coverage producer or ledger sequence mismatch",
                ));
            }
            if self
                .source_sequence_by_ledger
                .insert((ledger, ledger_sequence), self.source_file_sequence)
                .is_some()
            {
                return Err(io::Error::other("duplicate source ledger sequence"));
            }
            let record = if ledger == "payload-first" {
                let record_hash = value
                    .get("recordHash")
                    .cloned()
                    .ok_or_else(|| io::Error::other("payload-first authority hash missing"))?;
                json!({
                    "payloadFirst": value,
                    "payloadFirstRecordHash": record_hash,
                    "schema": "edge-payload-first-envelope/v1",
                })
            } else {
                value
            };
            for segment in self.push_rolling_record(ledger, domain, &state, record)? {
                self.ack_source_segment(ledger, &segment)?;
            }
            self.source_file_sequence = self
                .source_file_sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("source file sequence overflow"))?;
            progressed = true;
        }
        let flush_source = flush_cutoff;
        for ledger in [
            "payload-first",
            "connection",
            "clock",
            "coverage",
            "coverage-postcutoff-excluded",
            "postcutoff-diagnostic",
            "coverage-detail",
            "source-detail",
        ] {
            if (flush_source || self.rolling_flush_due(ledger, Instant::now()))
                && let Some(segment) = self.flush_rolling_ledger(ledger)?
            {
                self.ack_source_segment(ledger, &segment)?;
                progressed = true;
            }
        }

        let registry = self.recorder.registry();
        loop {
            let Some(terminal) =
                self.recorder.next_terminal_durable_record(self.next_registry_terminal)
            else {
                break;
            };
            let coverage_sequence = terminal.coverage_sequence;
            if coverage_sequence != self.next_registry_terminal {
                return Err(io::Error::other("registry terminal read cursor mismatch"));
            }
            let pending_sequence = terminal.metadata.identity.pending_snapshot_sequence;
            let h2_value = Self::registry_h2_value(terminal);
            let record_hash =
                B256::new(EdgeMeasurementDurabilityV1::sha256(&Self::canonical_bytes(&h2_value)?));
            Self::append_sequence_hash(
                &mut self.registry_record_sha,
                coverage_sequence,
                record_hash,
                "registry terminal record sequence mismatch",
            )?;
            if self.registry_pending.insert(coverage_sequence, terminal).is_some() {
                return Err(io::Error::other("duplicate registry terminal record"));
            }

            let h1_value = Self::registry_h1_value(terminal);
            for segment in self.push_rolling_record(
                "registry-h1",
                "edge-pending-registry-h1/v1",
                "H1",
                h1_value,
            )? {
                self.record_registry_segment("registry-h1", &segment)?;
            }
            for segment in self.push_rolling_record(
                "registry-h2",
                "edge-pending-registry-h2/v1",
                "H2",
                h2_value,
            )? {
                self.record_registry_segment("registry-h2", &segment)?;
            }
            let inserted_pending_sequence = self
                .registry_pending
                .get(&coverage_sequence)
                .ok_or_else(|| io::Error::other("registry terminal insert missing"))?
                .metadata
                .identity
                .pending_snapshot_sequence;
            if pending_sequence != inserted_pending_sequence {
                return Err(io::Error::other("registry pending identity changed"));
            }
            self.next_registry_terminal = self
                .next_registry_terminal
                .checked_add(1)
                .ok_or_else(|| io::Error::other("registry terminal cursor overflow"))?;
            progressed = true;
        }
        let flush_registry = flush_cutoff;
        for ledger in ["registry-h1", "registry-h2"] {
            if (flush_registry || self.rolling_flush_due(ledger, Instant::now()))
                && let Some(segment) = self.flush_rolling_ledger(ledger)?
            {
                self.record_registry_segment(ledger, &segment)?;
                progressed = true;
            }
        }
        while let Some(terminal) = self.registry_pending.get(&self.registry_durable_cursor).copied()
        {
            let coverage_sequence = terminal.coverage_sequence;
            let pending_sequence = terminal.metadata.identity.pending_snapshot_sequence;
            if Self::sequence_hash(&self.registry_segment_sha, coverage_sequence).is_none()
                || self.registry_h1_segment_sha.get(&pending_sequence).is_none_or(
                    |(stored_coverage_sequence, _)| *stored_coverage_sequence != coverage_sequence,
                )
            {
                break;
            }
            let terminal_hash =
                Self::sequence_hash(&self.registry_record_sha, coverage_sequence)
                    .ok_or_else(|| io::Error::other("registry H2 record hash missing"))?;
            registry
                .ack_terminal_durable(coverage_sequence)
                .map_err(|_| io::Error::other("registry durable ACK mismatch"))?;
            self.recorder
                .record_terminal_durable(terminal, terminal_hash)
                .map_err(|_| io::Error::other("terminal durable cleanup mismatch"))?;
            self.registry_pending.remove(&coverage_sequence);
            self.registry_durable_cursor = self
                .registry_durable_cursor
                .checked_add(1)
                .ok_or_else(|| io::Error::other("registry durable cursor overflow"))?;
            progressed = true;
        }

        for record in
            self.owner.drain_records().map_err(|_| io::Error::other("Blink record drain failed"))?
        {
            let sequence = self.producer_record_sequence;
            self.producer_record_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("producer record sequence overflow"))?;
            let (ledger, _sequence, value) = match record {
                EdgeProducerRecordV1::BlinkReject(record) => {
                    let sequence = record
                        .sequence
                        .parse()
                        .map_err(|_| io::Error::other("Blink reject sequence is not canonical"))?;
                    (
                        "blink-reject",
                        sequence,
                        json!({
                            "branchId": record.branch_id,
                            "previousRecordHash": record.previous_record_hash,
                            "producerEpoch": record.producer_epoch,
                            "reason": record.reason,
                            "recordHash": record.record_hash,
                            "schema": record.schema,
                            "sequence": record.sequence,
                            "state": record.state,
                        }),
                    )
                }
                EdgeProducerRecordV1::CandidateDrop { generation, reason } => (
                    "candidate-drop",
                    sequence,
                    json!({
                        "generation": generation.to_string(),
                        "reason": reason,
                        "schema": "edge-candidate-drop/v3",
                        "sequence": sequence.to_string(),
                    }),
                ),
            };
            let state = value
                .get("state")
                .or_else(|| value.get("schema"))
                .and_then(JsonValue::as_str)
                .unwrap_or(ledger)
                .to_owned();
            let domain = if ledger == "blink-reject" {
                "edge-blink-reject/v3"
            } else {
                "edge-writer-diagnostic/v1"
            };
            self.push_rolling_record(ledger, domain, &state, value)?;
            progressed = true;
        }
        for ledger in ["blink-reject", "candidate-drop"] {
            if (flush_cutoff || self.rolling_flush_due(ledger, Instant::now()))
                && self.flush_rolling_ledger(ledger)?.is_some()
            {
                progressed = true;
            }
        }
        let mut candidate_detail_queue_drained = false;
        for _ in 0..EDGE_CANDIDATE_DETAIL_DRAIN_BUDGET_V1 {
            let Some(detail) = self
                .owner
                .try_receive_candidate_detail()
                .map_err(|_| io::Error::other("candidate detail receive failed"))?
            else {
                candidate_detail_queue_drained = true;
                break;
            };
            detail
                .validate_detail()
                .map_err(|_| io::Error::other("candidate detail validation failed"))?;
            let sequence = detail.candidate_sequence;
            if self
                .ledgers
                .get("candidate")
                .map_or(sequence != 0, |ledger| ledger.next_sequence != sequence)
            {
                return Err(io::Error::other("candidate detail sequence mismatch"));
            }
            let (state, value) = if self.candidate_joins.len() >= EDGE_CANDIDATE_JOIN_CAPACITY {
                self.missing_evidence.insert("CandidateCompactJoinCapacityExcluded");
                self.recorder.latch_coordinator_failure("CandidateCompactJoinCapacityExcluded");
                if !self.candidate_join_exclusions.insert(sequence) {
                    return Err(io::Error::other("duplicate candidate join exclusion"));
                }
                (
                    "CandidateDetailExcluded",
                    Self::compact_candidate_detail_exclusion(
                        &Self::candidate_detail_value(detail)?,
                        "CandidateCompactJoinCapacityExcluded",
                    )?,
                )
            } else {
                let join = EdgeCandidateJoinV1 {
                    candidate_sequence: sequence,
                    coverage_generation: detail.coverage_generation,
                    connection_sequence: detail.connection_sequence_at_capture,
                    connection_record_hash: B256::new(detail.connection_record_hash_at_capture),
                    registry_terminal_record_hash: B256::new(detail.registry_terminal_record_hash),
                };
                if self.candidate_joins.insert(sequence, join).is_some() {
                    return Err(io::Error::other("duplicate candidate detail sequence"));
                }
                ("CandidateDetail", Self::candidate_detail_value(detail)?)
            };
            for segment in
                self.push_rolling_record("candidate", "edge-candidate-detail/v1", state, value)?
            {
                self.validate_candidate_segment(&segment)?;
            }
            progressed = true;
        }
        if (flush_cutoff || self.rolling_flush_due("candidate", Instant::now()))
            && let Some(segment) = self.flush_rolling_ledger("candidate")?
        {
            self.validate_candidate_segment(&segment)?;
            progressed = true;
        }
        if self.release_candidate_joins()? {
            progressed = true;
        }
        if flush_cutoff && candidate_detail_queue_drained {
            self.cutoff_batches_flushed = true;
        }
        Ok(progressed)
    }

    fn source_event_value(&self, event: EdgeSourceEventV1) -> JsonValue {
        match event {
            EdgeSourceEventV1::PayloadFirst(record) => Self::payload_first_value(record),
            EdgeSourceEventV1::Connection(record) => Self::connection_value(record),
            EdgeSourceEventV1::ClockAnchor(record) => self.clock_anchor_value(record),
            EdgeSourceEventV1::Processor(record) => Self::processor_value(record),
            EdgeSourceEventV1::Coverage(record) => Self::coverage_value(record),
            EdgeSourceEventV1::TerminalCoverage(record) => Self::terminal_coverage_value(record),
            EdgeSourceEventV1::Cutoff(record) => json!({
                "cutoffClockObservationOrdinal": record.cutoff_clock_observation_ordinal.to_string(),
                "lastAdmittedBlinkGeneration": record.last_admitted_blink_generation.to_string(),
                "lastAdmittedSourceGeneration": record.last_admitted_source_generation.to_string(),
                "lastAdmittedWireOrdinal": record.last_admitted_wire_ordinal.to_string(),
                "lastCandidateSequence": record.last_candidate_sequence.to_string(),
                "lastCoverageSequence": record.last_coverage_sequence.to_string(),
                "lastPendingSnapshotSequence": record.last_pending_snapshot_sequence.to_string(),
                "latchMonoNs": record.latch_mono_ns.to_string(),
                "producerEpoch": record.producer_epoch.to_string(),
                "recordHash": Self::hex(record.record_hash.as_slice()),
            }),
        }
    }
    fn terminal_coverage_value(record: SourceTerminalCoverageV3) -> JsonValue {
        json!({
            "coverageSequence": record.coverage_sequence.to_string(),
            "payloadFirstRecordHash": record
                .payload_first_record_hash
                .map(|value| JsonValue::String(Self::hex(value.as_slice()))),
            "pendingSnapshotSequence": record
                .pending_snapshot_sequence
                .map(|value| JsonValue::String(value.to_string())),
            "producerEpoch": record.producer_epoch.to_string(),
            "schema": "edge-source-coverage/v3",
            "sourceGeneration": record
                .source_generation
                .map(|value| JsonValue::String(value.to_string())),
            "terminal": record.terminal.wire_name(),
            "terminalHash": Self::hex(record.terminal_hash.as_slice()),
        })
    }

    fn coverage_value(record: SourceCoverageRecordV3) -> JsonValue {
        let (transition, source_generation, cache_disposition, pending_sequence, terminal) =
            match record.transition {
                WireLifecycleTransitionV1::WireObserved => {
                    ("WireObserved", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::DecodeSucceeded { source_generation } => {
                    ("DecodeSucceeded", Some(source_generation), None, None, None)
                }
                WireLifecycleTransitionV1::DecodeRejected => {
                    ("DecodeRejected", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::ActorEnqueueSucceeded => {
                    ("ActorEnqueueSucceeded", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::ActorEnqueueFailed => {
                    ("ActorEnqueueFailed", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::ActorDelivered => {
                    ("ActorDelivered", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::StateHandoffSucceeded => {
                    ("StateHandoffSucceeded", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::StateHandoffFailed => {
                    ("StateHandoffFailed", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::ProcessorTerminal => {
                    ("ProcessorTerminal", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::CacheAwait { disposition } => (
                    "CacheAwait",
                    record.source_generation,
                    Some(disposition.wire_name()),
                    None,
                    None,
                ),
                WireLifecycleTransitionV1::CacheClaimed => {
                    ("CacheClaimed", record.source_generation, None, None, None)
                }
                WireLifecycleTransitionV1::CliTerminal { pending_snapshot_sequence, terminal } => (
                    "CliTerminal",
                    record.source_generation,
                    None,
                    Some(pending_snapshot_sequence),
                    Some(Self::cli_terminal_value(terminal)),
                ),
                WireLifecycleTransitionV1::PostCutoffDecoded => {
                    ("PostCutoffDecoded", None, None, None, None)
                }
                WireLifecycleTransitionV1::PostCutoffActorEnqueued => {
                    ("PostCutoffActorEnqueued", None, None, None, None)
                }
                WireLifecycleTransitionV1::PostCutoffActorDelivered => {
                    ("PostCutoffActorDelivered", None, None, None, None)
                }
                WireLifecycleTransitionV1::PostCutoffStateHandedOff => {
                    ("PostCutoffStateHandedOff", None, None, None, None)
                }
                WireLifecycleTransitionV1::PostCutoffProcessorExcluded => {
                    ("PostCutoffProcessorExcluded", None, None, None, None)
                }
                WireLifecycleTransitionV1::PostCutoffNonAuthority => {
                    ("PostCutoffNonAuthority", None, None, None, None)
                }
            };
        json!({
            "cacheDisposition": cache_disposition,
            "coverageSequence": record.coverage_sequence.to_string(),
            "pendingSnapshotSequence": pending_sequence.map(|value| JsonValue::String(value.to_string())),
            "previousRecordHash": Self::hex(record.previous_record_hash.as_slice()),
            "producerEpoch": record.producer_epoch.to_string(),
            "recordHash": Self::hex(record.record_hash.as_slice()),
            "route": match record.route {
                EpochRouteV1::Authority => "Authority",
                EpochRouteV1::PostCutoffNonAuthority => "PostCutoffNonAuthority",
            },
            "sourceGeneration": source_generation.map(|value| JsonValue::String(value.to_string())),
            "terminal": terminal,
            "transition": transition,
            "wireOrdinal": record.wire_ordinal.to_string(),
        })
    }

    fn cli_terminal_value(terminal: PendingCliTerminalV2) -> JsonValue {
        match terminal {
            PendingCliTerminalV2::CliReceivedLookupSucceeded => {
                json!({"disposition": "CliReceivedLookupSucceeded"})
            }
            PendingCliTerminalV2::CliRegistryLookupFailed(reason) => json!({
                "disposition": "CliRegistryLookupFailed",
                "failure": Self::lookup_failure_wire(reason),
            }),
            PendingCliTerminalV2::CliLagged => json!({"disposition": "CliLagged"}),
            PendingCliTerminalV2::CliClosed => json!({"disposition": "CliClosed"}),
            PendingCliTerminalV2::CliCancelled => json!({"disposition": "CliCancelled"}),
            PendingCliTerminalV2::NoReceivers => json!({"disposition": "NoReceivers"}),
            PendingCliTerminalV2::RegistrationFailedNoReceivers => {
                json!({"disposition": "RegistrationFailedNoReceivers"})
            }
        }
    }

    fn payload_first_value(record: PayloadFirstObservationV1) -> JsonValue {
        json!({
            "blockNumber": record.key.block_number.to_string(),
            "bootId": String::from_utf8_lossy(&record.boot_id),
            "clockObservationOrdinal": record.observation.clock_observation_ordinal.to_string(),
            "clockSourceVersion": base_flashblocks::EDGE_CLOCK_SOURCE_VERSION_V1,
            "index0StructuralIdentity": {
                "blockNumber": record.key.block_number.to_string(),
                "canonicalWireDigest": Self::hex(record.observation.wire_digest.as_slice()),
                "flashblockIndex": "0",
                "payloadId": format!("0x{}", Self::hex(&record.key.payload_id)),
                "previousFlashblockId": JsonValue::Null,
            },
            "monoNs": record.observation.mono_ns.map(|value| JsonValue::String(value.to_string())),
            "monoStatus": record.observation.mono_status.wire_name(),
            "monotonicResolutionNs": record.monotonic_resolution_ns.to_string(),
            "payloadId": format!("0x{}", Self::hex(&record.key.payload_id)),
            "previousRecordHash": Self::hex(record.previous_record_hash.as_slice()),
            "producerEpoch": record.key.producer_epoch.to_string(),
            "realtimeResolutionNs": record.realtime_resolution_ns.to_string(),
            "recordHash": Self::hex(record.record_hash.as_slice()),
            "recordSequence": record.record_sequence.to_string(),
            "utcNs": record.observation.utc_ns.map(|value| JsonValue::String(value.to_string())),
            "utcStatus": record.observation.utc_status.wire_name(),
            "wireDigest": Self::hex(record.observation.wire_digest.as_slice()),
        })
    }

    fn connection_value(record: SourceConnectionRecordV1) -> JsonValue {
        let transition = record.transition.wire_name();
        json!({
            "clockObservationOrdinal": record.clock_observation_ordinal.map(|value| JsonValue::String(value.to_string())),
            "connectionSequence": record.connection_sequence.to_string(),
            "errorClass": record.error_class.map(|value| JsonValue::String(value.wire_name().to_owned())),
            "monoNs": record.mono_ns.map(|value| JsonValue::String(value.to_string())),
            "previousRecordHash": Self::hex(record.previous_record_hash.as_slice()),
            "producerEpoch": record.producer_epoch.to_string(),
            "recordHash": Self::hex(record.record_hash.as_slice()),
            "schema": "edge-source-connection/v1",
            "sequence": record.connection_sequence.to_string(),
            "state": transition,
            "transition": transition,
        })
    }

    fn clock_anchor_value(&self, record: ClockAnchorRecordV1) -> JsonValue {
        let boot_id = self.recorder.boot_id();
        let (pair_status, disposition, failure) = match (
            record.observation.utc_status.wire_name(),
            record.observation.mono_status.wire_name(),
        ) {
            ("Ok", "Ok") => ("BothOk", "Sampled", JsonValue::Null),
            ("Failed", "Ok") => {
                ("RealtimeFailedMonotonicOk", "Failed", json!("RealtimeSyscallFailed"))
            }
            ("Ok", "Failed") => {
                ("RealtimeOkMonotonicFailed", "Failed", json!("MonotonicSyscallFailed"))
            }
            _ => ("BothFailed", "Failed", json!("BothSyscallsFailed")),
        };
        let mut value = json!({
            "anchorKind": if record.startup { "Startup" } else { "Periodic" },
            "anchorSequence": record.anchor_sequence.to_string(),
            "bootId": String::from_utf8_lossy(&boot_id),
            "clockObservationOrdinal": record.observation.clock_observation_ordinal.to_string(),
            "clockSourceVersion": base_flashblocks::EDGE_CLOCK_SOURCE_VERSION_V1,
            "disposition": disposition,
            "dueMonoNs": record.due_mono_ns.to_string(),
            "failureEvidence": failure,
            "kind": "Anchor",
            "monoNs": record.observation.mono_ns.map(|value| JsonValue::String(value.to_string())),
            "monotonicResolutionNs": self.recorder.monotonic_resolution_ns().to_string(),
            "pairStatus": pair_status,
            "persistenceSequence": record.anchor_sequence.to_string(),
            "previousAnchorHash": Self::hex(record.previous_anchor_hash.as_slice()),
            "producerEpoch": record.producer_epoch.to_string(),
            "realtimeResolutionNs": self.recorder.realtime_resolution_ns().to_string(),
            "recordHash": Self::hex(record.record_hash.as_slice()),
            "sampledMonoNs": record.sampled_mono_ns.to_string(),
            "schema": "edge-clock-anchor/v1",
            "utcNs": record.observation.utc_ns.map(|value| JsonValue::String(value.to_string())),
        });
        if let Some(object) = value.as_object_mut() {
            if record.observation.mono_ns.is_none() {
                object.remove("monoNs");
            }
            if record.observation.utc_ns.is_none() {
                object.remove("utcNs");
            }
        }
        value
    }

    fn processor_value(record: ProcessorLifecycleProductV1) -> JsonValue {
        let (publish, receiver_count) = record.publish_disposition.wire_parts();
        json!({
            "baseDisposition": record.base_disposition.wire_name(),
            "cacheResolvedFinalDisposition": record.cache_resolved_final_disposition.map(|value| JsonValue::String(value.wire_name().to_owned())),
            "observerDisposition": record.observer_disposition.wire_name(),
            "payloadFirstRecordHash": record.payload_first_record_hash.map(|value| JsonValue::String(Self::hex(value.as_slice()))),
            "pendingSnapshotSequence": record.pending_snapshot_sequence.map(|value| JsonValue::String(value.to_string())),
            "processorErrorReason": record.processor_error_reason,
            "producerEpoch": record.producer_epoch.to_string(),
            "publishDisposition": publish,
            "receiverCount": receiver_count.map(|value| JsonValue::String(value.to_string())),
            "sourceGeneration": record.source_generation.to_string(),
            "structuralTerminalHash": Self::hex(record.structural_terminal_hash.as_slice()),
        })
    }

    fn registry_h1_value(record: PendingTerminalRecordV2) -> JsonValue {
        json!({
            "pendingPublicSubsetDigestV1": Self::hex(record.metadata.pending_public_subset_digest_v1.as_slice()),
            "pendingSnapshotSequence": record.metadata.identity.pending_snapshot_sequence.to_string(),
        })
    }

    const fn accounting_field_wire(
        field: base_flashblocks::PendingAccountingFieldV2,
    ) -> &'static str {
        match field {
            base_flashblocks::PendingAccountingFieldV2::AdvancedWithSnapshot => {
                "AdvancedWithSnapshot"
            }
            base_flashblocks::PendingAccountingFieldV2::RegistrationSucceeded => {
                "RegistrationSucceeded"
            }
            base_flashblocks::PendingAccountingFieldV2::RegistrationFailed => "RegistrationFailed",
            base_flashblocks::PendingAccountingFieldV2::SendPublished => "SendPublished",
            base_flashblocks::PendingAccountingFieldV2::SendNoReceivers => "SendNoReceivers",
            base_flashblocks::PendingAccountingFieldV2::CliReceivedLookupSucceeded => {
                "CliReceivedLookupSucceeded"
            }
            base_flashblocks::PendingAccountingFieldV2::CliRegistryLookupFailed => {
                "CliRegistryLookupFailed"
            }
            base_flashblocks::PendingAccountingFieldV2::CliLaggedAttributed => {
                "CliLaggedAttributed"
            }
            base_flashblocks::PendingAccountingFieldV2::CliClosedAttributed => {
                "CliClosedAttributed"
            }
            base_flashblocks::PendingAccountingFieldV2::CliCancelledAttributed => {
                "CliCancelledAttributed"
            }
        }
    }

    fn registration_failure_wire(
        reason: base_flashblocks::PendingRegistrationFailure,
    ) -> JsonValue {
        use base_flashblocks::PendingRegistrationFailure;
        match reason {
            PendingRegistrationFailure::PendingSnapshotSequenceOverflow => {
                json!({"reason": "PendingSnapshotSequenceOverflow"})
            }
            PendingRegistrationFailure::PendingAccountingOverflow(field) => json!({
                "accountingField": Self::accounting_field_wire(field),
                "reason": "PendingAccountingOverflow",
            }),
            PendingRegistrationFailure::PendingRegistryCapacityOverflow => {
                json!({"reason": "PendingRegistryCapacityOverflow"})
            }
            PendingRegistrationFailure::PendingRegistryLockPoisoned => {
                json!({"reason": "PendingRegistryLockPoisoned"})
            }
            PendingRegistrationFailure::PendingPointerBindingConflict => {
                json!({"reason": "PendingPointerBindingConflict"})
            }
            PendingRegistrationFailure::PendingArcIdentityExpired => {
                json!({"reason": "PendingArcIdentityExpired"})
            }
        }
    }

    fn lookup_failure_wire(reason: base_flashblocks::CliRegistryLookupFailureReason) -> JsonValue {
        use base_flashblocks::CliRegistryLookupFailureReason;
        match reason {
            CliRegistryLookupFailureReason::NoPublishedSequence => {
                json!({"reason": "NoPublishedSequence"})
            }
            CliRegistryLookupFailureReason::RegistrationFailed(reason) => json!({
                "reason": "RegistrationFailed",
                "registrationFailure": Self::registration_failure_wire(reason),
            }),
            CliRegistryLookupFailureReason::MissingPrimaryEntry => {
                json!({"reason": "MissingPrimaryEntry"})
            }
            CliRegistryLookupFailureReason::PendingPointerBindingConflict => {
                json!({"reason": "PendingPointerBindingConflict"})
            }
            CliRegistryLookupFailureReason::PendingArcIdentityExpired => {
                json!({"reason": "PendingArcIdentityExpired"})
            }
            CliRegistryLookupFailureReason::PendingArcIdentityMismatch => {
                json!({"reason": "PendingArcIdentityMismatch"})
            }
            CliRegistryLookupFailureReason::PendingPublicSubsetCorruption => {
                json!({"reason": "PendingPublicSubsetCorruption"})
            }
            CliRegistryLookupFailureReason::PassthroughNonAdvanced => {
                json!({"reason": "PassthroughNonAdvanced"})
            }
            CliRegistryLookupFailureReason::PostCutoffAdvancedNonAuthority => {
                json!({"reason": "PostCutoffAdvancedNonAuthority"})
            }
            CliRegistryLookupFailureReason::PendingAccountingOverflow(field) => json!({
                "accountingField": Self::accounting_field_wire(field),
                "reason": "PendingAccountingOverflow",
            }),
        }
    }

    fn registry_h2_value(record: PendingTerminalRecordV2) -> JsonValue {
        use base_flashblocks::{PendingRegistrationDispositionV2, PendingSendDispositionV2};
        let registration = match record.registration {
            PendingRegistrationDispositionV2::Succeeded => json!({"disposition": "Succeeded"}),
            PendingRegistrationDispositionV2::Failed(reason) => json!({
                "disposition": "Failed",
                "failure": Self::registration_failure_wire(reason),
            }),
        };
        let send = match record.send {
            PendingSendDispositionV2::Published { receiver_count } => json!({
                "disposition": "Published",
                "receiverCount": receiver_count.to_string(),
            }),
            PendingSendDispositionV2::NoReceivers => json!({
                "disposition": "NoReceivers",
                "receiverCount": "0",
            }),
        };
        let terminal = Self::cli_terminal_value(record.terminal);
        json!({
            "coverageSequence": record.coverage_sequence.to_string(),
            "pendingPublicSubsetDigestV1": Self::hex(record.metadata.pending_public_subset_digest_v1.as_slice()),
            "pendingSnapshotSequence": record.metadata.identity.pending_snapshot_sequence.to_string(),
            "producerEpoch": record.metadata.identity.producer_epoch.to_string(),
            "registration": registration,
            "send": send,
            "sourceGeneration": record.metadata.source_generation.map(|value| JsonValue::String(value.to_string())),
            "terminal": terminal,
        })
    }

    const fn protocol_name(protocol: ExactProtocol) -> &'static str {
        match protocol {
            ExactProtocol::UniswapV2 => "UniswapV2",
            ExactProtocol::AerodromeVolatile => "AerodromeVolatile",
            ExactProtocol::AerodromeStable => "AerodromeStable",
            ExactProtocol::UniswapV3 => "UniswapV3",
        }
    }

    fn prepared_quote_value(quote: &PreparedPoolQuote) -> JsonValue {
        match quote {
            PreparedPoolQuote::ConstantProduct { reserve0, reserve1 } => json!({
                "variant": "ConstantProduct",
                "reserve0": reserve0.to_string(),
                "reserve1": reserve1.to_string(),
            }),
            PreparedPoolQuote::Stable { reserve0, reserve1 } => json!({
                "variant": "Stable",
                "reserve0": reserve0.to_string(),
                "reserve1": reserve1.to_string(),
            }),
            PreparedPoolQuote::V3 { sqrt_price_x96, liquidity, tick, tick_spacing, ticks } => {
                json!({
                    "variant": "V3",
                    "sqrtPriceX96": sqrt_price_x96.to_string(),
                    "liquidity": liquidity.to_string(),
                    "tick": tick.to_string(),
                    "tickSpacing": tick_spacing.to_string(),
                    "ticks": ticks.iter().map(|initialized| json!({
                        "tick": initialized.tick.to_string(),
                        "liquidityNet": initialized.liquidity_net.to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
        }
    }

    fn materialized_write_value(write: &base_mev_trader::MaterializedWrite) -> JsonValue {
        let (variant, address, slot, evidence_digest) = match write.key {
            AuditedWriteKey::AccountBalance { address, evidence_digest } => {
                ("AccountBalance", address, JsonValue::String("0".to_owned()), evidence_digest)
            }
            AuditedWriteKey::AccountNonce { address, evidence_digest } => {
                ("AccountNonce", address, JsonValue::String("0".to_owned()), evidence_digest)
            }
            AuditedWriteKey::Storage { address, slot, evidence_digest } => {
                ("Storage", address, JsonValue::String(slot.to_string()), evidence_digest)
            }
        };
        json!({
            "variant": variant,
            "address": format!("0x{}", Self::hex(address.as_slice())),
            "slot": slot,
            "evidenceDigest": format!("0x{}", Self::hex(evidence_digest.as_slice())),
            "value": write.value.to_string(),
        })
    }

    fn candidate_detail_value(candidate: EdgeCandidateDetailV1) -> io::Result<JsonValue> {
        let transaction = &candidate.backrun_measurement_tx;
        let nonce = transaction.nonce_witness;
        let selected_hops = candidate
            .selected_plan
            .route
            .iter()
            .zip(candidate.execution_hops)
            .map(|(hop, execution)| {
                json!({
                    "adapter": format!("0x{}", Self::hex(execution.adapter.as_slice())),
                    "feePips": hop.fee_pips.to_string(),
                    "fundingTarget": format!("0x{}", Self::hex(execution.funding_target.as_slice())),
                    "minAmountOut": execution.min_amount_out.to_string(),
                    "pool": format!("0x{}", Self::hex(hop.pool.as_slice())),
                    "protocol": Self::protocol_name(hop.protocol),
                    "tokenIn": format!("0x{}", Self::hex(hop.token_in.as_slice())),
                    "tokenOut": format!("0x{}", Self::hex(hop.token_out.as_slice())),
                })
            })
            .collect::<Vec<_>>();
        let prepared_route = candidate
            .prepared_route
            .iter()
            .map(|pool| {
                json!({
                    "decimals0": pool.decimals0.to_string(),
                    "decimals1": pool.decimals1.to_string(),
                    "feePips": pool.fee_pips.to_string(),
                    "pool": format!("0x{}", Self::hex(pool.pool.as_slice())),
                    "protocol": Self::protocol_name(pool.protocol),
                    "quote": Self::prepared_quote_value(&pool.quote),
                    "token0": format!("0x{}", Self::hex(pool.token0.as_slice())),
                    "token1": format!("0x{}", Self::hex(pool.token1.as_slice())),
                })
            })
            .collect::<Vec<_>>();
        Ok(Self::stringify_numbers(json!({
            "backrunMeasurementTx": {
                "calldata": format!("0x{}", Self::hex(&transaction.calldata)),
                "nonceWitness": {
                    "committed": nonce.committed.to_string(),
                    "parentBlockHash": format!("0x{}", Self::hex(nonce.parent_block_hash.as_slice())),
                    "parentBlockNumber": nonce.parent_block_number.to_string(),
                    "pendingCurrent": nonce.pending_current.map(|value| JsonValue::String(value.to_string())),
                    "pendingOriginal": nonce.pending_original.map(|value| JsonValue::String(value.to_string())),
                },
                "selectedNonce": transaction.selected_nonce.to_string(),
                "snapshotBaseFeePerGas": transaction.snapshot_base_fee_per_gas.to_string(),
                "targetTxHash": format!("0x{}", Self::hex(transaction.target_tx_hash.as_slice())),
                "transaction": {
                    "accessList": [],
                    "chainId": transaction.transaction.chain_id.to_string(),
                    "gasLimit": transaction.transaction.gas_limit.to_string(),
                    "data": format!("0x{}", Self::hex(&transaction.transaction.input)),
                    "maxFeePerGas": transaction.transaction.max_fee_per_gas.to_string(),
                    "maxPriorityFeePerGas": transaction.transaction.max_priority_fee_per_gas.to_string(),
                    "nonce": transaction.transaction.nonce.to_string(),
                    "to": match transaction.transaction.to {
                        TxKind::Call(address) => {
                            JsonValue::String(format!("0x{}", Self::hex(address.as_slice())))
                        }
                        TxKind::Create => JsonValue::Null,
                    },
                    "value": transaction.transaction.value.to_string(),
                    "type": "0x2",
                },
                "unsignedEnvelopeBytes": format!("0x{}", Self::hex(&transaction.unsigned_envelope_bytes)),
                "unsignedEnvelopeHash": format!("0x{}", Self::hex(transaction.unsigned_envelope_hash.as_slice())),
                "validUntilBlock": transaction.valid_until_block.to_string(),
                "victimRawTx": format!("0x{}", Self::hex(&transaction.victim_raw_tx)),
                "victimTransaction": {
                    "accessList": &transaction.victim_transaction.access_list,
                    "chainId": transaction.victim_transaction.chain_id.to_string(),
                    "data": format!("0x{}", Self::hex(&transaction.victim_transaction.input)),
                    "gasLimit": transaction.victim_transaction.gas_limit.to_string(),
                    "maxFeePerGas": transaction.victim_transaction.max_fee_per_gas.to_string(),
                    "maxPriorityFeePerGas": transaction.victim_transaction.max_priority_fee_per_gas.to_string(),
                    "nonce": transaction.victim_transaction.nonce.to_string(),
                    "to": match transaction.victim_transaction.to {
                        TxKind::Call(address) => JsonValue::String(format!("0x{}", Self::hex(address.as_slice()))),
                        TxKind::Create => JsonValue::Null,
                    },
                    "type": "0x2",
                    "value": transaction.victim_transaction.value.to_string(),
                },
            },
            "blockNumber": candidate.block_number.to_string(),
            "parentHash": format!("0x{}", Self::hex(candidate.parent_hash.as_slice())),
            "orderedTransactionCount": candidate.ordered_transaction_count.to_string(),
            "victimAbsentBeforePosition": candidate.victim_absent_before_position,
            "candidateGeneration": candidate.candidate_generation.to_string(),
            "candidateSequence": candidate.candidate_sequence.to_string(),
            "codeWitnessDigest": Self::hex(candidate.code_witness_digest.as_slice()),
            "configDigest": Self::hex(candidate.config_digest.as_slice()),
            "connectionRecordHashAtCapture": Self::hex(&candidate.connection_record_hash_at_capture),
            "connectionSequenceAtCapture": candidate.connection_sequence_at_capture.to_string(),
            "coverageGeneration": candidate.coverage_generation.to_string(),
            "orderedTransactionCutoffPosition": candidate.ordered_transaction_cutoff_position.to_string(),
            "orderedTransactionDigest": Self::hex(candidate.ordered_transaction_digest.as_slice()),
            "economicsEvidenceDigest": Self::hex(candidate.economics_evidence_digest.as_slice()),
            "deploymentIdentities": candidate.deployment_identities.iter().map(|(address, hash)| json!({
                "address": format!("0x{}", Self::hex(address.as_slice())),
                "runtimeHash": format!("0x{}", Self::hex(hash.as_slice())),
            })).collect::<Vec<_>>(),
            "g0CodeIdentityDigest": Self::hex(candidate.g0_code_identity_digest.as_slice()),
            "ownerApprovalReceiptDigest": Self::hex(candidate.owner_approval_receipt_digest.as_slice()),
            "payloadFirstRecordHash": Self::hex(&candidate.payload_first_record_hash),
            "payloadFirstRecordSequence": candidate.payload_first_record_sequence.to_string(),
            "payloadId": format!("0x{}", Self::hex(&candidate.payload_id)),
            "pendingSnapshotSequence": candidate.pending_snapshot_sequence.to_string(),
            "policyDigest": Self::hex(candidate.policy_digest.as_slice()),
            "predecessorIndex": candidate.predecessor_index.to_string(),
            "preparedStateDigest": Self::hex(candidate.prepared_state_digest.as_slice()),
            "preparedRoute": prepared_route,
            "materializedState": {
                "writes": candidate.materialized_state.writes.iter()
                    .map(Self::materialized_write_value)
                    .collect::<Vec<_>>(),
            },
            "preregDigest": Self::hex(candidate.prereg_digest.as_slice()),
            "producerDigest": Self::hex(candidate.producer_digest.as_slice()),
            "producerEpoch": candidate.producer_epoch.to_string(),
            "registryTerminalRecordHash": Self::hex(&candidate.registry_terminal_record_hash),
            "schema": "edge-candidate-detail/v1",
            "selectedPlanDigest": Self::hex(candidate.selected_plan_digest.as_slice()),
            "selectedPlan": {
                "amountIn": candidate.selected_plan.amount_in.to_string(),
                "amountOut": candidate.selected_plan.amount_out.to_string(),
                "grossProfit": candidate.selected_plan.gross_profit.to_string(),
                "hops": selected_hops,
            },
            "slotWitnessDigest": Self::hex(candidate.slot_witness_digest.as_slice()),
            "sourceGeneration": candidate.source_generation.to_string(),
            "stateRoot": Self::hex(candidate.state_root.as_slice()),
            "structuralTerminalHash": Self::hex(&candidate.structural_terminal_hash),
            "victimHash": format!("0x{}", Self::hex(candidate.victim_hash.as_slice())),
            "victimRaw": format!("0x{}", Self::hex(&candidate.victim_raw)),
        })))
    }

    fn stringify_numbers(value: JsonValue) -> JsonValue {
        match value {
            JsonValue::Number(value) => JsonValue::String(value.to_string()),
            JsonValue::Array(values) => {
                JsonValue::Array(values.into_iter().map(Self::stringify_numbers).collect())
            }
            JsonValue::Object(values) => JsonValue::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::stringify_numbers(value)))
                    .collect(),
            ),
            value => value,
        }
    }
    fn camel_case_keys(value: JsonValue) -> JsonValue {
        match value {
            JsonValue::Array(values) => {
                JsonValue::Array(values.into_iter().map(Self::camel_case_keys).collect())
            }
            JsonValue::Object(values) => JsonValue::Object(
                values
                    .into_iter()
                    .map(|(key, value)| {
                        let mut camel = String::with_capacity(key.len());
                        let mut uppercase = false;
                        for character in key.chars() {
                            if character == '_' {
                                uppercase = true;
                            } else if uppercase {
                                camel.extend(character.to_uppercase());
                                uppercase = false;
                            } else {
                                camel.push(character);
                            }
                        }
                        (camel, Self::camel_case_keys(value))
                    })
                    .collect(),
            ),
            value => value,
        }
    }

    fn authority_record_hash(domain: &str, record: &JsonValue) -> io::Result<String> {
        let mut value = record
            .as_object()
            .cloned()
            .ok_or_else(|| io::Error::other("authority record must be object"))?;
        value.remove("recordHash");
        let canonical = Self::canonical_bytes(&JsonValue::Object(value))?;
        let authority_domain = b"base-edge-authority-record-v1\0";
        let mut bytes =
            Vec::with_capacity(authority_domain.len() + domain.len() + canonical.len() + 12);
        for field in [authority_domain.as_slice(), domain.as_bytes(), canonical.as_slice()] {
            let length = u32::try_from(field.len())
                .map_err(|_| io::Error::other("authority hash field exceeds u32"))?;
            bytes.extend_from_slice(&length.to_be_bytes());
            bytes.extend_from_slice(field);
        }
        Ok(Self::sha256_hex(&bytes))
    }

    fn compact_candidate_detail_exclusion(
        record: &JsonValue,
        reason: &'static str,
    ) -> io::Result<JsonValue> {
        let object = record
            .as_object()
            .ok_or_else(|| io::Error::other("candidate detail must be an object"))?;
        let required = |field: &str| {
            object
                .get(field)
                .cloned()
                .ok_or_else(|| io::Error::other("candidate detail exclusion field missing"))
        };
        Ok(json!({
            "candidateGeneration": required("candidateGeneration")?,
            "candidateSequence": required("candidateSequence")?,
            "connectionRecordHashAtCapture": required("connectionRecordHashAtCapture")?,
            "connectionSequenceAtCapture": required("connectionSequenceAtCapture")?,
            "coverageGeneration": required("coverageGeneration")?,
            "payloadFirstRecordHash": required("payloadFirstRecordHash")?,
            "payloadFirstRecordSequence": required("payloadFirstRecordSequence")?,
            "pendingSnapshotSequence": required("pendingSnapshotSequence")?,
            "reason": reason,
            "registryTerminalRecordHash": required("registryTerminalRecordHash")?,
            "schema": "edge-candidate-detail-exclusion/v1",
            "selectedPlanDigest": required("selectedPlanDigest")?,
            "sourceGeneration": required("sourceGeneration")?,
            "structuralTerminalHash": required("structuralTerminalHash")?,
            "victimHash": required("victimHash")?,
        }))
    }

    fn push_rolling_record(
        &mut self,
        ledger_name: &'static str,
        hash_domain: &'static str,
        state: &str,
        mut record: JsonValue,
    ) -> io::Result<Vec<EdgeSealedSegmentV1>> {
        let mut ledger = self
            .ledgers
            .remove(ledger_name)
            .unwrap_or_else(|| EdgeRollingLedgerV1::new(ledger_name, hash_domain));
        if ledger.hash_domain != hash_domain {
            return Err(io::Error::other("ledger hash domain changed"));
        }
        let mut sealed = Vec::new();
        let object = record
            .as_object_mut()
            .ok_or_else(|| io::Error::other("rolling record must be object"))?;
        object.insert(
            "producerEpoch".to_owned(),
            JsonValue::String(self.owner.producer_epoch().to_string()),
        );
        object.insert("sequence".to_owned(), JsonValue::String(ledger.next_sequence.to_string()));
        object.insert("state".to_owned(), JsonValue::String(state.to_owned()));
        object.insert(
            "previousRecordHash".to_owned(),
            JsonValue::String(ledger.previous_record_hash.clone()),
        );
        object.remove("recordHash");
        let mut record_hash = Self::authority_record_hash(hash_domain, &record)?;
        record
            .as_object_mut()
            .ok_or_else(|| io::Error::other("rolling record must remain an object"))?
            .insert("recordHash".to_owned(), JsonValue::String(record_hash.clone()));
        let mut line_len = Self::canonical_line(&record)?.len();
        if line_len + 1024 > EDGE_SEGMENT_MAX_RECORD_BYTES && ledger_name == "candidate" {
            record =
                Self::compact_candidate_detail_exclusion(&record, "CandidateDetailRecordTooLarge")?;
            let object = record
                .as_object_mut()
                .ok_or_else(|| io::Error::other("candidate exclusion must be an object"))?;
            object.insert(
                "producerEpoch".to_owned(),
                JsonValue::String(self.owner.producer_epoch().to_string()),
            );
            object
                .insert("sequence".to_owned(), JsonValue::String(ledger.next_sequence.to_string()));
            object.insert(
                "state".to_owned(),
                JsonValue::String("CandidateDetailExcluded".to_owned()),
            );
            object.insert(
                "previousRecordHash".to_owned(),
                JsonValue::String(ledger.previous_record_hash.clone()),
            );
            let exclusion_hash = Self::authority_record_hash(hash_domain, &record)?;
            record
                .as_object_mut()
                .ok_or_else(|| io::Error::other("candidate exclusion must be an object"))?
                .insert("recordHash".to_owned(), JsonValue::String(exclusion_hash.clone()));
            record_hash = exclusion_hash;
            line_len = Self::canonical_line(&record)?.len();
            self.missing_evidence.insert("CandidateDetailRecordTooLarge");
        }
        if line_len + 1024 > EDGE_SEGMENT_MAX_RECORD_BYTES {
            return Err(io::Error::other("single edge record exceeds segment bound"));
        }
        if !ledger.records.is_empty()
            && (ledger.records.len() == EDGE_SEGMENT_MAX_RECORDS
                || ledger.record_bytes + line_len + 1024 > EDGE_SEGMENT_MAX_RECORD_BYTES)
        {
            sealed.push(self.seal_rolling_ledger(&mut ledger)?);
        }
        if ledger.records.is_empty() {
            ledger.batch_started_at = Some(Instant::now());
        }
        ledger.record_bytes += line_len;
        ledger.records.push(record);
        ledger.previous_record_hash = record_hash;
        ledger.next_sequence = ledger
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("rolling ledger sequence overflow"))?;
        if ledger.records.len() == EDGE_SEGMENT_MAX_RECORDS {
            sealed.push(self.seal_rolling_ledger(&mut ledger)?);
        }
        self.ledgers.insert(ledger_name, ledger);
        Ok(sealed)
    }
    fn rolling_flush_due(&self, ledger_name: &'static str, now: Instant) -> bool {
        self.ledgers.get(ledger_name).and_then(|ledger| ledger.batch_started_at).is_some_and(
            |started| now.saturating_duration_since(started) >= EDGE_SEGMENT_MAX_FLUSH_LATENCY,
        )
    }

    fn flush_rolling_ledger(
        &mut self,
        ledger_name: &'static str,
    ) -> io::Result<Option<EdgeSealedSegmentV1>> {
        let Some(mut ledger) = self.ledgers.remove(ledger_name) else {
            return Ok(None);
        };
        let result = if ledger.records.is_empty() {
            None
        } else {
            Some(self.seal_rolling_ledger(&mut ledger)?)
        };
        self.ledgers.insert(ledger_name, ledger);
        Ok(result)
    }

    fn seal_rolling_ledger(
        &mut self,
        ledger: &mut EdgeRollingLedgerV1,
    ) -> io::Result<EdgeSealedSegmentV1> {
        let first = ledger.records.first().ok_or_else(|| io::Error::other("empty segment"))?;
        let last = ledger.records.last().ok_or_else(|| io::Error::other("empty segment"))?;
        let field = |record: &JsonValue, name: &str| -> io::Result<String> {
            record
                .get(name)
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| io::Error::other(format!("segment record missing {name}")))
        };
        let first_sequence = field(first, "sequence")?;
        let last_sequence = field(last, "sequence")?;
        let first_previous_hash = field(first, "previousRecordHash")?;
        let last_record_hash = field(last, "recordHash")?;
        let mut record_bytes = Vec::with_capacity(ledger.record_bytes);
        let mut member_sequences = Vec::with_capacity(ledger.records.len());
        for record in &ledger.records {
            member_sequences.push(
                field(record, "sequence")?
                    .parse()
                    .map_err(|_| io::Error::other("noncanonical segment sequence"))?,
            );
            record_bytes.extend_from_slice(&Self::canonical_line(record)?);
        }
        let footer_base = json!({
            "firstPreviousRecordHash": first_previous_hash,
            "firstSequence": first_sequence,
            "lastRecordHash": last_record_hash,
            "lastSequence": last_sequence,
            "recordCount": ledger.records.len().to_string(),
            "recordsSha256": Self::sha256_hex(&record_bytes),
            "schemaVersion": "edge-sidecar-segment-footer-v1",
        });
        let mut footer_domain = b"edge-sidecar-segment-footer-v1\0".to_vec();
        footer_domain.extend_from_slice(&Self::canonical_bytes(&footer_base)?);
        let mut footer = footer_base
            .as_object()
            .cloned()
            .ok_or_else(|| io::Error::other("footer must be object"))?;
        let segment_seal_sha256 = Self::sha256_hex(&footer_domain);
        footer
            .insert("segmentSealSha256".to_owned(), JsonValue::String(segment_seal_sha256.clone()));
        let mut bytes = record_bytes;
        bytes.extend_from_slice(&Self::canonical_line(&JsonValue::Object(footer))?);
        if bytes.len() > EDGE_SEGMENT_MAX_RECORD_BYTES {
            return Err(io::Error::other("sealed segment exceeds byte bound"));
        }
        let filename = format!("{}-{:020}.ndjson", ledger.name, ledger.next_segment);
        self.persist_immutable(&filename, &bytes)?;
        let descriptor = EdgeSegmentDescriptorV1 {
            filename: filename.clone(),
            first_sequence,
            last_sequence,
            record_count: ledger.records.len().to_string(),
            segment_seal_sha256,
            file_sha256: Self::sha256_hex(&bytes),
        };
        self.persisted_artifacts.insert(filename, descriptor.file_sha256.clone());
        ledger.descriptors.push(descriptor.clone());
        ledger.records.clear();
        ledger.record_bytes = 0;
        ledger.batch_started_at = None;
        ledger.next_segment = ledger
            .next_segment
            .checked_add(1)
            .ok_or_else(|| io::Error::other("rolling segment sequence overflow"))?;
        Ok(EdgeSealedSegmentV1 { descriptor, member_sequences })
    }

    fn ack_source_segment(
        &mut self,
        ledger: &'static str,
        segment: &EdgeSealedSegmentV1,
    ) -> io::Result<()> {
        let segment_hash = B256::from_str(&format!("0x{}", segment.descriptor.file_sha256))
            .map_err(|_| io::Error::other("source segment SHA malformed"))?;
        for ledger_sequence in &segment.member_sequences {
            let source_sequence = self
                .source_sequence_by_ledger
                .remove(&(ledger, *ledger_sequence))
                .ok_or_else(|| io::Error::other("source durable identity missing"))?;
            if !self.source_durable.insert(source_sequence) {
                return Err(io::Error::other("duplicate source durable identity"));
            }
            if ledger == "connection" {
                self.connection_segment_sha.push((*ledger_sequence, segment_hash));
            }
        }
        while self.source_durable.remove(&self.source_durable_ack_cursor) {
            self.recorder
                .ack_event_durable()
                .map_err(|_| io::Error::other("source durable ACK mismatch"))?;
            self.source_durable_ack_cursor = self
                .source_durable_ack_cursor
                .checked_add(1)
                .ok_or_else(|| io::Error::other("source durable ACK cursor overflow"))?;
        }
        Ok(())
    }
    fn validate_candidate_segment(&mut self, segment: &EdgeSealedSegmentV1) -> io::Result<()> {
        for sequence in &segment.member_sequences {
            if !self.candidate_durable.insert(*sequence) {
                return Err(io::Error::other("duplicate candidate durable identity"));
            }
        }
        self.release_candidate_joins()?;
        Ok(())
    }

    fn release_candidate_joins(&mut self) -> io::Result<bool> {
        let mut progressed = false;
        loop {
            let sequence = self.candidate_validated_count;
            if !self.candidate_durable.contains(&sequence) {
                break;
            }
            if self.candidate_join_exclusions.remove(&sequence) {
                self.candidate_durable.remove(&sequence);
                self.candidate_validated_count = self
                    .candidate_validated_count
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("candidate validated count overflow"))?;
                progressed = true;
                continue;
            }
            let join = self
                .candidate_joins
                .get(&sequence)
                .ok_or_else(|| io::Error::other("candidate compact join missing"))?;
            let Some(connection_record_hash) =
                Self::sequence_hash(&self.connection_records, join.connection_sequence)
            else {
                break;
            };
            if connection_record_hash != join.connection_record_hash {
                return Err(io::Error::other("candidate compact join validation failed"));
            }
            if self
                .connection_segment_sha
                .binary_search_by_key(&join.connection_sequence, |(sequence, _)| *sequence)
                .is_err()
            {
                break;
            }
            let Some(registry_record_hash) =
                Self::sequence_hash(&self.registry_record_sha, join.coverage_generation)
            else {
                break;
            };
            if registry_record_hash != join.registry_terminal_record_hash {
                return Err(io::Error::other("candidate compact join validation failed"));
            }
            if Self::sequence_hash(&self.registry_segment_sha, join.coverage_generation).is_none() {
                break;
            }
            if sequence != join.candidate_sequence {
                return Err(io::Error::other("candidate compact join validation failed"));
            }
            self.candidate_joins
                .remove(&sequence)
                .ok_or_else(|| io::Error::other("candidate compact join release failed"))?;
            self.candidate_durable.remove(&sequence);
            self.candidate_validated_count = self
                .candidate_validated_count
                .checked_add(1)
                .ok_or_else(|| io::Error::other("candidate validated count overflow"))?;
            progressed = true;
        }
        Ok(progressed)
    }

    fn record_registry_segment(
        &mut self,
        ledger: &'static str,
        segment: &EdgeSealedSegmentV1,
    ) -> io::Result<()> {
        let segment_hash = B256::from_str(&format!("0x{}", segment.descriptor.file_sha256))
            .map_err(|_| io::Error::other("registry segment SHA malformed"))?;
        for coverage_sequence in &segment.member_sequences {
            let terminal = self
                .registry_pending
                .get(coverage_sequence)
                .ok_or_else(|| io::Error::other("registry durable identity missing"))?;
            if ledger == "registry-h1" {
                let pending_sequence = terminal.metadata.identity.pending_snapshot_sequence;
                if self
                    .registry_h1_segment_sha
                    .insert(pending_sequence, (*coverage_sequence, segment_hash))
                    .is_some()
                {
                    return Err(io::Error::other("duplicate registry H1 pending sequence"));
                }
            } else if ledger == "registry-h2" {
                Self::append_sequence_hash(
                    &mut self.registry_segment_sha,
                    *coverage_sequence,
                    segment_hash,
                    "registry H2 durable segment sequence mismatch",
                )?;
            } else {
                return Err(io::Error::other("unknown registry ledger"));
            }
        }
        Ok(())
    }

    const fn registry_final_non_authority_send_count(
        registry: &PendingRegistryFinalSummaryV2,
    ) -> u64 {
        registry.unregistered_send_count
    }

    fn registry_terminal_exclusion_counts(
        registry: &PendingRegistryFinalSummaryV2,
    ) -> BTreeMap<&'static str, String> {
        [
            ("terminalRecordAllocationMissing", registry.terminal_record_allocation_missing),
            ("terminalRecordCapacityMissing", registry.terminal_record_capacity_missing),
            ("registrationCapacityMissing", registry.registration_capacity_missing),
        ]
        .into_iter()
        .map(|(reason, count)| (reason, count.to_string()))
        .collect()
    }
    fn ledger_segment_set_sha256(&self, ledger_name: &'static str) -> io::Result<String> {
        let mut descriptors = self
            .ledgers
            .get(ledger_name)
            .map(|ledger| {
                ledger
                    .descriptors
                    .iter()
                    .map(|descriptor| {
                        json!({
                            "fileSha256": descriptor.file_sha256,
                            "filename": descriptor.filename,
                            "firstSequence": descriptor.first_sequence,
                            "lastSequence": descriptor.last_sequence,
                            "recordCount": descriptor.record_count,
                            "segmentSealSha256": descriptor.segment_seal_sha256,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        descriptors
            .sort_by(|left, right| left["filename"].as_str().cmp(&right["filename"].as_str()));
        let mut domain = b"edge-sidecar-segment-set-v1\0".to_vec();
        domain.extend_from_slice(&Self::canonical_bytes(&JsonValue::Array(descriptors))?);
        Ok(Self::sha256_hex(&domain))
    }

    fn rolling_descriptor_manifest(&self) -> io::Result<JsonValue> {
        let mut ledgers = Vec::new();
        for (name, ledger) in &self.ledgers {
            let mut descriptors = ledger
                .descriptors
                .iter()
                .map(|descriptor| {
                    json!({
                        "fileSha256": descriptor.file_sha256,
                        "filename": descriptor.filename,
                        "firstSequence": descriptor.first_sequence,
                        "lastSequence": descriptor.last_sequence,
                        "recordCount": descriptor.record_count,
                        "segmentSealSha256": descriptor.segment_seal_sha256,
                    })
                })
                .collect::<Vec<_>>();
            descriptors
                .sort_by(|left, right| left["filename"].as_str().cmp(&right["filename"].as_str()));
            let descriptors = JsonValue::Array(descriptors);
            let mut set_domain = b"edge-sidecar-segment-set-v1\0".to_vec();
            set_domain.extend_from_slice(&Self::canonical_bytes(&descriptors)?);
            ledgers.push(json!({
                "ledger": name,
                "segmentSetSha256": Self::sha256_hex(&set_domain),
                "segments": descriptors,
            }));
        }
        Ok(json!({
            "ledgers": ledgers,
            "schemaVersion": "edge-segment-descriptor-manifest-v1",
        }))
    }
    fn immutable_file_matches(path: &Path, expected: &[u8]) -> io::Result<bool> {
        let mut file = File::open(path)?;
        let mut offset = 0;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok(offset == expected.len());
            }
            let Some(end) = offset.checked_add(read) else {
                return Ok(false);
            };
            if expected.get(offset..end) != Some(&buffer[..read]) {
                return Ok(false);
            }
            offset = end;
        }
    }

    fn persist_immutable(&self, filename: &str, bytes: &[u8]) -> io::Result<()> {
        Self::validate_canonical_artifact(bytes)?;
        let final_path = self.directory.join(filename);
        if final_path.try_exists()? {
            if !Self::immutable_file_matches(&final_path, bytes)? {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "immutable conflict"));
            }
            return Ok(());
        }
        let open_path = self.directory.join(format!("{filename}.open"));
        let mut file =
            OpenOptions::new().create_new(true).write(true).mode(0o600).open(&open_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if !Self::immutable_file_matches(&open_path, bytes)? {
            return Err(io::Error::other("durable re-read mismatch"));
        }
        match fs::hard_link(&open_path, &final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !Self::immutable_file_matches(&final_path, bytes)? {
                    return Err(io::Error::new(io::ErrorKind::AlreadyExists, "immutable conflict"));
                }
            }
            Err(error) => return Err(error),
        }
        fs::remove_file(open_path)?;
        self.directory_handle.sync_all()
    }

    fn finalize(&mut self) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        let source_final = self
            .recorder
            .verify_source_final()
            .map_err(|_| io::Error::other("source final seal rejected"))?;
        let registry = self.recorder.registry().final_summary();
        let candidate_bounds = self.owner.checked_candidate_bounds();
        let (blink_count, _) = self
            .owner
            .cutoff_cursors()
            .map_err(|_| io::Error::other("Blink cursor unavailable"))?;
        let (
            wire_count,
            source_generation_count,
            payload_first_count,
            connection_count,
            processor_terminal_count,
            authority_wire_terminal_count,
            source_pending,
            poison_count,
            _,
        ) = self.recorder.source_final_counters();
        let coverage_len = usize::try_from(registry.coverage_count)
            .map_err(|_| io::Error::other("coverage count does not fit usize"))?;
        let connection_len = usize::try_from(connection_count)
            .map_err(|_| io::Error::other("connection count does not fit usize"))?;
        let coverage_ledger_conserved = self.ledgers.get("coverage").map_or(
            authority_wire_terminal_count == 0
                && registry.coverage_count == 0
                && source_final.last_coverage_sequence == 0,
            |ledger| {
                ledger.records.is_empty()
                    && !ledger.descriptors.is_empty()
                    && ledger.next_sequence.checked_sub(1)
                        == Some(source_final.last_coverage_sequence)
            },
        );
        if !coverage_ledger_conserved
            || registry.poisoned
            || poison_count != 0
            || source_pending != 0
            || registry.coverage_queue_pending_ack != 0
            || registry.primary_pending != 0
            || registry.secondary_pending != 0
            || registry.published_pending != 0
            || registry.unregistered_send_inflight != 0
            || registry.registration_without_sequence_send_inflight != 0
            || registry.terminal_records != coverage_len
            || registry.durability_acked != coverage_len
            || self.registry_h1_segment_sha.len() != coverage_len
            || self.registry_segment_sha.len() != coverage_len
            || self.registry_record_sha.len() != coverage_len
            || self.connection_records.len() != connection_len
            || self.connection_segment_sha.len() != connection_len
            || registry.coverage_count.checked_sub(1) != registry.last_coverage_sequence
            || (candidate_bounds.count != 0
                && candidate_bounds.last_sequence != Some(source_final.last_candidate_sequence))
            || (blink_count != 0
                && blink_count.checked_sub(1) != Some(source_final.last_admitted_blink_generation))
        {
            return Err(io::Error::other(format!(
                "final conservation mismatch: coverageLedger={coverage_ledger_conserved} registryPoisoned={} poison={poison_count} sourcePending={source_pending} coverageAck={} primary={} secondary={} published={} nonAuthorityInflight={} sequenceLessRegistrationInflight={} terminals={}/{} durability={}/{} h1={} h2={} hashes={} connections={}/{} connectionSegments={}/{} registryLast={:?}",
                registry.poisoned,
                registry.coverage_queue_pending_ack,
                registry.primary_pending,
                registry.secondary_pending,
                registry.published_pending,
                registry.unregistered_send_inflight,
                registry.registration_without_sequence_send_inflight,
                registry.terminal_records,
                coverage_len,
                registry.durability_acked,
                coverage_len,
                self.registry_h1_segment_sha.len(),
                self.registry_segment_sha.len(),
                self.registry_record_sha.len(),
                self.connection_records.len(),
                connection_len,
                self.connection_segment_sha.len(),
                connection_len,
                registry.last_coverage_sequence,
            )));
        }
        if self.connection_segment_sha.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(io::Error::other("connection durable segment order mismatch"));
        }

        let connection_segment_set_sha256 = self.ledger_segment_set_sha256("connection")?;
        let connection_final_record_hash = self.ledgers.get("connection").map_or_else(
            || EDGE_ZERO_HASH.to_owned(),
            |ledger| ledger.previous_record_hash.clone(),
        );
        let connection_final_value = json!({
            "connectionCount": connection_count.to_string(),
            "connectionEmpty": connection_count == 0,
            "cutoffRecordHash": Self::hex(source_final.record_hash.as_slice()),
            "finalRecordHash": connection_final_record_hash,
            "lastConnectionSequenceInclusive": connection_count.checked_sub(1).map(|value| JsonValue::String(value.to_string())),
            "pending": "0",
            "schemaVersion": "edge-connection-final-v1",
            "segmentSetSha256": connection_segment_set_sha256,
        });
        if let Some(segment) = self.flush_rolling_ledger("candidate")? {
            self.validate_candidate_segment(&segment)?;
        }
        let blink_final =
            self.owner.final_record().map_err(|_| io::Error::other("Blink final seal rejected"))?;
        let candidate_ledger = self.ledgers.get("candidate");
        let persisted_candidate_count = candidate_ledger.map_or(0, |ledger| ledger.next_sequence);
        let candidate_ledger_sealed = candidate_ledger.is_none_or(|ledger| {
            ledger.records.is_empty()
                && (ledger.next_sequence == 0 || !ledger.descriptors.is_empty())
        });
        if !candidate_ledger_sealed
            || persisted_candidate_count != candidate_bounds.count
            || self.candidate_validated_count != candidate_bounds.count
            || !self.candidate_joins.is_empty()
            || !self.candidate_join_exclusions.is_empty()
            || !self.candidate_durable.is_empty()
        {
            return Err(io::Error::other("candidate final conservation mismatch"));
        }
        self.persist_named_artifact("connection-final-v1.json", &connection_final_value)?;
        drop(connection_final_value);
        drop(std::mem::take(&mut self.connection_records));
        drop(std::mem::take(&mut self.connection_segment_sha));

        let source_value = json!({
            "cutoffClockObservationOrdinal": source_final.cutoff_clock_observation_ordinal.to_string(),
            "lastAdmittedBlinkGeneration": source_final.last_admitted_blink_generation.to_string(),
            "lastAdmittedSourceGeneration": source_final.last_admitted_source_generation.to_string(),
            "lastAdmittedWireOrdinal": source_final.last_admitted_wire_ordinal.to_string(),
            "lastCandidateSequence": source_final.last_candidate_sequence.to_string(),
            "lastCoverageSequence": source_final.last_coverage_sequence.to_string(),
            "lastPendingSnapshotSequence": source_final.last_pending_snapshot_sequence.to_string(),
            "latchMonoNs": source_final.latch_mono_ns.to_string(),
            "producerEpoch": source_final.producer_epoch.to_string(),
            "recordHash": Self::hex(source_final.record_hash.as_slice()),
        });
        let mut blink_value = Self::camel_case_keys(Self::stringify_numbers(
            serde_json::to_value(blink_final).map_err(io::Error::other)?,
        ));
        blink_value
            .as_object_mut()
            .ok_or_else(|| io::Error::other("Blink final must serialize as an object"))?
            .insert(
                "schemaVersion".to_owned(),
                JsonValue::String("edge-blink-final-v1".to_owned()),
            );
        if blink_value.get("cutoff") != Some(&source_value) {
            return Err(io::Error::other("source and Blink cutoff finals disagree"));
        }
        self.persist_named_artifact("source-final-v1.json", &source_value)?;
        self.persist_named_artifact("blink-final-v1.json", &blink_value)?;
        drop(source_value);
        drop(blink_value);

        let non_authority_send_count = Self::registry_final_non_authority_send_count(&registry);
        let counters = registry.counters;
        let sets = registry.set_cardinalities;
        let registry_h1_segment_set_sha256 = self.ledger_segment_set_sha256("registry-h1")?;
        let registry_h2_segment_set_sha256 = self.ledger_segment_set_sha256("registry-h2")?;
        let mut registry_value = Self::stringify_numbers(json!({
            "counters": {
                "advancedWithSnapshot": counters.advanced_with_snapshot,
                "cliCancelledAttributed": counters.cli_cancelled_attributed,
                "cliClosedAttributed": counters.cli_closed_attributed,
                "cliLaggedAttributed": counters.cli_lagged_attributed,
                "cliReceivedLookupSucceeded": counters.cli_received_lookup_succeeded,
                "cliRegistryLookupFailed": counters.cli_registry_lookup_failed,
                "registrationFailed": counters.registration_failed,
                "registrationSucceeded": counters.registration_succeeded,
                "sendNoReceivers": counters.send_no_receivers,
                "sendPublished": counters.send_published,
            },
            "coverageCount": registry.coverage_count,
            "coverageEmpty": registry.coverage_count == 0,
            "durabilityAckedCount": registry.durability_acked,
            "h1DurableRecordCount": self.registry_h1_segment_sha.len(),
            "h1SegmentSetSha256": registry_h1_segment_set_sha256,
            "h2DurableRecordCount": self.registry_segment_sha.len(),
            "h2RecordHashCount": self.registry_record_sha.len(),
            "h2SegmentSetSha256": registry_h2_segment_set_sha256,
            "lastCoverageSequenceInclusive": registry.last_coverage_sequence.map(|value| JsonValue::String(value.to_string())),
            "pending": "0",
            "pendingBreakdown": {
                "coverageAck": registry.coverage_queue_pending_ack,
                "delivery": sets.pending_delivery_final,
                "primary": registry.primary_pending,
                "published": registry.published_pending,
                "secondary": registry.secondary_pending,
                "unregisteredSendInflight": registry.unregistered_send_inflight,
                "sequenceLessRegistrationSendInflight": registry.registration_without_sequence_send_inflight,
            },
            "poisoned": registry.poisoned,
            "nonAuthoritySendCount": non_authority_send_count,
            "schemaVersion": "edge-registry-final-v1",
            "setCardinalities": {
                "advancedWithSnapshot": sets.advanced_with_snapshot,
                "cliCancelledAttributed": sets.cli_cancelled_attributed,
                "cliClosedAttributed": sets.cli_closed_attributed,
                "cliLaggedAttributed": sets.cli_lagged_attributed,
                "cliOkReceived": sets.cli_ok_received,
                "cliReceivedLookupSucceeded": sets.cli_received_lookup_succeeded,
                "cliRegistryLookupFailed": sets.cli_registry_lookup_failed,
                "failedRegCliCancelledAttributed": sets.failed_reg_cli_cancelled_attributed,
                "failedRegCliClosedAttributed": sets.failed_reg_cli_closed_attributed,
                "failedRegCliLaggedAttributed": sets.failed_reg_cli_lagged_attributed,
                "failedRegCliRegistryLookupFailed": sets.failed_reg_cli_registry_lookup_failed,
                "failedRegCliReceivedLookupSucceeded": sets.failed_reg_cli_received_lookup_succeeded,
                "failedRegPendingFinal": sets.failed_reg_pending_final,
                "failedRegistrationNoReceivers": sets.failed_registration_no_receivers,
                "failedRegistrationPublished": sets.failed_registration_published,
                "pendingDeliveryFinal": sets.pending_delivery_final,
                "registeredNoReceivers": sets.registered_no_receivers,
                "registeredPublished": sets.registered_published,
                "registrationFailed": sets.registration_failed,
                "registrationFailedNoReceivers": sets.registration_failed_no_receivers,
                "registrationSucceeded": sets.registration_succeeded,
                "sendNoReceivers": sets.send_no_receivers,
                "sendPublished": sets.send_published,
                "snapshotRecordsInstalled": sets.snapshot_records_installed,
            },
            "terminalRecordCount": registry.terminal_records,
        }));
        if let Some(through) = registry.last_coverage_sequence {
            let object = registry_value
                .as_object_mut()
                .ok_or_else(|| io::Error::other("registry final must be an object"))?;
            object.insert("expectedThrough".to_owned(), JsonValue::String(through.to_string()));
            object.insert("terminalizedThrough".to_owned(), JsonValue::String(through.to_string()));
        }
        self.persist_named_artifact("registry-final-v1.json", &registry_value)?;
        drop(registry_value);
        drop(std::mem::take(&mut self.registry_h1_segment_sha));
        drop(std::mem::take(&mut self.registry_segment_sha));
        drop(std::mem::take(&mut self.registry_record_sha));

        let source_segment_hashes = self
            .persisted_artifacts
            .iter()
            .filter(|(filename, _)| {
                filename.ends_with(".ndjson")
                    && !filename.starts_with("blink-")
                    && !filename.starts_with("candidate")
                    && !filename.starts_with("registry-")
            })
            .map(|(filename, sha256)| json!({"filename": filename, "sha256": sha256}))
            .collect::<Vec<_>>();
        let coverage_segment_set_sha256 = self.ledger_segment_set_sha256("coverage")?;
        let source_coverage_count =
            self.ledgers.get("coverage").map_or(0, |ledger| ledger.next_sequence);
        let last_source_coverage_sequence = source_coverage_count.checked_sub(1);
        let source_coverage_value = json!({
            "coverageCount": source_coverage_count.to_string(),
            "coverageEmpty": source_coverage_count == 0,
            "authorityWireTerminalCount": authority_wire_terminal_count.to_string(),
            "finalRecordHash": self.ledgers.get("coverage").map_or_else(
                || EDGE_ZERO_HASH.to_owned(),
                |ledger| ledger.previous_record_hash.clone(),
            ),
            "lastPayloadFirstSequenceInclusive": payload_first_count.checked_sub(1).map(|value| JsonValue::String(value.to_string())),
            "lastSequence": source_final.last_coverage_sequence.to_string(),
            "lastSequenceInclusive": last_source_coverage_sequence.map(|value| JsonValue::String(value.to_string())),
            "lastSourceGenerationInclusive": source_generation_count.checked_sub(1).map(|value| JsonValue::String(value.to_string())),
            "lastWireOrdinalInclusive": wire_count.checked_sub(1).map(|value| JsonValue::String(value.to_string())),
            "ledger": "coverage",
            "payloadFirstCount": payload_first_count.to_string(),
            "pending": source_pending.to_string(),
            "processorTerminalCount": processor_terminal_count.to_string(),
            "producerEpoch": self.owner.producer_epoch().to_string(),
            "schemaVersion": "edge-source-coverage-final-v1",
            "segmentSetSha256": coverage_segment_set_sha256,
            "sourceSegmentHashes": source_segment_hashes,
            "sourceGenerationCount": source_generation_count.to_string(),
            "wireCount": wire_count.to_string(),
        });
        self.persist_named_artifact("source-coverage-final-v1.json", &source_coverage_value)?;
        drop(source_coverage_value);

        let source_missing_evidence_snapshot = self.recorder.missing_evidence_snapshot();
        let mut source_missing_evidence_numeric = source_missing_evidence_snapshot
            .breakdown
            .iter()
            .map(|entry| (entry.artifact_key, entry.count))
            .collect::<BTreeMap<_, _>>();
        let mut source_missing_evidence_total = source_missing_evidence_snapshot.total;
        let source_breakdown_total = source_missing_evidence_snapshot
            .breakdown
            .iter()
            .try_fold(0_u64, |total, entry| total.checked_add(entry.count));
        let mut coordinator_missing_evidence_numeric = source_missing_evidence_snapshot
            .coordinator_breakdown
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let coordinator_missing_evidence_total = coordinator_missing_evidence_numeric
            .values()
            .try_fold(0_u64, |total, count| total.checked_add(*count));
        let source_coordinator_failure_count =
            source_missing_evidence_numeric.get("coordinatorFailure").copied().unwrap_or(0);
        let registry_source_terminal_mismatch = registry.terminal_record_capacity_missing
            != source_missing_evidence_numeric
                .get("terminalRecordCapacityExcluded")
                .copied()
                .unwrap_or(0)
            || registry.terminal_record_allocation_missing
                != source_missing_evidence_numeric
                    .get("terminalRecordAllocationExcluded")
                    .copied()
                    .unwrap_or(0);
        let mut snapshot_mismatches = Vec::new();
        if source_breakdown_total != Some(source_missing_evidence_total) {
            snapshot_mismatches.push("SourceMissingEvidenceSnapshotMismatch");
        }
        if coordinator_missing_evidence_total != Some(source_coordinator_failure_count) {
            snapshot_mismatches.push("CoordinatorMissingEvidenceSnapshotMismatch");
        }
        if registry_source_terminal_mismatch {
            snapshot_mismatches.push("RegistrySourceTerminalExclusionMismatch");
        }
        for entry in &source_missing_evidence_snapshot.breakdown {
            if entry.count != 0 {
                self.missing_evidence.insert(entry.reason_name);
            }
        }
        for reason in snapshot_mismatches {
            self.missing_evidence.insert(reason);
            let count = coordinator_missing_evidence_numeric.entry(reason).or_default();
            *count = count.saturating_add(1);
            let coordinator_failure =
                source_missing_evidence_numeric.entry("coordinatorFailure").or_default();
            *coordinator_failure = coordinator_failure.saturating_add(1);
            source_missing_evidence_total = source_missing_evidence_total.saturating_add(1);
        }
        for (reason, count) in &coordinator_missing_evidence_numeric {
            if *count != 0 {
                self.missing_evidence.insert(reason);
            }
        }
        let coordinator_failure_count =
            source_missing_evidence_numeric.get("coordinatorFailure").copied().unwrap_or(0);
        let missing_evidence_count = source_missing_evidence_total;
        let source_missing_evidence_counts = source_missing_evidence_numeric
            .into_iter()
            .map(|(reason, count)| (reason, count.to_string()))
            .collect::<BTreeMap<_, _>>();
        let coordinator_missing_evidence_counts = coordinator_missing_evidence_numeric
            .into_iter()
            .map(|(reason, count)| (reason, count.to_string()))
            .collect::<BTreeMap<_, _>>();
        let registry_terminal_exclusion_count = registry
            .terminal_record_capacity_missing
            .checked_add(registry.terminal_record_allocation_missing)
            .and_then(|count| count.checked_add(registry.registration_capacity_missing))
            .ok_or_else(|| io::Error::other("registry terminal exclusion total overflow"))?;
        let registry_terminal_exclusion_counts =
            Self::registry_terminal_exclusion_counts(&registry);
        for (reason, count) in [
            ("TerminalRecordAllocationMissing", registry.terminal_record_allocation_missing),
            ("TerminalRecordCapacityMissing", registry.terminal_record_capacity_missing),
            ("RegistrationCapacityMissing", registry.registration_capacity_missing),
        ] {
            if count != 0 {
                self.missing_evidence.insert(reason);
            }
        }
        let candidate_drops = self.owner.candidate_pre_enqueue_drop_counters();
        let candidate_drop_counts = json!({
            "cancelledAfterDraft": candidate_drops.cancelled_after_draft.to_string(),
            "candidateQueueClosed": candidate_drops.candidate_queue_closed.to_string(),
            "cutoffDrainDeadline": candidate_drops.cutoff_drain_deadline.to_string(),
            "candidateQueueFull": candidate_drops.candidate_queue_full.to_string(),
            "evidenceMismatch": candidate_drops.evidence_mismatch.to_string(),
            "failedAfterDraft": candidate_drops.failed_after_draft.to_string(),
            "measurementDerivationRejected": candidate_drops.measurement_derivation_rejected.to_string(),
            "missingRequiredEvidence": candidate_drops.missing_required_evidence.to_string(),
            "staleAfterDraft": candidate_drops.stale_after_draft.to_string(),
        });
        let blink_reject_queue_loss = self.owner.blink_reject_queue_loss_counters();
        let blink_reject_queue_loss_counts = json!({
            "queueClosed": blink_reject_queue_loss.queue_closed.to_string(),
            "queueFull": blink_reject_queue_loss.queue_full.to_string(),
        });
        for (reason, count) in [
            ("BlinkRejectQueueClosed", blink_reject_queue_loss.queue_closed),
            ("BlinkRejectQueueFull", blink_reject_queue_loss.queue_full),
        ] {
            if count != 0 {
                self.missing_evidence.insert(reason);
            }
        }
        let candidate_drop_queue_loss = self.owner.candidate_drop_queue_loss_counters();
        let candidate_drop_queue_loss_counts = json!({
            "queueClosed": candidate_drop_queue_loss.queue_closed.to_string(),
            "queueFull": candidate_drop_queue_loss.queue_full.to_string(),
        });
        for (reason, count) in [
            ("CandidateDropQueueClosed", candidate_drop_queue_loss.queue_closed),
            ("CandidateDropQueueFull", candidate_drop_queue_loss.queue_full),
        ] {
            if count != 0 {
                self.missing_evidence.insert(reason);
            }
        }
        let health_value = json!({
            "coordinatorFailureCount": coordinator_failure_count.to_string(),
            "coordinatorMissingEvidenceCounts": coordinator_missing_evidence_counts.clone(),
            "candidatePreEnqueueDropCounts": candidate_drop_counts,
            "candidateDropQueueLossCounts": candidate_drop_queue_loss_counts,
            "blinkRejectQueueLossCounts": blink_reject_queue_loss_counts,
            "missingEvidenceCount": missing_evidence_count.to_string(),
            "pending": "0",
            "poisonCount": poison_count.to_string(),
            "poisoned": poison_count != 0 || registry.poisoned,
            "producerEpoch": self.owner.producer_epoch().to_string(),
            "registryTerminalExclusionCount": registry_terminal_exclusion_count.to_string(),
            "registryTerminalExclusionCounts": registry_terminal_exclusion_counts.clone(),
            "schemaVersion": "edge-producer-health-final-v1",
            "sourceMissingEvidenceCounts": source_missing_evidence_counts.clone(),
        });
        self.persist_named_artifact("producer-health-final-v1.json", &health_value)?;
        drop(health_value);
        let clock_observations = JsonValue::Array(std::mem::take(&mut self.clock_observations));
        self.persist_named_artifact("clock-observations-v1.json", &clock_observations)?;
        drop(clock_observations);

        let candidate_segment_hashes = self
            .persisted_artifacts
            .iter()
            .filter(|(filename, _)| {
                filename
                    .strip_prefix("candidate-")
                    .and_then(|value| value.strip_suffix(".ndjson"))
                    .is_some_and(|sequence| sequence.bytes().all(|byte| byte.is_ascii_digit()))
            })
            .map(|(filename, sha256)| json!({"filename": filename, "sha256": sha256}))
            .collect::<Vec<_>>();
        let candidate_segment_set_sha256 = self.ledger_segment_set_sha256("candidate")?;
        let candidate_value = json!({
            "candidateCount": candidate_bounds.count.to_string(),
            "candidateEmpty": candidate_bounds.count == 0,
            "candidatePreEnqueueDropCounts": candidate_drop_counts,
            "candidateReconcileVeto": "CandidateFreezeV5InputUnavailable",
            "candidateSegmentHashes": candidate_segment_hashes,
            "finalRecordHash": self.ledgers.get("candidate").map_or_else(
                || EDGE_ZERO_HASH.to_owned(),
                |ledger| ledger.previous_record_hash.clone(),
            ),
            "lastCandidateSequenceInclusive": candidate_bounds.last_sequence.map(|value| JsonValue::String(value.to_string())),
            "lastSequence": candidate_bounds.last_sequence.unwrap_or(0).to_string(),
            "ledger": "candidate-detail",
            "pending": "0",
            "producerEpoch": self.owner.producer_epoch().to_string(),
            "schemaVersion": "edge-candidate-detail-final-v1",
            "segmentSetSha256": candidate_segment_set_sha256,
            "strictCandidateSourceAvailable": false,
        });
        self.persist_named_artifact("candidate-detail-final-v1.json", &candidate_value)?;
        drop(candidate_value);

        let provenance = &self.provenance;
        let provenance_value = json!({
            "aerodromeAdapter": provenance.aerodrome_adapter.to_string(),
            "aerodromeAdapterRuntimeHash": provenance.aerodrome_adapter_runtime_hash.to_string(),
            "blinkCandidateCapacity": provenance.blink_candidate_capacity.to_string(),
            "blinkRecordCapacity": provenance.blink_record_capacity.to_string(),
            "configDigest": provenance.config_digest.to_string(),
            "executorRuntimeHash": provenance.executor_runtime_hash.to_string(),
            "flashActiveCapacity": provenance.flash_active_capacity.to_string(),
            "flashEventCapacity": provenance.flash_event_capacity.to_string(),
            "flashRegistryCapacity": provenance.flash_registry_capacity.to_string(),
            "g0CodeIdentityDigest": provenance.g0_code_identity_digest.to_string(),
            "measurementSender": provenance.measurement_sender.to_string(),
            "measurementTxSourceSha256": provenance.measurement_tx_source_sha256.to_string(),
            "outputRootOwnerPrivate": true,
            "outputRootPinnedDescriptor": true,
            "ownerApprovalReceiptDigest": provenance.owner_approval_receipt_digest.to_string(),
            "policyDigest": provenance.policy_digest.to_string(),
            "preregDigest": provenance.prereg_digest.to_string(),
            "producerDigest": provenance.producer_digest.to_string(),
            "producerEpoch": provenance.producer_epoch.get().to_string(),
            "rawRejectInventorySha256": provenance.raw_reject_inventory_sha256.to_string(),
            "rawRejectSourceSha256": provenance.raw_reject_source_sha256.to_string(),
            "rejectSchemaDigest": provenance.reject_schema_digest.to_string(),
            "schemaVersion": "edge-producer-provenance-final-v1",
            "v2Adapter": provenance.v2_adapter.to_string(),
            "v2AdapterRuntimeHash": provenance.v2_adapter_runtime_hash.to_string(),
            "v3Adapter": provenance.v3_adapter.to_string(),
            "v3AdapterRuntimeHash": provenance.v3_adapter_runtime_hash.to_string(),
        });
        self.persist_named_artifact("producer-provenance-final-v1.json", &provenance_value)?;
        drop(provenance_value);
        let descriptor_manifest = self.rolling_descriptor_manifest()?;
        self.persist_named_artifact("segment-descriptors-v1.json", &descriptor_manifest)?;
        drop(descriptor_manifest);
        self.missing_evidence.insert("CandidateFreezeV5InputUnavailable");
        let missing_evidence: Vec<_> = self.missing_evidence.iter().copied().collect();
        let missing_value = json!({
            "candidateReconcileVeto": "CandidateFreezeV5InputUnavailable",
            "checkpointPublished": false,
            "coordinatorMissingEvidenceCounts": coordinator_missing_evidence_counts.clone(),
            "candidatePreEnqueueDropCounts": candidate_drop_counts,
            "candidateDropQueueLossCounts": candidate_drop_queue_loss_counts,
            "blinkRejectQueueLossCounts": blink_reject_queue_loss_counts,
            "missingEvidence": missing_evidence,
            "missingEvidenceCount": missing_evidence_count.to_string(),
            "producerEpoch": self.owner.producer_epoch().to_string(),
            "registryTerminalExclusionCount": registry_terminal_exclusion_count.to_string(),
            "registryTerminalExclusionCounts": registry_terminal_exclusion_counts,
            "schemaVersion": "edge-missing-evidence-final-v1",
            "sourceMissingEvidenceCounts": source_missing_evidence_counts,
        });
        self.persist_named_artifact("missing-evidence-final.json", &missing_value)?;
        drop(missing_value);

        let artifacts = self
            .persisted_artifacts
            .iter()
            .map(|(filename, sha256)| {
                json!({
                    "filename": filename,
                    "sha256": sha256,
                })
            })
            .collect::<Vec<_>>();
        let manifest = json!({
            "artifacts": artifacts,
            "blinkCount": blink_count.to_string(),
            "blinkEmpty": blink_count == 0,
            "candidateCount": candidate_bounds.count.to_string(),
            "candidateEmpty": candidate_bounds.count == 0,
            "candidateReconcileVeto": "CandidateFreezeV5InputUnavailable",
            "candidatePreEnqueueDropCounts": candidate_drop_counts,
            "candidateDropQueueLossCounts": candidate_drop_queue_loss_counts,
            "blinkRejectQueueLossCounts": blink_reject_queue_loss_counts,
            "coordinatorMissingEvidenceCounts": coordinator_missing_evidence_counts,
            "checkpointPublished": false,
            "coverageCount": registry.coverage_count.to_string(),
            "coverageEmpty": registry.coverage_count == 0,
            "lastAdmittedBlinkGenerationInclusive": blink_count.checked_sub(1).map(|value| JsonValue::String(value.to_string())),
            "lastCandidateSequenceInclusive": candidate_bounds.last_sequence.map(|value| JsonValue::String(value.to_string())),
            "lastCoverageSequenceInclusive": registry.last_coverage_sequence.map(|value| JsonValue::String(value.to_string())),
            "missingEvidence": missing_evidence,
            "producerEpoch": self.owner.producer_epoch().to_string(),
            "schemaVersion": "edge-producer-manifest-v1",
        });
        self.persist_named_artifact("producer-manifest-v1.json", &manifest)?;
        drop(manifest);
        let indexed_artifacts = self
            .persisted_artifacts
            .iter()
            .map(|(filename, sha256)| {
                json!({
                    "filename": filename,
                    "sha256": sha256,
                })
            })
            .collect::<Vec<_>>();
        self.persist_named_artifact(
            "artifact-index-v1.json",
            &json!({
                "artifacts": indexed_artifacts,
                "producerEpoch": self.owner.producer_epoch().to_string(),
                "schemaVersion": "edge-production-artifact-index-v1",
            }),
        )?;
        self.finalized = true;
        Ok(())
    }

    fn persist_named_artifact(&mut self, filename: &str, value: &JsonValue) -> io::Result<()> {
        let bytes = Self::canonical_line(value)?;
        self.persist_immutable(filename, &bytes)?;
        self.persisted_artifacts.insert(filename.to_owned(), Self::sha256_hex(&bytes));
        Ok(())
    }
    fn validate_canonical_artifact(bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || !bytes.ends_with(b"\n") {
            return Err(io::Error::other("canonical artifact trailing bytes"));
        }
        let mut start = 0;
        for end in
            bytes.iter().enumerate().filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
        {
            if end == start {
                return Err(io::Error::other("canonical artifact empty line"));
            }
            let value: JsonValue =
                serde_json::from_slice(&bytes[start..end]).map_err(io::Error::other)?;
            if Self::canonical_line(&value)?.as_slice() != &bytes[start..=end] {
                return Err(io::Error::other("canonical artifact replay drift"));
            }
            start = end + 1;
        }
        if start != bytes.len() {
            return Err(io::Error::other("canonical artifact unterminated line"));
        }
        Ok(())
    }

    fn canonical_line(value: &JsonValue) -> io::Result<Vec<u8>> {
        let mut bytes = Self::canonical_bytes(value)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn canonical_bytes(value: &JsonValue) -> io::Result<Vec<u8>> {
        fn write_value(value: &JsonValue, output: &mut String) -> io::Result<()> {
            match value {
                JsonValue::Null => output.push_str("null"),
                JsonValue::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
                JsonValue::String(value) => {
                    output.push_str(&serde_json::to_string(value).map_err(io::Error::other)?)
                }
                JsonValue::Array(values) => {
                    output.push('[');
                    for (index, value) in values.iter().enumerate() {
                        if index != 0 {
                            output.push(',');
                        }
                        write_value(value, output)?;
                    }
                    output.push(']');
                }
                JsonValue::Object(values) => {
                    output.push('{');
                    let mut entries: Vec<_> = values.iter().collect();
                    entries
                        .sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    for (index, (key, value)) in entries.into_iter().enumerate() {
                        if index != 0 {
                            output.push(',');
                        }
                        output.push_str(&serde_json::to_string(key).map_err(io::Error::other)?);
                        output.push(':');
                        write_value(value, output)?;
                    }
                    output.push('}');
                }
                JsonValue::Number(_) => {
                    return Err(io::Error::other("numeric JSON authority value is forbidden"));
                }
            }
            Ok(())
        }
        let mut output = String::new();
        write_value(value, &mut output)?;
        Ok(output.into_bytes())
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Self::hex(&EdgeMeasurementDurabilityV1::sha256(bytes))
    }

    fn hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

// B5-1a dormant-preparation tier: a private default-off child compiled only under
// `b5-dormant-presign`, never registered or re-exported at the crate root.
#[cfg(feature = "b5-dormant-presign")]
mod b5_dormant;

#[derive(Debug)]
struct PendingSnapshotViewAdapter {
    pending: Arc<PendingBlocks>,
}

impl PendingSnapshotView for PendingSnapshotViewAdapter {
    fn parent_hash(&self) -> B256 {
        self.pending.parent_hash()
    }

    fn latest_block_number(&self) -> u64 {
        self.pending.latest_block_number()
    }

    fn canonical_block_number(&self) -> u64 {
        match self.pending.canonical_block_number() {
            BlockNumberOrTag::Number(number) => number,
            _ => 0,
        }
    }

    fn latest_flashblock_index(&self) -> u64 {
        self.pending.latest_flashblock_index()
    }

    fn latest_header(&self) -> Sealed<Header> {
        self.pending.latest_header()
    }

    fn latest_block_transaction_count(&self) -> usize {
        self.pending.latest_block_transaction_count()
    }

    fn has_transaction_hash(&self, transaction_hash: B256) -> bool {
        self.pending.has_transaction_hash(&transaction_hash)
    }

    fn transaction_position(&self, block_number: u64, transaction_hash: B256) -> Option<usize> {
        self.pending.transaction_position(block_number, &transaction_hash)
    }

    fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut visited = 0u32;
        for flashblock in self.pending.latest_block_flashblocks_iter() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(flashblock.payload_id, flashblock.index)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }

    fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut transactions = self.pending.get_transactions_for_block(block_number).skip(start);
        let mut visited = 0u32;
        for position in start..start.saturating_add(limit) {
            let Some(transaction) = transactions.next() else {
                return Ok(VisitSummary { visited, complete: true });
            };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(position, transaction.inner.inner.inner())? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: transactions.next().is_none() })
    }

    fn pending_account_nonce(
        &self,
        address: Address,
    ) -> Result<Option<PendingAccountNonce>, PortError> {
        let bundle = self.pending.get_bundle_state();
        let Some(account) = bundle.account(&address) else {
            return Ok(None);
        };
        let original_nonce = account.original_info.as_ref().map_or(0, |info| info.nonce);
        let current_nonce = account.account_info().ok_or(PortError::Incoherent)?.nonce;
        PendingAccountNonce::checked(original_nonce, current_nonce).map(Some)
    }

    fn visit_bundle(&self, visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError> {
        let bundle = self.pending.get_bundle_state();
        let mut visited = 0u32;
        let mut code_hashes = BTreeSet::new();
        for (address, account) in bundle.state() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_account(*address, account)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
            if let Some(info) = account.account_info()
                && !info.code_hash.is_zero()
            {
                code_hashes.insert(info.code_hash);
            }
        }
        for code_hash in code_hashes {
            let Some(bytecode) = bundle.bytecode(&code_hash) else { continue };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_contract(code_hash, &bytecode)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }
}

#[derive(Debug)]
struct PendingSnapshotRecord {
    view: Arc<dyn PendingSnapshotView + Send + Sync>,
    pending: Arc<PendingBlocks>,
    received_at: Instant,
    #[cfg(feature = "edge-measurement")]
    edge_evidence: RwLock<Option<EdgeSnapshotEvidenceV1>>,
}

#[derive(Debug)]
pub(crate) struct CliTraderSnapshotPort<Provider> {
    flashblocks: Arc<FlashblocksState>,
    provider: Provider,
    current_record: RwLock<Option<Arc<PendingSnapshotRecord>>>,
}

impl<Provider> CliTraderSnapshotPort<Provider> {
    pub(crate) const fn new(flashblocks: Arc<FlashblocksState>, provider: Provider) -> Self {
        Self { flashblocks, provider, current_record: RwLock::new(None) }
    }

    pub(crate) fn record_pending_snapshot(
        &self,
        pending: Arc<PendingBlocks>,
        received_at: Instant,
    ) {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(PendingSnapshotViewAdapter { pending: Arc::clone(&pending) });
        let record = Arc::new(PendingSnapshotRecord {
            view,
            pending,
            received_at,
            #[cfg(feature = "edge-measurement")]
            edge_evidence: RwLock::new(None),
        });
        *self.current_record.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(record);
    }

    fn current_record(&self) -> Option<Arc<PendingSnapshotRecord>> {
        self.current_record.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
    #[cfg(feature = "edge-measurement")]
    fn attach_edge_snapshot_evidence(
        &self,
        pending: &Arc<PendingBlocks>,
        evidence: EdgeSnapshotEvidenceV1,
    ) -> Result<(), &'static str> {
        let record = self.current_record().ok_or("MissingSnapshotRecord")?;
        if !Arc::ptr_eq(&record.pending, pending) {
            return Err("PendingSnapshotBindingConflict");
        }
        let mut installed =
            record.edge_evidence.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        if installed.replace(evidence).is_some() {
            return Err("DuplicateSnapshotEvidence");
        }
        Ok(())
    }
    #[cfg(feature = "edge-measurement")]
    fn finish_edge_registry_received(
        &self,
        pending: &Arc<PendingBlocks>,
        metadata: Option<base_flashblocks::PendingSnapshotMetadataV2>,
    ) -> Result<(), &'static str> {
        let Some(metadata) = metadata else {
            return Ok(());
        };
        let recorder = EdgeMeasurementGlobal::installed().ok_or("EdgeRecorderUninstalled")?;
        let (metadata, payload_first, processor, connection, registry_terminal) =
            recorder.snapshot_evidence(metadata).ok_or("MissingRequiredSnapshotEvidence")?;
        let registry_terminal_record_hash = B256::new(EdgeMeasurementDurabilityV1::sha256(
            &EdgeCanonicalWriterV1::canonical_bytes(&EdgeCanonicalWriterV1::registry_h2_value(
                registry_terminal,
            ))
            .map_err(|_| "RegistryTerminalCanonicalizationFailed")?,
        ));
        self.attach_edge_snapshot_evidence(
            pending,
            EdgeSnapshotEvidenceV1 {
                source_generation: processor.source_generation,
                pending_snapshot_sequence: metadata.identity.pending_snapshot_sequence,
                coverage_sequence: registry_terminal.coverage_sequence,
                payload_first_record_sequence: payload_first.record_sequence,
                payload_first_record_hash: payload_first.record_hash,
                structural_terminal_hash: processor.structural_terminal_hash,
                connection_sequence: connection.connection_sequence,
                connection_record_hash: connection.record_hash,
                registry_terminal_record_hash,
            },
        )
    }

    fn record_is_current(&self, record: &PendingSnapshotRecord) -> bool {
        let current = self.flashblocks.get_pending_blocks();
        current.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &record.pending))
    }
}

impl<Provider> TraderSnapshotPort for CliTraderSnapshotPort<Provider>
where
    Provider: StateProviderFactory
        + HeaderProvider<Header = Header>
        + Clone
        + Debug
        + Send
        + Sync
        + 'static,
{
    fn capture_latest(
        &self,
        factory: &SnapshotHandleFactory,
    ) -> Result<Option<SnapshotHandle>, PortError> {
        let Some(record) = self.current_record() else { return Ok(None) };
        if !self.record_is_current(&record) {
            return Ok(None);
        }
        #[cfg(feature = "edge-measurement")]
        {
            let evidence =
                *record.edge_evidence.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            evidence.map_or_else(
                || factory.issue(Arc::clone(&record.view), record.received_at).map(Some),
                |evidence| {
                    factory
                        .issue_with_edge_evidence(
                            Arc::clone(&record.view),
                            record.received_at,
                            evidence,
                        )
                        .map(Some)
                },
            )
        }
        #[cfg(not(feature = "edge-measurement"))]
        factory.issue(Arc::clone(&record.view), record.received_at).map(Some)
    }

    fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
        let Some(record) = self.current_record() else { return false };
        self.record_is_current(&record) && handle.matches_capture(&record.view, record.received_at)
    }

    fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError> {
        self.provider.state_by_block_hash(block_hash).map_err(|_| PortError::ProviderUnavailable)
    }

    fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError> {
        let header = self
            .provider
            .sealed_header_by_hash(block_hash)
            .map_err(|_| PortError::HeaderUnavailable)?
            .ok_or(PortError::HeaderUnavailable)?;
        Ok(Sealed::new_unchecked(header.clone_header(), header.hash()))
    }
}
#[cfg(feature = "edge-measurement")]
#[derive(Debug, Clone)]
struct EdgeCliProducerConfigV1 {
    output_root: PathBuf,
    output_root_handle: Arc<File>,
    producer_epoch: NonZeroU64,
    producer_digest: B256,
    reject_schema_digest: B256,
    prereg_digest: B256,
    policy_digest: B256,
    config_digest: B256,
    owner_approval_receipt_digest: B256,
    flash_event_capacity: usize,
    flash_active_capacity: usize,
    flash_registry_capacity: usize,
    blink_record_capacity: usize,
    blink_candidate_capacity: usize,
    measurement_sender: Address,
    executor_runtime_hash: B256,
    v2_adapter: Address,
    v2_adapter_runtime_hash: B256,
    v3_adapter: Address,
    v3_adapter_runtime_hash: B256,
    aerodrome_adapter: Address,
    aerodrome_adapter_runtime_hash: B256,
    g0_code_identity_digest: B256,
    raw_reject_inventory_sha256: B256,
    raw_reject_source_sha256: B256,
    measurement_tx_source_sha256: B256,
}

#[cfg(feature = "edge-measurement")]
impl EdgeCliProducerConfigV1 {
    const MAX_CONFIG_BYTES: u64 = 1_048_576;
    const LINUX_O_DIRECTORY: i32 = 0o200_000;
    const LINUX_O_NOFOLLOW: i32 = 0o400_000;
    const LINUX_O_CLOEXEC: i32 = 0o2_000_000;

    fn from_environment() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os("BASE_EDGE_MEASUREMENT_CONFIG") else {
            return Ok(None);
        };
        let path = path
            .into_string()
            .map(PathBuf::from)
            .map_err(|_| "BASE_EDGE_MEASUREMENT_CONFIG is not Unicode".to_string())?;
        Self::load(&path).map(Some)
    }

    fn load(path: &Path) -> Result<Self, String> {
        let mut file = Self::open_validated_config(path)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(Self::MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("edge config read failed: {error}"))?;
        if bytes.len() as u64 > Self::MAX_CONFIG_BYTES {
            return Err("edge config exceeds 1 MiB".to_string());
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "edge config contents are not UTF-8".to_string())?;
        let values = FlatJsonObjectV1::parse(text)?;
        const KEYS: [&str; 25] = [
            "outputRoot",
            "producerEpoch",
            "producerDigest",
            "rejectSchemaDigest",
            "preregDigest",
            "policyDigest",
            "configDigest",
            "ownerApprovalReceiptDigest",
            "flashEventCapacity",
            "flashActiveCapacity",
            "flashRegistryCapacity",
            "blinkRecordCapacity",
            "blinkCandidateCapacity",
            "measurementSender",
            "executorRuntimeHash",
            "v2Adapter",
            "v2AdapterRuntimeHash",
            "v3Adapter",
            "v3AdapterRuntimeHash",
            "aerodromeAdapter",
            "aerodromeAdapterRuntimeHash",
            "g0CodeIdentityDigest",
            "rawRejectInventorySha256",
            "rawRejectSourceSha256",
            "measurementTxSourceSha256",
        ];
        if values.0.len() != KEYS.len()
            || values.0.keys().any(|key| !KEYS.contains(&key.as_str()))
            || KEYS.iter().any(|key| !values.0.contains_key(*key))
        {
            return Err("edge config has missing or unknown keys".to_string());
        }
        let output_root = PathBuf::from(values.string("outputRoot")?);
        if output_root.as_os_str().is_empty() {
            return Err("edge outputRoot is empty".to_string());
        }
        let output_root_handle = Self::open_validated_output_root(&output_root)?;
        Ok(Self {
            output_root,
            output_root_handle,
            producer_epoch: NonZeroU64::new(values.u64("producerEpoch")?)
                .ok_or_else(|| "edge producerEpoch is zero".to_string())?,
            producer_digest: values.digest("producerDigest")?,
            reject_schema_digest: values.digest("rejectSchemaDigest")?,
            prereg_digest: values.digest("preregDigest")?,
            policy_digest: values.digest("policyDigest")?,
            config_digest: values.digest("configDigest")?,
            owner_approval_receipt_digest: values.digest("ownerApprovalReceiptDigest")?,
            flash_event_capacity: values.capacity("flashEventCapacity")?,
            flash_active_capacity: values.capacity("flashActiveCapacity")?,
            flash_registry_capacity: values.capacity("flashRegistryCapacity")?,
            measurement_sender: values.address("measurementSender")?,
            executor_runtime_hash: values.digest("executorRuntimeHash")?,
            v2_adapter: values.address("v2Adapter")?,
            v2_adapter_runtime_hash: values.digest("v2AdapterRuntimeHash")?,
            v3_adapter: values.address("v3Adapter")?,
            v3_adapter_runtime_hash: values.digest("v3AdapterRuntimeHash")?,
            aerodrome_adapter: values.address("aerodromeAdapter")?,
            aerodrome_adapter_runtime_hash: values.digest("aerodromeAdapterRuntimeHash")?,
            g0_code_identity_digest: values.digest("g0CodeIdentityDigest")?,
            raw_reject_inventory_sha256: values.digest("rawRejectInventorySha256")?,
            raw_reject_source_sha256: values.digest("rawRejectSourceSha256")?,
            measurement_tx_source_sha256: values.digest("measurementTxSourceSha256")?,
            blink_record_capacity: values.capacity("blinkRecordCapacity")?,
            blink_candidate_capacity: values.capacity("blinkCandidateCapacity")?,
        })
    }

    fn validate_components(path: &Path) -> Result<(), String> {
        if !path.is_absolute() {
            return Err("edge authority path is not absolute".to_string());
        }
        let mut current = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(value) => current.push(value),
                Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                    return Err("edge authority path contains an unsupported component".to_string());
                }
            }
            let metadata = fs::symlink_metadata(&current)
                .map_err(|error| format!("edge authority path metadata failed: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("edge authority path contains a symlink".to_string());
            }
        }
        Ok(())
    }

    fn effective_uid() -> Result<u32, String> {
        let status = fs::read_to_string("/proc/self/status")
            .map_err(|error| format!("effective uid read failed: {error}"))?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| "effective uid is unavailable".to_string())
    }

    fn open_validated_config(path: &Path) -> Result<File, String> {
        Self::validate_components(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(Self::LINUX_O_NOFOLLOW | Self::LINUX_O_CLOEXEC)
            .open(path)
            .map_err(|error| format!("edge config open failed: {error}"))?;
        let metadata =
            file.metadata().map_err(|error| format!("edge config metadata failed: {error}"))?;
        if !metadata.is_file() {
            return Err("edge config is not a regular file".to_string());
        }
        if metadata.len() > Self::MAX_CONFIG_BYTES {
            return Err("edge config exceeds 1 MiB".to_string());
        }
        if metadata.mode() & 0o022 != 0 {
            return Err("edge config is group- or world-writable".to_string());
        }
        if metadata.uid() != Self::effective_uid()? {
            return Err("edge config is not owned by the effective uid".to_string());
        }
        Ok(file)
    }

    fn open_validated_output_root(path: &Path) -> Result<Arc<File>, String> {
        if !path.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .map_err(|error| format!("edge outputRoot creation failed: {error}"))?;
        }
        Self::validate_components(path)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(Self::LINUX_O_DIRECTORY | Self::LINUX_O_NOFOLLOW | Self::LINUX_O_CLOEXEC)
            .open(path)
            .map_err(|error| format!("edge outputRoot open failed: {error}"))?;
        let metadata = directory
            .metadata()
            .map_err(|error| format!("edge outputRoot metadata failed: {error}"))?;
        if !metadata.is_dir() {
            return Err("edge outputRoot is not a directory".to_string());
        }
        if metadata.uid() != Self::effective_uid()? {
            return Err("edge outputRoot is not owned by the effective uid".to_string());
        }
        if metadata.mode() & 0o077 != 0 {
            return Err("edge outputRoot is not owner-private".to_string());
        }

        let pinned = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let probe = pinned.join(format!(".edge-measurement-preflight-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&probe)
            .map_err(|error| format!("edge outputRoot write preflight failed: {error}"))?;
        file.write_all(b"edge-measurement-preflight\n")
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("edge outputRoot fsync preflight failed: {error}"))?;
        drop(file);
        fs::remove_file(&probe)
            .map_err(|error| format!("edge outputRoot cleanup failed: {error}"))?;
        directory
            .sync_all()
            .map_err(|error| format!("edge outputRoot directory fsync failed: {error}"))?;
        Ok(Arc::new(directory))
    }
}

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct FlatJsonObjectV1(BTreeMap<String, FlatJsonValueV1>);

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
enum FlatJsonValueV1 {
    String(String),
    Number(String),
}

#[cfg(feature = "edge-measurement")]
impl FlatJsonObjectV1 {
    fn parse(input: &str) -> Result<Self, String> {
        let mut parser = FlatJsonParserV1 { bytes: input.as_bytes(), offset: 0 };
        parser.whitespace();
        parser.expect(b'{')?;
        let mut values = BTreeMap::new();
        parser.whitespace();
        if parser.peek() == Some(b'}') {
            parser.offset += 1;
        } else {
            loop {
                parser.whitespace();
                let key = parser.string()?;
                parser.whitespace();
                parser.expect(b':')?;
                parser.whitespace();
                let value = match parser.peek() {
                    Some(b'"') => FlatJsonValueV1::String(parser.string()?),
                    Some(b'0'..=b'9') => FlatJsonValueV1::Number(parser.number()?),
                    _ => {
                        return Err("edge config value must be a string or unsigned integer".into());
                    }
                };
                if values.insert(key, value).is_some() {
                    return Err("edge config contains a duplicate key".into());
                }
                parser.whitespace();
                match parser.peek() {
                    Some(b',') => parser.offset += 1,
                    Some(b'}') => {
                        parser.offset += 1;
                        break;
                    }
                    _ => return Err("edge config object is malformed".into()),
                }
            }
        }
        parser.whitespace();
        if parser.offset != parser.bytes.len() {
            return Err("edge config has trailing bytes".into());
        }
        Ok(Self(values))
    }

    fn string(&self, key: &str) -> Result<String, String> {
        match self.0.get(key) {
            Some(FlatJsonValueV1::String(value)) => Ok(value.clone()),
            _ => Err(format!("edge config {key} must be a string")),
        }
    }

    fn u64(&self, key: &str) -> Result<u64, String> {
        match self.0.get(key) {
            Some(FlatJsonValueV1::Number(value)) => {
                value.parse().map_err(|_| format!("edge config {key} is malformed"))
            }
            _ => Err(format!("edge config {key} must be an unsigned integer")),
        }
    }

    fn capacity(&self, key: &str) -> Result<usize, String> {
        let value = self.u64(key)?;
        if value == 0 {
            return Err(format!("edge config {key} is zero"));
        }
        usize::try_from(value).map_err(|_| format!("edge config {key} is too large"))
    }

    fn digest(&self, key: &str) -> Result<B256, String> {
        let value = self.string(key)?;
        if value.len() != 66
            || !value.starts_with("0x")
            || !value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
        {
            return Err(format!("edge config {key} is not a 32-byte digest"));
        }
        let digest =
            B256::from_str(&value).map_err(|_| format!("edge config {key} is malformed"))?;
        if digest.is_zero() {
            return Err(format!("edge config {key} is zero"));
        }
        Ok(digest)
    }

    fn address(&self, key: &str) -> Result<Address, String> {
        let value = self.string(key)?;
        let address =
            Address::from_str(&value).map_err(|_| format!("edge config {key} is malformed"))?;
        if address.is_zero() {
            return Err(format!("edge config {key} is zero"));
        }
        Ok(address)
    }
}

#[cfg(feature = "edge-measurement")]
#[derive(Debug)]
struct FlatJsonParserV1<'a> {
    bytes: &'a [u8],
    offset: usize,
}

#[cfg(feature = "edge-measurement")]
impl FlatJsonParserV1<'_> {
    fn whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.offset += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.peek() != Some(expected) {
            return Err("edge config JSON is malformed".into());
        }
        self.offset += 1;
        Ok(())
    }

    fn number(&mut self) -> Result<String, String> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        let value = std::str::from_utf8(&self.bytes[start..self.offset])
            .map_err(|_| "edge config number is malformed")?;
        if value.len() > 1 && value.starts_with('0') {
            return Err("edge config number has a leading zero".into());
        }
        Ok(value.to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut value = String::new();
        loop {
            let byte = self.peek().ok_or_else(|| "unterminated edge config string".to_string())?;
            self.offset += 1;
            match byte {
                b'"' => return Ok(value),
                b'\\' => {
                    let escaped =
                        self.peek().ok_or_else(|| "unterminated edge config escape".to_string())?;
                    self.offset += 1;
                    value.push(match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        _ => return Err("unsupported edge config string escape".into()),
                    });
                }
                0..=31 => return Err("edge config string contains a control byte".into()),
                _ if byte.is_ascii() => value.push(char::from(byte)),
                _ => {
                    let start = self.offset - 1;
                    let remaining = std::str::from_utf8(&self.bytes[start..])
                        .map_err(|_| "edge config string is not UTF-8")?;
                    let character =
                        remaining.chars().next().ok_or("edge config string is malformed")?;
                    self.offset = start + character.len_utf8();
                    value.push(character);
                }
            }
        }
    }
}

/// Exact-1 Phase A extension configuration with post-gate receive credential input.
#[derive(Debug, Clone)]
pub struct BaseNodeTraderConfig {
    flashblocks: Arc<FlashblocksState>,
    credential_file: Option<OsString>,
    t4a_shadow: bool,
    t4b_shadow: bool,
    t4d_shadow: bool,
    #[cfg(feature = "edge-measurement")]
    edge_measurement: Result<Option<EdgeCliProducerConfigV1>, String>,
    #[cfg(feature = "edge-measurement")]
    edge_owner: Arc<RwLock<Option<Arc<EdgeMeasurementOwnerV1>>>>,
}

impl BaseNodeTraderConfig {
    /// Returns true only for the exact native `OsStr` bytes `1`.
    pub fn enabled(env: Option<&OsStr>) -> bool {
        env == Some(OsStr::new("1"))
    }

    #[cfg(feature = "t4a-shadow")]
    fn t4a_shadow_enabled() -> bool {
        Self::enabled(std::env::var_os("MEV_TRADER_T4A_SHADOW").as_deref())
    }

    #[cfg(not(feature = "t4a-shadow"))]
    const fn t4a_shadow_enabled() -> bool {
        false
    }

    #[cfg(feature = "t4b-shadow")]
    fn t4b_shadow_enabled() -> bool {
        Self::enabled(std::env::var_os("MEV_TRADER_T4B_SHADOW").as_deref())
    }

    #[cfg(not(feature = "t4b-shadow"))]
    const fn t4b_shadow_enabled() -> bool {
        false
    }
    #[cfg(feature = "t4d-shadow")]
    fn t4d_shadow_enabled() -> bool {
        Self::enabled(std::env::var_os("MEV_TRADER_T4D_SHADOW").as_deref())
    }

    #[cfg(not(feature = "t4d-shadow"))]
    const fn t4d_shadow_enabled() -> bool {
        false
    }

    /// Applies exact-1 and flashblocks-present gates before consulting the credential environment.
    pub fn from_inputs(
        flashblocks_config: &Option<FlashblocksConfig>,
        env: Option<&OsStr>,
    ) -> Option<Self> {
        if !Self::enabled(env) {
            return None;
        }
        let config = flashblocks_config.as_ref()?;
        let credential_file = std::env::var_os("MEV_TRADER_BLINK_CREDENTIAL_FILE");
        Some(Self {
            flashblocks: Arc::clone(&config.state),
            credential_file,
            t4a_shadow: Self::t4a_shadow_enabled(),
            t4b_shadow: Self::t4b_shadow_enabled(),
            t4d_shadow: Self::t4d_shadow_enabled(),
            #[cfg(feature = "edge-measurement")]
            edge_measurement: EdgeCliProducerConfigV1::from_environment(),
            #[cfg(feature = "edge-measurement")]
            edge_owner: Arc::new(RwLock::new(None)),
        })
    }
    #[cfg(feature = "edge-measurement")]
    fn with_edge_measurement(
        &self,
        runtime_config: MevTraderRuntimeConfig,
    ) -> eyre::Result<MevTraderRuntimeConfig> {
        let Some(config) =
            self.edge_measurement.as_ref().map_err(|error| eyre::eyre!(error.clone()))?
        else {
            return Ok(runtime_config);
        };
        let owner = EdgeMeasurementOwnerV1::new(EdgeMeasurementOwnerConfigV1 {
            producer_epoch: config.producer_epoch.get(),
            output_root: config.output_root.clone(),
            output_root_handle: Arc::clone(&config.output_root_handle),
            producer_digest: config.producer_digest,
            reject_schema_digest: config.reject_schema_digest,
            prereg_digest: config.prereg_digest,
            policy_digest: config.policy_digest,
            config_digest: config.config_digest,
            owner_approval_receipt_digest: config.owner_approval_receipt_digest,
            record_queue_capacity: config.blink_record_capacity,
            candidate_queue_capacity: config.blink_candidate_capacity,
            measurement_sender: config.measurement_sender,
            executor_runtime_hash: config.executor_runtime_hash,
            v2_adapter: config.v2_adapter,
            v2_adapter_runtime_hash: config.v2_adapter_runtime_hash,
            v3_adapter: config.v3_adapter,
            v3_adapter_runtime_hash: config.v3_adapter_runtime_hash,
            aerodrome_adapter: config.aerodrome_adapter,
            aerodrome_adapter_runtime_hash: config.aerodrome_adapter_runtime_hash,
            g0_code_identity_digest: config.g0_code_identity_digest,
            raw_reject_inventory_sha256: config.raw_reject_inventory_sha256,
            raw_reject_source_sha256: config.raw_reject_source_sha256,
            measurement_tx_source_sha256: config.measurement_tx_source_sha256,
        })
        .map_err(|error| eyre::eyre!("Blink edge owner construction failed: {error}"))?;
        EdgeMeasurementGlobal::install(EdgeMeasurementInstallConfigV1 {
            producer_epoch: config.producer_epoch,
            event_queue_capacity: config.flash_event_capacity,
            active_state_capacity: config.flash_active_capacity,
            pending_registry_capacity: config.flash_registry_capacity,
            terminal_record_capacity: PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2,
        })
        .map_err(|error| eyre::eyre!("flash edge owner installation failed: {error:?}"))?;
        *self.edge_owner.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::clone(&owner));
        runtime_config
            .with_edge_measurement_owner(owner)
            .map_err(|error| eyre::eyre!("Blink edge owner installation failed: {error}"))
    }

    #[cfg(not(feature = "edge-measurement"))]
    fn with_edge_measurement(
        &self,
        runtime_config: MevTraderRuntimeConfig,
    ) -> eyre::Result<MevTraderRuntimeConfig> {
        Ok(runtime_config)
    }

    /// Creates the sole snapshot subscription and receive-only A1 runtime.
    pub fn start_idle(self) -> eyre::Result<BaseNodeTraderStart> {
        if self.t4b_shadow || self.t4d_shadow {
            eyre::bail!("selected shadow authority requires the in-process node provider");
        }
        let runtime_config = self.with_edge_measurement(t4a_runtime_config(self.t4a_shadow)?)?;
        self.start_with_runtime_config(runtime_config)
    }

    #[cfg(feature = "t4b-shadow")]
    fn start_with_t4b_observer(
        self,
        observer: Arc<dyn CandidateTxShapeObserver>,
    ) -> eyre::Result<BaseNodeTraderStart> {
        if !self.t4a_shadow || !self.t4b_shadow {
            eyre::bail!("T4b shadow requires exact T4a and T4b opt-in");
        }
        let runtime_config =
            self.with_edge_measurement(t4a_runtime_config(true)?.with_t4b_observer(observer))?;
        self.start_with_runtime_config(runtime_config)
    }
    #[cfg(feature = "t4d-shadow")]
    fn start_with_t4d_observer(
        self,
        observer: Arc<dyn CandidateTxShapeObserver>,
    ) -> eyre::Result<BaseNodeTraderStart> {
        if !self.t4a_shadow || !self.t4b_shadow || !self.t4d_shadow {
            eyre::bail!("T4d shadow requires exact T4a, T4b, and T4d opt-in");
        }
        let runtime_config =
            self.with_edge_measurement(t4a_runtime_config(true)?.with_t4b_observer(observer))?;
        self.start_with_runtime_config(runtime_config)
    }

    fn start_with_runtime_config(
        self,
        runtime_config: MevTraderRuntimeConfig,
    ) -> eyre::Result<BaseNodeTraderStart> {
        #[cfg(feature = "edge-measurement")]
        let edge_owner =
            self.edge_owner.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let receiver = self.flashblocks.subscribe_to_flashblocks();
        let runtime = Arc::new(MevTraderRuntime::start(runtime_config)?);
        let client = self.credential_file.and_then(|credential_file| {
            BlinkFeedClient::new(
                BlinkIngressConfig::production(credential_file),
                Arc::clone(&runtime),
            )
        });
        if client.is_none() {
            runtime.set_a1_status(A1Status::DisabledNoConnect);
        }
        Ok(BaseNodeTraderStart {
            receiver,
            runtime,
            client,
            #[cfg(feature = "edge-measurement")]
            edge_owner,
        })
    }
}

/// Provider-independent node-start resources for the receive-only runtime.
#[derive(Debug)]
pub struct BaseNodeTraderStart {
    receiver: tokio::sync::broadcast::Receiver<Arc<PendingBlocks>>,
    runtime: Arc<MevTraderRuntime>,
    client: Option<BlinkFeedClient>,
    #[cfg(feature = "edge-measurement")]
    edge_owner: Option<Arc<EdgeMeasurementOwnerV1>>,
}

impl BaseNodeTraderStart {
    /// Returns the exact one existing flashblock broadcast subscription.
    pub const fn subscriber_count(&self) -> usize {
        1
    }

    /// Returns the exact one sole consumer.
    pub fn worker_count(&self) -> usize {
        if self.runtime.worker_is_claimed() { 1 } else { 0 }
    }

    /// Returns the exact one dedicated Rayon4 analysis pool.
    pub const fn pool_count(&self) -> usize {
        1
    }

    /// Returns the exact one separate watchdog/control domain.
    pub const fn watchdog_count(&self) -> usize {
        1
    }

    /// Returns whether the measurement-only registry is disabled.
    pub fn registry_is_empty(&self) -> bool {
        self.runtime.registry_is_empty()
    }

    /// Returns whether a valid receive-only ingress client was constructed.
    pub const fn has_live_victim_producer(&self) -> bool {
        self.client.is_some()
    }
}

/// CLI-owned node extension for snapshot observation and receive-only A1 ownership.
#[derive(Debug)]
pub struct BaseNodeTraderExtension {
    config: BaseNodeTraderConfig,
}

impl BaseNodeTraderExtension {
    /// Creates an extension from exact-1 configuration.
    pub const fn new(config: BaseNodeTraderConfig) -> Self {
        Self { config }
    }
}

impl FromExtensionConfig for BaseNodeTraderExtension {
    type Config = BaseNodeTraderConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

impl BaseNodeExtension for BaseNodeTraderExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_node_started_hook(move |node| {
            let chain_spec = node.chain_spec();
            let port = Arc::new(CliTraderSnapshotPort::new(
                Arc::clone(&self.config.flashblocks),
                node.provider().clone(),
            ));
            #[cfg(feature = "edge-measurement")]
            let edge_measurement_registry = self.config.flashblocks.edge_measurement_registry();
            #[cfg(feature = "edge-measurement")]
            let edge_producer_config = self
                .config
                .edge_measurement
                .as_ref()
                .ok()
                .and_then(|value| value.as_ref())
                .cloned();
            #[cfg(feature = "t4b-shadow")]
            let start = if self.config.t4d_shadow {
                #[cfg(feature = "t4d-shadow")]
                {
                    if !self.config.t4a_shadow || !self.config.t4b_shadow {
                        eyre::bail!("T4d shadow requires exact T4a, T4b, and T4d opt-in");
                    }
                    let observer =
                        t4d_shadow::observer(Arc::clone(&port), chain_spec.chain().id())?;
                    self.config.start_with_t4d_observer(observer)?
                }
                #[cfg(not(feature = "t4d-shadow"))]
                {
                    unreachable!("T4d opt-in is unavailable without the compiled feature")
                }
            } else if self.config.t4b_shadow {
                let observer =
                    t4b_shadow::observer(Arc::clone(&port), chain_spec.chain().id())?;
                self.config.start_with_t4b_observer(observer)?
            } else {
                self.config.start_idle()?
            };
            #[cfg(not(feature = "t4b-shadow"))]
            let start = self.config.start_idle()?;
            let BaseNodeTraderStart {
                mut receiver,
                runtime,
                client,
                #[cfg(feature = "edge-measurement")]
                edge_owner,
            } = start;
            let concrete_consumer_port = Arc::clone(&port);
            let consumer_port: Arc<dyn TraderSnapshotPort> = concrete_consumer_port;
            let executor = node.task_executor;
            let startup_status = runtime.a1_status();
            let registry_empty = runtime.registry_is_empty();
            #[cfg(feature = "edge-measurement")]
            let edge_cutoff_owner = edge_owner.clone();
            #[cfg(feature = "edge-measurement")]
            let edge_writer_inputs =
                edge_owner.zip(edge_producer_config).and_then(|(owner, provenance)| {
                    EdgeMeasurementGlobal::installed()
                        .map(|recorder| (provenance, recorder, owner))
                });
            executor.spawn_with_graceful_shutdown_signal(move |signal| {
                Box::pin(async move {
                    #[cfg(feature = "edge-measurement")]
                    let edge_writer_handle =
                        edge_writer_inputs.map(|(provenance, recorder, owner)| {
                            tokio::task::spawn_blocking(move || {
                                EdgeCanonicalWriterV1::new(provenance, recorder, owner).run()
                            })
                        });
                    #[cfg(feature = "edge-measurement")]
                    let edge_cutoff_latched = Arc::new(EdgeCutoffLatchV1::default());
                    #[cfg(feature = "edge-measurement")]
                    let edge_cutoff_handle = EdgeMeasurementGlobal::installed()
                        .zip(edge_cutoff_owner.clone())
                        .map(|(recorder, owner)| {
                            let latched = Arc::clone(&edge_cutoff_latched);
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(72 * 60 * 60)).await;
                                latch_edge_cutoff_once(&latched, &recorder, &owner);
                            })
                        });
                    let snapshot_runtime = Arc::clone(&runtime);
                    #[cfg(feature = "edge-measurement")]
                    let snapshot_cleanup_registry = edge_measurement_registry;
                    let snapshot_handle = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                () = snapshot_runtime.shutdown().wait_cancelled() => {
                                    loop {
                                        match receiver.try_recv() {
                                            Ok(pending) => {
                                                #[cfg(feature = "edge-measurement")]
                                                let measurement_pending = Arc::clone(&pending);
                                                port.record_pending_snapshot(pending, Instant::now());
                                                #[cfg(feature = "edge-measurement")]
                                                match tokio::task::block_in_place(|| {
                                                    edge_measurement_registry
                                                        .cli_received(&measurement_pending)
                                                }) {
                                                    Ok(metadata) => {
                                                        if port
                                                            .finish_edge_registry_received(
                                                                &measurement_pending,
                                                                metadata,
                                                            )
                                                            .is_err()
                                                        {
                                                            latch_edge_failure(
                                                                "MissingRequiredEvidenceDuringCutoffDrain",
                                                            );
                                                        }
                                                    }
                                                    Err(_) => latch_edge_failure(
                                                        "CliRegistryLookupFailedDuringCutoffDrain",
                                                    ),
                                                }
                                            }
                                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(
                                                count,
                                            )) => {
                                                #[cfg(not(feature = "edge-measurement"))]
                                                let _ = count;
                                                #[cfg(feature = "edge-measurement")]
                                                if edge_measurement_registry.cli_lagged(count).is_err()
                                                {
                                                    latch_edge_failure(
                                                        "CliLagRangeFailedDuringCutoffDrain",
                                                    );
                                                }
                                            }
                                            Err(
                                                tokio::sync::broadcast::error::TryRecvError::Empty,
                                            ) => break,
                                            Err(
                                                tokio::sync::broadcast::error::TryRecvError::Closed,
                                            ) => {
                                                #[cfg(feature = "edge-measurement")]
                                                if edge_measurement_registry.cli_closed().is_err() {
                                                    latch_edge_failure(
                                                        "CliCloseRangeFailedDuringCutoffDrain",
                                                    );
                                                }
                                                return;
                                            }
                                        }
                                    }
                                    #[cfg(feature = "edge-measurement")]
                                    if edge_measurement_registry.cli_cancelled().is_err() {
                                        latch_edge_failure("CliCancellationRangeFailed");
                                    }
                                    return;
                                },
                                pending = receiver.recv() => match pending {
                                    Ok(pending) => {
                                        #[cfg(feature = "edge-measurement")]
                                        let measurement_pending = Arc::clone(&pending);
                                        port.record_pending_snapshot(pending, Instant::now());
                                        #[cfg(feature = "edge-measurement")]
                                        match tokio::task::block_in_place(|| {
                                            edge_measurement_registry
                                                .cli_received(&measurement_pending)
                                        }) {
                                            Ok(metadata) => {
                                                if port
                                                    .finish_edge_registry_received(
                                                        &measurement_pending,
                                                        metadata,
                                                    )
                                                    .is_err()
                                                {
                                                    latch_edge_failure("MissingRequiredEvidence");
                                                }
                                            }
                                            Err(_) => latch_edge_failure("CliRegistryLookupFailed"),
                                        }
                                    },
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                                        #[cfg(not(feature = "edge-measurement"))]
                                        let _ = count;
                                        #[cfg(feature = "edge-measurement")]
                                        if edge_measurement_registry.cli_lagged(count).is_err() {
                                            latch_edge_failure("CliLagRangeFailed");
                                        }
                                        continue;
                                    },
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                        #[cfg(feature = "edge-measurement")]
                                        if edge_measurement_registry.cli_closed().is_err() {
                                            latch_edge_failure("CliCloseRangeFailed");
                                        }
                                        return;
                                    },
                                }
                            }
                        }
                    });
                    let consumer_runtime = Arc::clone(&runtime);
                    let consumer_handle = tokio::spawn(async move {
                        consumer_runtime.run_consumer(consumer_port, chain_spec).await;
                    });
                    let control_runtime = Arc::clone(&runtime);
                    let control_handle =
                        tokio::spawn(async move { control_runtime.run_control().await });
                    let ingress_handle = client.map(|client| tokio::spawn(client.run()));

                    let _guard = signal.await;
                    #[cfg(feature = "edge-measurement")]
                    abort_and_await_edge_cutoff_task(edge_cutoff_handle).await;
                    runtime.close();
                    #[cfg(feature = "edge-measurement")]
                    match await_edge_shutdown_task(
                        snapshot_handle,
                        EDGE_SHUTDOWN_JOIN_DEADLINE_V1,
                    )
                    .await
                    {
                        EdgeShutdownTaskOutcome::Completed(()) => {}
                        EdgeShutdownTaskOutcome::JoinFailed => {
                            latch_edge_failure("SnapshotTaskJoinFailed");
                            cleanup_snapshot_task_registry(
                                &snapshot_cleanup_registry,
                                "SnapshotTaskJoinFailedCliCancellationRangeFailed",
                            );
                        }
                        EdgeShutdownTaskOutcome::TimedOut => {
                            latch_edge_failure("SnapshotTaskJoinTimeout");
                            cleanup_snapshot_task_registry(
                                &snapshot_cleanup_registry,
                                "SnapshotTaskJoinTimeoutCliCancellationRangeFailed",
                            );
                        }
                    }
                    #[cfg(not(feature = "edge-measurement"))]
                    let _ = snapshot_handle.await;
                    #[cfg(feature = "edge-measurement")]
                    match await_edge_shutdown_task(
                        consumer_handle,
                        EDGE_SHUTDOWN_JOIN_DEADLINE_V1,
                    )
                    .await
                    {
                        EdgeShutdownTaskOutcome::Completed(()) => {}
                        EdgeShutdownTaskOutcome::JoinFailed => {
                            latch_edge_failure("ConsumerTaskJoinFailed");
                        }
                        EdgeShutdownTaskOutcome::TimedOut => {
                            latch_edge_failure("ConsumerTaskJoinTimeout");
                        }
                    }
                    #[cfg(not(feature = "edge-measurement"))]
                    let _ = consumer_handle.await;
                    #[cfg(feature = "edge-measurement")]
                    match await_edge_shutdown_task(
                        control_handle,
                        EDGE_SHUTDOWN_JOIN_DEADLINE_V1,
                    )
                    .await
                    {
                        EdgeShutdownTaskOutcome::Completed(()) => {}
                        EdgeShutdownTaskOutcome::JoinFailed => {
                            latch_edge_failure("ControlTaskJoinFailed");
                        }
                        EdgeShutdownTaskOutcome::TimedOut => {
                            latch_edge_failure("ControlTaskJoinTimeout");
                        }
                    }
                    #[cfg(not(feature = "edge-measurement"))]
                    let _ = control_handle.await;
                    if let Some(handle) = ingress_handle {
                        #[cfg(feature = "edge-measurement")]
                        match await_edge_shutdown_task(handle, EDGE_SHUTDOWN_JOIN_DEADLINE_V1).await {
                            EdgeShutdownTaskOutcome::Completed(()) => {}
                            EdgeShutdownTaskOutcome::JoinFailed => {
                                latch_edge_failure("IngressTaskJoinFailed");
                            }
                            EdgeShutdownTaskOutcome::TimedOut => {
                                latch_edge_failure("IngressTaskJoinTimeout");
                            }
                        }
                        #[cfg(not(feature = "edge-measurement"))]
                        let _ = handle.await;
                    }
                    #[cfg(feature = "edge-measurement")]
                    if let (Some(recorder), Some(owner)) =
                        (EdgeMeasurementGlobal::installed(), edge_cutoff_owner.as_ref())
                    {
                        latch_edge_cutoff_once(&edge_cutoff_latched, &recorder, owner);
                    }
                    #[cfg(feature = "edge-measurement")]
                    if let Some(handle) = edge_writer_handle {
                        match await_edge_shutdown_task(handle, EDGE_SHUTDOWN_JOIN_DEADLINE_V1).await {
                            EdgeShutdownTaskOutcome::Completed(Ok(())) => {}
                            EdgeShutdownTaskOutcome::Completed(Err(_)) => {
                                latch_edge_failure("CanonicalWriterFinalFailed");
                            }
                            EdgeShutdownTaskOutcome::JoinFailed => {
                                latch_edge_failure("CanonicalWriterJoinFailed");
                            }
                            EdgeShutdownTaskOutcome::TimedOut => {
                                latch_edge_failure("CanonicalWriterJoinTimeout");
                            }
                        }
                    }
                })
            });
            info!(
                status = ?startup_status,
                registry_empty,
                receive_only = true,
                "MEV trader Phase A receive-only runtime started"
            );
            Ok(())
        })
    }
}

/// Public exact-1 installer called from standard-node assembly before flashblocks config is moved.
#[derive(Debug, Default, Clone, Copy)]
pub struct MevTraderPhaseAInstaller;

impl MevTraderPhaseAInstaller {
    /// Installs exactly one extension only for `MEV_TRADER_PHASE_A=1` plus flashblocks `Some`.
    pub fn maybe_install(
        runner: &mut BaseNodeRunner,
        flashblocks_config: &Option<FlashblocksConfig>,
        env: Option<&OsStr>,
    ) {
        if let Some(config) = BaseNodeTraderConfig::from_inputs(flashblocks_config, env) {
            runner.install_ext::<BaseNodeTraderExtension>(config);
        }
    }
}

#[cfg(feature = "t4a-shadow")]
mod t4a_provisioning {
    use std::{
        ffi::OsStr,
        fs::{File, OpenOptions},
        io::{Read, Take},
        os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        },
        path::PathBuf,
        str::FromStr,
    };

    use base_mev_trader::{
        AuditedWriteKey, BitmapWordRead, DescriptorPlanDigest, ExactProtocol, FieldKind, FieldRead,
        InitializedTickRead, MevTraderRuntimeConfig, PoolDescriptor, PoolUniverseSnapshot,
        ProvisionedPoolRegistry, RegistryDigest, StorageReadPlan, V3ReadPlan,
    };
    use eyre::{WrapErr, bail, eyre};
    use serde::Deserialize;

    const POOL_UNIVERSE_PATH: &str = "/home/ubuntu/.config/base-mev/t4a-pool-universe-v1.json";
    const MAX_POOL_UNIVERSE_BYTES: u64 = 8 * 1024 * 1024;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProvisionedRegistryFile {
        version: u8,
        registry_digest: String,
        descriptors: Vec<PoolDescriptorFile>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PoolDescriptorFile {
        pool: String,
        protocol: ProtocolFile,
        token0: String,
        token1: String,
        decimals0: u8,
        decimals1: u8,
        fee: u32,
        code_hash: String,
        read_plan: StorageReadPlanFile,
        audited_writes: Vec<AuditedWriteKeyFile>,
        descriptor_digest: String,
    }

    #[derive(Debug, Deserialize)]
    enum ProtocolFile {
        UniswapV2,
        AerodromeVolatile,
        AerodromeStable,
        UniswapV3,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    enum AuditedWriteKeyFile {
        AccountBalance { address: String, evidence_digest: String },
        AccountNonce { address: String, evidence_digest: String },
        Storage { address: String, slot: String, evidence_digest: String },
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    enum StorageReadPlanFile {
        ConstantProduct {
            reserve0: FieldReadFile,
            reserve1: FieldReadFile,
        },
        Stable {
            reserve0: FieldReadFile,
            reserve1: FieldReadFile,
            stable: FieldReadFile,
        },
        V3 {
            sqrt_price_x96: FieldReadFile,
            liquidity: FieldReadFile,
            current_tick: FieldReadFile,
            tick_spacing: i32,
            lower_word: i16,
            upper_word: i16,
            words: Vec<BitmapWordReadFile>,
            lower_sentinel: BitmapWordReadFile,
            upper_sentinel: BitmapWordReadFile,
            initialized_ticks: Vec<InitializedTickReadFile>,
            coverage_digest: String,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FieldReadFile {
        kind: FieldKindFile,
        slot: String,
        bit_offset: u16,
        bit_width: u16,
        signed: bool,
    }

    #[derive(Debug, Deserialize)]
    enum FieldKindFile {
        Reserve0,
        Reserve1,
        StableFlag,
        SqrtPriceX96,
        Liquidity,
        CurrentTick,
        LiquidityGross,
        LiquidityNet,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BitmapWordReadFile {
        word_position: i16,
        slot: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InitializedTickReadFile {
        tick: i32,
        liquidity_gross: FieldReadFile,
        liquidity_net: FieldReadFile,
    }

    pub(super) fn runtime_config() -> eyre::Result<MevTraderRuntimeConfig> {
        let bytes = read_pool_universe()?;
        if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
            bail!("T4a pool universe must be one exact JSON object without surrounding bytes");
        }
        let file: ProvisionedRegistryFile =
            serde_json::from_slice(&bytes).wrap_err("invalid T4a pool universe schema")?;
        if file.version != 1 {
            bail!("unsupported T4a pool universe version");
        }
        if file.descriptors.is_empty() {
            bail!("T4a pool universe descriptors must be nonempty");
        }

        let descriptors = file
            .descriptors
            .into_iter()
            .map(PoolDescriptor::try_from)
            .collect::<eyre::Result<Vec<_>>>()?;
        let registry = ProvisionedPoolRegistry::new(
            descriptors,
            RegistryDigest(parse_fixed(&file.registry_digest, "registry_digest")?),
        )
        .wrap_err("T4a provisioned registry validation failed")?;
        let snapshot = PoolUniverseSnapshot::capture(&registry)
            .wrap_err("T4a pool universe snapshot validation failed")?;
        MevTraderRuntimeConfig::shadow(snapshot).wrap_err("T4a shadow runtime configuration failed")
    }

    fn read_pool_universe() -> eyre::Result<Vec<u8>> {
        let mut directory = File::open("/").wrap_err("failed to open filesystem root")?;
        for component in ["home", "ubuntu", ".config", "base-mev"] {
            directory = open_child(&directory, component, true)
                .wrap_err_with(|| format!("unsafe T4a pool universe ancestor: {component}"))?;
        }
        let file =
            open_child(&directory, "t4a-pool-universe-v1.json", false).wrap_err_with(|| {
                format!("failed to open {POOL_UNIVERSE_PATH} without following symlinks")
            })?;
        let metadata = file.metadata().wrap_err("failed to inspect T4a pool universe")?;
        if !metadata.file_type().is_file() {
            bail!("T4a pool universe is not a regular file");
        }
        if metadata.mode() & 0o7777 != 0o600 {
            bail!("T4a pool universe mode must be 0600");
        }
        // SAFETY: `geteuid` has no arguments, does not dereference memory, and has no
        // preconditions.
        let service_uid = unsafe { libc::geteuid() };
        if metadata.uid() != service_uid {
            bail!("T4a pool universe is not owned by the service uid");
        }

        let mut bytes = Vec::new();
        let mut bounded: Take<File> = file.take(MAX_POOL_UNIVERSE_BYTES + 1);
        bounded.read_to_end(&mut bytes).wrap_err("failed to read T4a pool universe")?;
        if bytes.len() as u64 > MAX_POOL_UNIVERSE_BYTES {
            bail!("T4a pool universe exceeds the size limit");
        }
        Ok(bytes)
    }

    fn open_child(parent: &File, child: &str, directory: bool) -> std::io::Result<File> {
        let mut path = PathBuf::from("/proc/self/fd");
        path.push(parent.as_raw_fd().to_string());
        path.push(OsStr::new(child));
        let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC;
        if directory {
            flags |= libc::O_DIRECTORY;
        } else {
            flags |= libc::O_NONBLOCK;
        }
        OpenOptions::new().read(true).custom_flags(flags).open(path)
    }

    fn parse_fixed<T>(value: &str, field: &'static str) -> eyre::Result<T>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        value.parse().map_err(|error| eyre!("invalid {field}: {error}"))
    }

    impl TryFrom<PoolDescriptorFile> for PoolDescriptor {
        type Error = eyre::Report;

        fn try_from(value: PoolDescriptorFile) -> Result<Self, Self::Error> {
            Ok(Self {
                pool: parse_fixed(&value.pool, "pool")?,
                protocol: value.protocol.into(),
                token0: parse_fixed(&value.token0, "token0")?,
                token1: parse_fixed(&value.token1, "token1")?,
                decimals0: value.decimals0,
                decimals1: value.decimals1,
                fee: value.fee,
                code_hash: parse_fixed(&value.code_hash, "code_hash")?,
                read_plan: value.read_plan.try_into()?,
                audited_writes: value
                    .audited_writes
                    .into_iter()
                    .map(AuditedWriteKey::try_from)
                    .collect::<eyre::Result<Vec<_>>>()?,
                descriptor_digest: DescriptorPlanDigest(parse_fixed(
                    &value.descriptor_digest,
                    "descriptor_digest",
                )?),
            })
        }
    }

    impl From<ProtocolFile> for ExactProtocol {
        fn from(value: ProtocolFile) -> Self {
            match value {
                ProtocolFile::UniswapV2 => Self::UniswapV2,
                ProtocolFile::AerodromeVolatile => Self::AerodromeVolatile,
                ProtocolFile::AerodromeStable => Self::AerodromeStable,
                ProtocolFile::UniswapV3 => Self::UniswapV3,
            }
        }
    }

    impl TryFrom<AuditedWriteKeyFile> for AuditedWriteKey {
        type Error = eyre::Report;

        fn try_from(value: AuditedWriteKeyFile) -> Result<Self, Self::Error> {
            Ok(match value {
                AuditedWriteKeyFile::AccountBalance { address, evidence_digest } => {
                    Self::AccountBalance {
                        address: parse_fixed(&address, "audited address")?,
                        evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                    }
                }
                AuditedWriteKeyFile::AccountNonce { address, evidence_digest } => {
                    Self::AccountNonce {
                        address: parse_fixed(&address, "audited address")?,
                        evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                    }
                }
                AuditedWriteKeyFile::Storage { address, slot, evidence_digest } => Self::Storage {
                    address: parse_fixed(&address, "audited address")?,
                    slot: parse_fixed(&slot, "audited slot")?,
                    evidence_digest: parse_fixed(&evidence_digest, "evidence_digest")?,
                },
            })
        }
    }

    impl TryFrom<StorageReadPlanFile> for StorageReadPlan {
        type Error = eyre::Report;

        fn try_from(value: StorageReadPlanFile) -> Result<Self, Self::Error> {
            Ok(match value {
                StorageReadPlanFile::ConstantProduct { reserve0, reserve1 } => {
                    Self::constant_product(reserve0.try_into()?, reserve1.try_into()?)
                }
                StorageReadPlanFile::Stable { reserve0, reserve1, stable } => {
                    Self::stable(reserve0.try_into()?, reserve1.try_into()?, stable.try_into()?)
                }
                StorageReadPlanFile::V3 {
                    sqrt_price_x96,
                    liquidity,
                    current_tick,
                    tick_spacing,
                    lower_word,
                    upper_word,
                    words,
                    lower_sentinel,
                    upper_sentinel,
                    initialized_ticks,
                    coverage_digest,
                } => Self::v3(V3ReadPlan {
                    sqrt_price_x96: sqrt_price_x96.try_into()?,
                    liquidity: liquidity.try_into()?,
                    current_tick: current_tick.try_into()?,
                    tick_spacing,
                    lower_word,
                    upper_word,
                    words: words
                        .into_iter()
                        .map(BitmapWordRead::try_from)
                        .collect::<Result<_, _>>()?,
                    lower_sentinel: lower_sentinel.try_into()?,
                    upper_sentinel: upper_sentinel.try_into()?,
                    initialized_ticks: initialized_ticks
                        .into_iter()
                        .map(InitializedTickRead::try_from)
                        .collect::<Result<_, _>>()?,
                    coverage_digest: parse_fixed(&coverage_digest, "coverage_digest")?,
                }),
            })
        }
    }

    impl TryFrom<FieldReadFile> for FieldRead {
        type Error = eyre::Report;

        fn try_from(value: FieldReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                kind: value.kind.into(),
                slot: parse_fixed(&value.slot, "field slot")?,
                bit_offset: value.bit_offset,
                bit_width: value.bit_width,
                signed: value.signed,
            })
        }
    }

    impl From<FieldKindFile> for FieldKind {
        fn from(value: FieldKindFile) -> Self {
            match value {
                FieldKindFile::Reserve0 => Self::Reserve0,
                FieldKindFile::Reserve1 => Self::Reserve1,
                FieldKindFile::StableFlag => Self::StableFlag,
                FieldKindFile::SqrtPriceX96 => Self::SqrtPriceX96,
                FieldKindFile::Liquidity => Self::Liquidity,
                FieldKindFile::CurrentTick => Self::CurrentTick,
                FieldKindFile::LiquidityGross => Self::LiquidityGross,
                FieldKindFile::LiquidityNet => Self::LiquidityNet,
            }
        }
    }

    impl TryFrom<BitmapWordReadFile> for BitmapWordRead {
        type Error = eyre::Report;

        fn try_from(value: BitmapWordReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                word_position: value.word_position,
                slot: parse_fixed(&value.slot, "bitmap slot")?,
            })
        }
    }

    impl TryFrom<InitializedTickReadFile> for InitializedTickRead {
        type Error = eyre::Report;

        fn try_from(value: InitializedTickReadFile) -> Result<Self, Self::Error> {
            Ok(Self {
                tick: value.tick,
                liquidity_gross: value.liquidity_gross.try_into()?,
                liquidity_net: value.liquidity_net.try_into()?,
            })
        }
    }
}

#[cfg(feature = "t4b-shadow")]
mod t4b_shadow {
    use std::sync::{Arc, Weak};

    use super::{
        AccountReader, Address, B256, BlockReaderIdExt, BytecodeReader, CandidateAssemblyView,
        CandidateTxShapeObserver, CliTraderSnapshotPort, Debug, Header, HeaderProvider,
        PendingSnapshotRecord, ShadowLatestSlot, ShadowSubmit, SnapshotFreshnessToken,
        SnapshotHandle, StateProviderFactory, T4bOutcome, T4bOutcomeCounters, TraderSnapshotPort,
        TxAuthorityAssembler, TxAuthorityError, TxAuthorityNodeError, TxAuthorityNodeView,
        TxAuthorityStateRead, ValidatedUnsignedAtomicTx,
    };

    #[derive(Debug)]
    struct CliSnapshotFreshness<Provider> {
        port: Weak<CliTraderSnapshotPort<Provider>>,
        record: Weak<PendingSnapshotRecord>,
    }

    impl<Provider> SnapshotFreshnessToken for CliSnapshotFreshness<Provider>
    where
        Provider: StateProviderFactory
            + HeaderProvider<Header = Header>
            + BlockReaderIdExt
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
    {
        fn is_current(&self) -> Result<bool, TxAuthorityNodeError> {
            let Some(port) = self.port.upgrade() else {
                return Ok(false);
            };
            let Some(expected) = self.record.upgrade() else {
                return Ok(false);
            };
            let Some(current) = port.current_record() else {
                return Ok(false);
            };
            Ok(Arc::ptr_eq(&current, &expected) && port.record_is_current(&current))
        }
    }

    #[derive(Debug)]
    struct T4bNodeView<Provider> {
        port: Arc<CliTraderSnapshotPort<Provider>>,
        chain_id: u64,
    }

    impl<Provider> TxAuthorityNodeView for T4bNodeView<Provider>
    where
        Provider: StateProviderFactory
            + HeaderProvider<Header = Header>
            + BlockReaderIdExt
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
    {
        fn chain_id(&self) -> Result<u64, TxAuthorityNodeError> {
            Ok(self.chain_id)
        }

        fn current_parent_hash(&self) -> Result<B256, TxAuthorityNodeError> {
            if let Some(record) = self.port.current_record()
                && self.port.record_is_current(&record)
            {
                return Ok(record.pending.parent_hash());
            }
            self.port
                .provider
                .latest_header()
                .map_err(|_| TxAuthorityNodeError::Unavailable)?
                .map(|header| header.hash())
                .ok_or(TxAuthorityNodeError::Unavailable)
        }

        fn read_state_at_parent(
            &self,
            parent_hash: B256,
            sender: Address,
            contracts: [Address; 4],
        ) -> Result<TxAuthorityStateRead, TxAuthorityNodeError> {
            let state = self
                .port
                .provider
                .state_by_block_hash(parent_hash)
                .map_err(|_| TxAuthorityNodeError::Unavailable)?;
            let committed_sender_nonce = state
                .basic_account(&sender)
                .map_err(|_| TxAuthorityNodeError::Unavailable)?
                .map(|account| account.nonce);
            let mut runtime_codes = Vec::with_capacity(contracts.len());
            for address in contracts {
                let code_hash = state
                    .basic_account(&address)
                    .map_err(|_| TxAuthorityNodeError::Unavailable)?
                    .and_then(|account| account.bytecode_hash)
                    .ok_or(TxAuthorityNodeError::Incoherent)?;
                let code = state
                    .bytecode_by_hash(&code_hash)
                    .map_err(|_| TxAuthorityNodeError::Unavailable)?
                    .map(|bytecode| bytecode.original_bytes());
                runtime_codes.push(code);
            }
            let runtime_codes =
                runtime_codes.try_into().map_err(|_| TxAuthorityNodeError::Incoherent)?;
            Ok(TxAuthorityStateRead::new(parent_hash, committed_sender_nonce, runtime_codes))
        }

        fn is_current_authoritative(
            &self,
            snapshot: &SnapshotHandle,
        ) -> Result<bool, TxAuthorityNodeError> {
            Ok(self.port.is_current_authoritative(snapshot))
        }

        fn capture_snapshot_freshness(
            &self,
            snapshot: &SnapshotHandle,
        ) -> Result<Box<dyn SnapshotFreshnessToken>, TxAuthorityNodeError> {
            let record = self.port.current_record().ok_or(TxAuthorityNodeError::Unavailable)?;
            if !self.port.record_is_current(&record)
                || !snapshot.matches_capture(&record.view, record.received_at)
            {
                return Err(TxAuthorityNodeError::Incoherent);
            }
            Ok(Box::new(CliSnapshotFreshness {
                port: Arc::downgrade(&self.port),
                record: Arc::downgrade(&record),
            }))
        }
    }

    #[derive(Debug)]
    struct T4bShadowAuthority {
        assembler: TxAuthorityAssembler,
        slot: ShadowLatestSlot<ValidatedUnsignedAtomicTx>,
        counters: T4bOutcomeCounters,
    }

    pub(super) const fn outcome(error: TxAuthorityError) -> T4bOutcome {
        match error {
            TxAuthorityError::PlanOrFrameRejected | TxAuthorityError::AssemblyRejected => {
                T4bOutcome::PlanOrFrameRejected
            }
            TxAuthorityError::FeeAuthorityRejected => T4bOutcome::FeeAuthorityRejected,
            TxAuthorityError::RequoteRejected => T4bOutcome::RequoteRejected,
            TxAuthorityError::DeploymentIdentityRejected => T4bOutcome::DeploymentIdentityRejected,
            TxAuthorityError::NonceWitnessUnavailable => T4bOutcome::NonceWitnessUnavailable,
            TxAuthorityError::ObservationBusy => T4bOutcome::ObservationBusy,
            TxAuthorityError::NonceWitnessStaleBeforePublish => {
                T4bOutcome::NonceWitnessStaleBeforePublish
            }
            TxAuthorityError::SnapshotStaleAtDrain => T4bOutcome::SnapshotStaleAtDrain,
            TxAuthorityError::Cancelled => T4bOutcome::Cancelled,
            TxAuthorityError::DeadlineNoShape => T4bOutcome::DeadlineNoShape,
        }
    }

    impl CandidateTxShapeObserver for T4bShadowAuthority {
        fn try_observe(&self, view: CandidateAssemblyView<'_>) -> T4bOutcome {
            let outcome = match self.assembler.assemble_validated(view) {
                Ok(detail) => match self.slot.try_submit(detail) {
                    ShadowSubmit::Accepted => T4bOutcome::SelectedUnsignedShape,
                    ShadowSubmit::DroppedBusy => T4bOutcome::ShadowDroppedBusy,
                    ShadowSubmit::Closed => T4bOutcome::ShadowClosed,
                    ShadowSubmit::ReplacedOldUnobserved => {
                        self.slot.close();
                        T4bOutcome::ShadowClosed
                    }
                },
                Err(error) => outcome(error),
            };
            if outcome != T4bOutcome::SelectedUnsignedShape {
                self.counters.record(outcome);
            }
            outcome
        }

        fn drain_one(&self) {
            let Some(detail) = self.slot.try_take() else {
                return;
            };
            let outcome = if detail.validate_at_drain().is_ok() {
                let observation = detail.observation();
                tracing::debug!(
                    block_number = observation.frame().block_number,
                    predecessor_index = observation.frame().predecessor_index,
                    victim = %observation.victim(),
                    plan_digest = %observation.plan_digest(),
                    sender = %observation.sender(),
                    nonce = observation.nonce(),
                    chain_id = observation.chain_id(),
                    executor = %observation.executor(),
                    protocols = ?observation.hop_protocols(),
                    adapters = ?observation.hop_adapters(),
                    adapter_runtime_hashes = ?observation.hop_runtime_hashes(),
                    gas_limit = observation.gas_limit(),
                    max_fee_per_gas = observation.max_fee_per_gas(),
                    max_priority_fee_per_gas = observation.max_priority_fee_per_gas(),
                    base_fee = observation.base_fee(),
                    valid_until_block = observation.valid_until_block(),
                    unsigned_signing_hash = %observation.unsigned_signing_hash(),
                    "drained T4b unsigned transaction shape"
                );
                T4bOutcome::SelectedUnsignedShape
            } else {
                T4bOutcome::SnapshotStaleAtDrain
            };
            self.counters.record(outcome);
        }

        fn close(&self) {
            let before = self.slot.counters().shutdown_dropped();
            self.slot.close();
            let after = self.slot.counters().shutdown_dropped();
            if after > before {
                self.counters.record(T4bOutcome::ShadowClosed);
            }
        }
    }
    pub(super) fn node_view<Provider>(
        port: Arc<CliTraderSnapshotPort<Provider>>,
        chain_id: u64,
    ) -> Arc<dyn TxAuthorityNodeView>
    where
        Provider: StateProviderFactory
            + HeaderProvider<Header = Header>
            + BlockReaderIdExt
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
    {
        Arc::new(T4bNodeView { port, chain_id })
    }

    pub(super) fn observer<Provider>(
        port: Arc<CliTraderSnapshotPort<Provider>>,
        chain_id: u64,
    ) -> Result<Arc<dyn CandidateTxShapeObserver>, TxAuthorityError>
    where
        Provider: StateProviderFactory
            + HeaderProvider<Header = Header>
            + BlockReaderIdExt
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
    {
        let node = node_view(port, chain_id);
        let assembler = TxAuthorityAssembler::base_mainnet(node)?;
        Ok(Arc::new(T4bShadowAuthority {
            assembler,
            slot: ShadowLatestSlot::new(),
            counters: T4bOutcomeCounters::default(),
        }))
    }
}

#[cfg(feature = "t4d-shadow")]
mod t4d_shadow {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::{
        AdapterAwareProofBindings, BlockReaderIdExt, BridgeError, CandidateAssemblyView,
        CandidateTxShapeObserver, CliTraderSnapshotPort, Debug, Header, HeaderProvider,
        InstalledSubmissionBridge, SealedUnsignedCandidate, ShadowLatestSlot, ShadowSubmit,
        StateProviderFactory, T4bOutcome, T4bOutcomeCounters, TxAuthorityError, t4b_shadow,
    };

    #[derive(Debug, Clone, Copy)]
    pub(super) enum T4dTerminal {
        SealedFresh,
        AssemblyRejected,
        BindingRejected,
        CrossInstallation,
        SnapshotStale,
        ExecutionStale,
        Cancelled,
        DeadlineNoHandoff,
        ShadowBusy,
        ShadowClosed,
    }

    #[derive(Debug, Default)]
    struct T4dTerminalCounters {
        sealed_fresh: AtomicU64,
        assembly_rejected: AtomicU64,
        binding_rejected: AtomicU64,
        cross_installation: AtomicU64,
        snapshot_stale: AtomicU64,
        execution_stale: AtomicU64,
        cancelled: AtomicU64,
        deadline_no_handoff: AtomicU64,
        shadow_busy: AtomicU64,
        shadow_closed: AtomicU64,
    }

    impl T4dTerminalCounters {
        fn record(&self, terminal: T4dTerminal) {
            let counter = match terminal {
                T4dTerminal::SealedFresh => &self.sealed_fresh,
                T4dTerminal::AssemblyRejected => &self.assembly_rejected,
                T4dTerminal::BindingRejected => &self.binding_rejected,
                T4dTerminal::CrossInstallation => &self.cross_installation,
                T4dTerminal::SnapshotStale => &self.snapshot_stale,
                T4dTerminal::ExecutionStale => &self.execution_stale,
                T4dTerminal::Cancelled => &self.cancelled,
                T4dTerminal::DeadlineNoHandoff => &self.deadline_no_handoff,
                T4dTerminal::ShadowBusy => &self.shadow_busy,
                T4dTerminal::ShadowClosed => &self.shadow_closed,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    pub(super) struct T4dShadowAuthority {
        bridge: InstalledSubmissionBridge,
        slot: ShadowLatestSlot<SealedUnsignedCandidate>,
        t4b_counters: T4bOutcomeCounters,
        terminal_counters: T4dTerminalCounters,
    }

    impl T4dShadowAuthority {
        pub(super) const fn bridge_error(error: BridgeError) -> (T4bOutcome, T4dTerminal) {
            match error {
                BridgeError::Assembly(TxAuthorityError::ObservationBusy) => {
                    (T4bOutcome::ObservationBusy, T4dTerminal::ShadowBusy)
                }
                BridgeError::Assembly(error) => {
                    (t4b_shadow::outcome(error), T4dTerminal::AssemblyRejected)
                }
                BridgeError::BindingRejected => {
                    (T4bOutcome::DeploymentIdentityRejected, T4dTerminal::BindingRejected)
                }
                BridgeError::CrossInstallation => {
                    (T4bOutcome::DeploymentIdentityRejected, T4dTerminal::CrossInstallation)
                }
                BridgeError::SnapshotStale => {
                    (T4bOutcome::SnapshotStaleAtDrain, T4dTerminal::SnapshotStale)
                }
                BridgeError::ExecutionFreshnessUnavailable
                | BridgeError::ExecutionIdentityChanged => {
                    (T4bOutcome::DeploymentIdentityRejected, T4dTerminal::ExecutionStale)
                }
                BridgeError::Cancelled => (T4bOutcome::Cancelled, T4dTerminal::Cancelled),
                BridgeError::DeadlineNoHandoff => {
                    (T4bOutcome::DeadlineNoShape, T4dTerminal::DeadlineNoHandoff)
                }
            }
        }

        fn record(&self, outcome: T4bOutcome, terminal: T4dTerminal) {
            self.t4b_counters.record(outcome);
            self.terminal_counters.record(terminal);
        }

        fn observe_bounded_bindings(bindings: &AdapterAwareProofBindings) {
            tracing::debug!(
                bindings = ?bindings,
                "drained T4d bounded bindings"
            );
        }
    }

    impl CandidateTxShapeObserver for T4dShadowAuthority {
        fn try_observe(&self, view: CandidateAssemblyView<'_>) -> T4bOutcome {
            let candidate = match self.bridge.assemble_sealed(view) {
                Ok(candidate) => candidate,
                Err(error) => {
                    let (outcome, terminal) = Self::bridge_error(error);
                    self.record(outcome, terminal);
                    return outcome;
                }
            };
            match self.slot.try_submit(candidate) {
                ShadowSubmit::Accepted => T4bOutcome::SelectedUnsignedShape,
                ShadowSubmit::ReplacedOldUnobserved => {
                    self.slot.close();
                    self.record(T4bOutcome::ShadowClosed, T4dTerminal::ShadowClosed);
                    T4bOutcome::ShadowClosed
                }
                ShadowSubmit::DroppedBusy => {
                    self.record(T4bOutcome::ShadowDroppedBusy, T4dTerminal::ShadowBusy);
                    T4bOutcome::ShadowDroppedBusy
                }
                ShadowSubmit::Closed => {
                    self.record(T4bOutcome::ShadowClosed, T4dTerminal::ShadowClosed);
                    T4bOutcome::ShadowClosed
                }
            }
        }

        fn drain_one(&self) {
            let Some(candidate) = self.slot.try_take() else {
                return;
            };
            match self.bridge.revalidate_for_handoff(&candidate) {
                Ok(bindings) => {
                    Self::observe_bounded_bindings(bindings);
                    self.record(T4bOutcome::SelectedUnsignedShape, T4dTerminal::SealedFresh);
                }
                Err(error) => {
                    let (outcome, terminal) = Self::bridge_error(error);
                    self.record(outcome, terminal);
                }
            }
        }

        fn close(&self) {
            let before = self.slot.counters().shutdown_dropped();
            self.slot.close();
            let after = self.slot.counters().shutdown_dropped();
            if after > before {
                self.record(T4bOutcome::ShadowClosed, T4dTerminal::ShadowClosed);
            }
        }
    }

    pub(super) fn observer<Provider>(
        port: Arc<CliTraderSnapshotPort<Provider>>,
        chain_id: u64,
    ) -> Result<Arc<dyn CandidateTxShapeObserver>, BridgeError>
    where
        Provider: StateProviderFactory
            + HeaderProvider<Header = Header>
            + BlockReaderIdExt
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
    {
        let node = t4b_shadow::node_view(port, chain_id);
        let bridge = InstalledSubmissionBridge::base_mainnet(node)?;
        Ok(Arc::new(T4dShadowAuthority {
            bridge,
            slot: ShadowLatestSlot::new(),
            t4b_counters: T4bOutcomeCounters::default(),
            terminal_counters: T4dTerminalCounters::default(),
        }))
    }
}
fn t4a_runtime_config(enabled: bool) -> eyre::Result<MevTraderRuntimeConfig> {
    #[cfg(feature = "t4a-shadow")]
    if enabled {
        return t4a_provisioning::runtime_config();
    }
    let _ = enabled;
    MevTraderRuntimeConfig::empty().map_err(Into::into)
}
#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bloom, Bytes, U256};
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_flashblocks::PendingBlocksBuilder;

    use super::*;

    fn pending_blocks() -> PendingBlocks {
        let parent_hash = B256::with_last_byte(1);
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([Flashblock {
            payload_id: Default::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 100,
                gas_limit: 30_000_000,
                timestamp: 1,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::with_last_byte(2),
                transactions: Vec::new(),
                withdrawals: Vec::new(),
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(100),
        }]);
        builder.with_header(Sealed::new_unchecked(
            Header { parent_hash, number: 100, ..Default::default() },
            B256::with_last_byte(2),
        ));
        builder.build().expect("pending blocks")
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_measurement_installs_before_lookup_and_terminalizes_every_receive_branch() {
        const CLI_MANIFEST: &str = include_str!("../Cargo.toml");
        const CLI_SOURCE: &str = include_str!("mev_trader.rs");
        const SNAPSHOT_TASK: &str = concat!("let snapshot_handle = tokio::", "spawn");

        let receiver_block = CLI_SOURCE
            .split_once(SNAPSHOT_TASK)
            .and_then(|(_, source)| source.split_once("let consumer_runtime"))
            .map(|(source, _)| source)
            .expect("isolated snapshot receiver block");
        let installs = receiver_block
            .match_indices("port.record_pending_snapshot(")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let lookups = receiver_block
            .match_indices(".cli_received(")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(installs.len(), 2);
        assert_eq!(lookups.len(), installs.len());
        assert!(
            installs.iter().zip(&lookups).all(|(install, lookup)| install < lookup),
            "snapshot install must precede registry lookup on every receive path"
        );
        for terminal in ["cli_lagged(count)", "cli_closed()", "cli_cancelled()"] {
            assert!(receiver_block.contains(terminal), "missing receiver terminal: {terminal}");
        }

        let feature = CLI_MANIFEST
            .split_once("edge-measurement = [")
            .and_then(|(_, manifest)| manifest.split_once(']'))
            .map(|(feature, _)| feature)
            .expect("edge measurement CLI feature");
        assert!(feature.contains("\"base-flashblocks/edge-measurement\""));
        assert!(feature.contains("\"base-mev-trader/edge-measurement\""));
        for forbidden in ["mev-trader-submit", "signer", "submission", "arm", "egress"] {
            assert!(
                !feature.contains(forbidden),
                "forbidden measurement feature edge: {forbidden}"
            );
        }
    }

    #[test]
    fn t4a_selected_closure_remains_measurement_only_zero_capability() {
        const CLI_MANIFEST: &str = include_str!("../Cargo.toml");
        const NODE_MANIFEST: &str = include_str!("../../../../bin/node/Cargo.toml");
        const TRADER_MANIFEST: &str = include_str!("../../mev-trader/Cargo.toml");
        const CLI_SOURCE: &str = include_str!("mev_trader.rs");
        const TASK_SPAWN: &str = concat!("tokio", "::spawn");
        const FLASHBLOCK_SUBSCRIBE: &str = concat!("subscribe_to_", "flashblocks");

        let cli_feature = CLI_MANIFEST
            .split_once("t4a-shadow = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(feature, _)| feature)
            .expect("CLI t4a-shadow feature");
        let selected_members = cli_feature
            .split(',')
            .map(|member| member.trim().trim_matches('"'))
            .filter(|member| !member.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            selected_members,
            ["base-mev-trader/t4a-shadow", "dep:libc", "dep:serde", "dep:serde_json",]
        );
        assert!(cli_feature.contains("\"base-mev-trader/t4a-shadow\""));
        for forbidden in ["mev-trader-submit", "reqwest", "signer", "assembly", "arm", "egress"] {
            assert!(
                !cli_feature.contains(forbidden),
                "forbidden T4a CLI feature edge: {forbidden}"
            );
        }

        assert!(
            NODE_MANIFEST.contains("t4a-shadow = [ \"base-execution-cli/t4a-shadow\" ]"),
            "node must forward only the T4a measurement feature"
        );
        assert!(
            TRADER_MANIFEST.contains("t4a-shadow = []"),
            "mev-trader T4a leaf feature must add no dependency edge"
        );

        let provisioning = CLI_SOURCE
            .split_once("#[cfg(feature = \"t4a-shadow\")]\nmod t4a_provisioning")
            .and_then(|(_, rest)| rest.split_once("\nfn t4a_runtime_config"))
            .map(|(source, _)| source)
            .expect("isolated T4a provisioning source");
        for forbidden in [
            "send_gated(",
            "mev-trader-submit",
            "reqwest::",
            "signer.",
            TASK_SPAWN,
            FLASHBLOCK_SUBSCRIBE,
        ] {
            assert!(
                !provisioning.contains(forbidden),
                "forbidden T4a provisioning seam: {forbidden}"
            );
        }
        assert_eq!(CLI_SOURCE.matches(TASK_SPAWN).count(), 5);
        assert_eq!(CLI_SOURCE.matches(FLASHBLOCK_SUBSCRIBE).count(), 1);
    }

    #[test]
    fn adapter_requires_live_some_and_pointer_identical_capture() {
        let flashblocks = Arc::new(FlashblocksState::default());
        flashblocks.set_pending_blocks_for_testing(Some(pending_blocks()));
        let current = flashblocks.get_pending_blocks();
        let captured = Arc::clone(current.as_ref().expect("current pending"));
        drop(current);

        let port = CliTraderSnapshotPort::new(Arc::clone(&flashblocks), ());
        port.record_pending_snapshot(captured, Instant::now());
        let record = port.current_record().expect("captured record");
        assert!(port.record_is_current(&record));

        flashblocks.set_pending_blocks_for_testing(None);
        assert!(!port.record_is_current(&record));
    }
    #[cfg(feature = "t4b-shadow")]
    #[test]
    fn t4b_shadow_slot_is_capacity_one_nonblocking_and_releases_every_guard() {
        use std::sync::atomic::{AtomicU64, Ordering};

        #[derive(Debug)]
        struct DropProbe(Arc<AtomicU64>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicU64::new(0));
        let slot = ShadowLatestSlot::new();
        assert_eq!(slot.try_submit(DropProbe(Arc::clone(&drops))), ShadowSubmit::Accepted);
        assert_eq!(
            slot.try_submit(DropProbe(Arc::clone(&drops))),
            ShadowSubmit::ReplacedOldUnobserved
        );
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(slot.try_take().expect("capacity-one detail"));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(slot.try_submit(DropProbe(Arc::clone(&drops))), ShadowSubmit::Accepted);
        slot.close();
        assert_eq!(drops.load(Ordering::Relaxed), 3);
        assert_eq!(slot.try_submit(DropProbe(Arc::clone(&drops))), ShadowSubmit::Closed);
        assert_eq!(drops.load(Ordering::Relaxed), 4);
    }
    #[cfg(feature = "t4d-shadow")]
    #[test]
    fn t4d_shadow_drain_observes_only_bounded_bindings_and_drops_linear_candidate() {
        use std::sync::atomic::{AtomicU64, Ordering};

        #[derive(Debug)]
        struct LinearCandidate(Arc<AtomicU64>);

        impl Drop for LinearCandidate {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        const CLI_MANIFEST: &str = include_str!("../Cargo.toml");
        const NODE_MANIFEST: &str = include_str!("../../../../bin/node/Cargo.toml");
        const CLI_SOURCE: &str = include_str!("mev_trader.rs");

        let cli_feature = CLI_MANIFEST
            .split_once("t4d-shadow = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(feature, _)| feature)
            .expect("CLI t4d-shadow feature");
        assert_eq!(
            cli_feature
                .split(',')
                .map(|member| member.trim().trim_matches('"'))
                .filter(|member| !member.is_empty())
                .collect::<Vec<_>>(),
            ["t4b-shadow", "mev-trader-submit/t4d-bridge"]
        );
        assert!(NODE_MANIFEST.contains("t4d-shadow = [ \"base-execution-cli/t4d-shadow\" ]"));

        let bounded_observation = CLI_SOURCE
            .split_once("fn observe_bounded_bindings")
            .and_then(|(_, rest)| rest.split_once("\n        }\n"))
            .map(|(source, _)| source)
            .expect("bounded T4d drain observation");
        assert!(bounded_observation.contains("bindings = ?bindings"));
        for forbidden in ["candidate", "unsigned_tx", "calldata", "raw", "input"] {
            assert!(
                !bounded_observation.contains(forbidden),
                "T4d drain observation exposed forbidden detail: {forbidden}"
            );
        }

        assert!(matches!(
            t4d_shadow::T4dShadowAuthority::bridge_error(BridgeError::Assembly(
                TxAuthorityError::ObservationBusy
            )),
            (T4bOutcome::ObservationBusy, t4d_shadow::T4dTerminal::ShadowBusy)
        ));

        let drops = Arc::new(AtomicU64::new(0));
        let slot = ShadowLatestSlot::new();
        assert_eq!(slot.try_submit(LinearCandidate(Arc::clone(&drops))), ShadowSubmit::Accepted);
        let candidate = slot.try_take().expect("linear sealed candidate");
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(candidate);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(slot.try_take().is_none());

        assert_eq!(slot.try_submit(LinearCandidate(Arc::clone(&drops))), ShadowSubmit::Accepted);
        slot.close();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_config_parser_rejects_duplicate_unknown_zero_and_malformed_values() {
        assert!(FlatJsonObjectV1::parse(r#"{"producerEpoch":1,"producerEpoch":2}"#).is_err());
        let unknown = FlatJsonObjectV1::parse(r#"{"unknown":1}"#).expect("flat JSON");
        assert!(!unknown.0.contains_key("producerEpoch"));
        let zero = FlatJsonObjectV1::parse(r#"{"capacity":0}"#).expect("flat JSON");
        assert!(zero.capacity("capacity").is_err());
        let malformed = FlatJsonObjectV1::parse(r#"{"digest":"0x01"}"#).expect("flat JSON");
        assert!(malformed.digest("digest").is_err());
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_config_parser_accepts_only_the_complete_strict_schema() {
        let digest = format!("0x{}", "11".repeat(32));
        let address = format!("0x{}", "22".repeat(20));
        let text = format!(
            concat!(
                "{{\"outputRoot\":\"/tmp/edge\",\"producerEpoch\":1,",
                "\"producerDigest\":\"{0}\",\"rejectSchemaDigest\":\"{0}\",",
                "\"preregDigest\":\"{0}\",\"policyDigest\":\"{0}\",",
                "\"configDigest\":\"{0}\",\"ownerApprovalReceiptDigest\":\"{0}\",",
                "\"flashEventCapacity\":1,\"flashActiveCapacity\":1,",
                "\"flashRegistryCapacity\":1,\"blinkRecordCapacity\":1,",
                "\"blinkCandidateCapacity\":1,\"measurementSender\":\"{1}\",",
                "\"executorRuntimeHash\":\"{0}\",\"v2Adapter\":\"{1}\",",
                "\"v2AdapterRuntimeHash\":\"{0}\",\"v3Adapter\":\"{1}\",",
                "\"v3AdapterRuntimeHash\":\"{0}\",\"aerodromeAdapter\":\"{1}\",",
                "\"aerodromeAdapterRuntimeHash\":\"{0}\",\"g0CodeIdentityDigest\":\"{0}\",",
                "\"rawRejectInventorySha256\":\"{0}\",\"rawRejectSourceSha256\":\"{0}\",",
                "\"measurementTxSourceSha256\":\"{0}\"}}"
            ),
            digest, address,
        );
        let values = FlatJsonObjectV1::parse(&text).expect("strict JSON");
        assert_eq!(values.0.len(), 25);
        assert_eq!(values.u64("producerEpoch"), Ok(1));
        assert!(!values.digest("producerDigest").expect("digest").is_zero());
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_authority_files_are_descriptor_pinned_and_owner_private() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!(
            "base-edge-config-security-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("test root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private root");

        let config = root.join("config.json");
        fs::write(&config, b"{}").expect("config");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o660)).expect("writable config");
        assert!(EdgeCliProducerConfigV1::open_validated_config(&config).is_err());
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("private config");
        assert!(EdgeCliProducerConfigV1::open_validated_config(&config).is_ok());

        let output = root.join("output");
        let pinned =
            EdgeCliProducerConfigV1::open_validated_output_root(&output).expect("pinned output");
        assert!(output.is_dir());
        assert_eq!(pinned.metadata().expect("output metadata").mode() & 0o077, 0);
        drop(pinned);

        let redirect = root.join("redirect");
        symlink(&output, &redirect).expect("output symlink");
        assert!(EdgeCliProducerConfigV1::open_validated_output_root(&redirect).is_err());

        fs::remove_dir_all(&root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn registry_h1_is_exact_and_h2_is_replayable_without_process_identity() {
        let terminal = PendingTerminalRecordV2 {
            coverage_sequence: 0,
            metadata: base_flashblocks::PendingSnapshotMetadataV2 {
                identity: base_flashblocks::PendingSnapshotIdentityV2 {
                    producer_epoch: 9,
                    pending_snapshot_sequence: 4,
                    arc_pointer_identity: usize::MAX,
                },
                source_generation: Some(3),
                pending_public_subset_digest_v1: B256::with_last_byte(7),
            },
            registration: base_flashblocks::PendingRegistrationDispositionV2::Failed(
                base_flashblocks::PendingRegistrationFailure::PendingAccountingOverflow(
                    base_flashblocks::PendingAccountingFieldV2::SendPublished,
                ),
            ),
            send: base_flashblocks::PendingSendDispositionV2::Published { receiver_count: 2 },
            terminal: base_flashblocks::PendingCliTerminalV2::CliRegistryLookupFailed(
                base_flashblocks::CliRegistryLookupFailureReason::RegistrationFailed(
                    base_flashblocks::PendingRegistrationFailure::PendingRegistryCapacityOverflow,
                ),
            ),
        };

        let h1 = EdgeCanonicalWriterV1::canonical_bytes(&EdgeCanonicalWriterV1::registry_h1_value(
            terminal,
        ))
        .expect("H1 canonical bytes");
        assert_eq!(
            h1,
            format!(
                "{{\"pendingPublicSubsetDigestV1\":\"{}\",\"pendingSnapshotSequence\":\"4\"}}",
                "00".repeat(31) + "07"
            )
            .into_bytes()
        );

        let h2 = EdgeCanonicalWriterV1::registry_h2_value(terminal);
        assert_eq!(h2["coverageSequence"], "0");
        assert_eq!(h2["pendingSnapshotSequence"], "4");
        assert_eq!(h2["sourceGeneration"], "3");
        assert_eq!(h2["send"]["receiverCount"], "2");
        assert_eq!(h2["registration"]["failure"]["accountingField"], "SendPublished");
        assert_eq!(
            h2["terminal"]["failure"]["registrationFailure"]["reason"],
            "PendingRegistryCapacityOverflow"
        );
        let h2_bytes = EdgeCanonicalWriterV1::canonical_bytes(&h2).expect("H2 canonical bytes");
        assert!(!h2_bytes.windows(10).any(|window| window == b"arcPointer"));
        assert!(!h2_bytes.windows(4).any(|window| window == b"Weak"));
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn cutoff_drain_deadline_is_deterministic_and_finite() {
        let recorder = EdgeMeasurementRecorderV1::new(EdgeMeasurementInstallConfigV1 {
            producer_epoch: NonZeroU64::new(77).expect("nonzero epoch"),
            event_queue_capacity: 8,
            active_state_capacity: 8,
            pending_registry_capacity: 8,
            terminal_record_capacity: 8,
        })
        .expect("Linux recorder");
        assert!(!await_edge_cutoff_drain(&recorder, Instant::now()));
    }
    #[cfg(feature = "edge-measurement")]
    #[tokio::test]
    async fn hung_shutdown_join_timeout_continues_to_finals() {
        let (mut writer, root) = edge_test_writer("hung-shutdown-join");
        seal_empty_test_writer(&mut writer);

        let handle = tokio::task::spawn(std::future::pending::<()>());
        let outcome = await_edge_shutdown_task(handle, Duration::ZERO).await;
        assert!(matches!(outcome, EdgeShutdownTaskOutcome::TimedOut));
        writer.recorder.latch_coordinator_failure("SyntheticShutdownTaskJoinTimeout");
        writer.run().expect("writer finalization after shutdown timeout");

        let health: JsonValue = serde_json::from_slice(
            &fs::read(root.join("producer-health-final-v1.json")).expect("health final"),
        )
        .expect("health JSON");
        assert_eq!(
            health["coordinatorMissingEvidenceCounts"]["SyntheticShutdownTaskJoinTimeout"],
            "1"
        );
        assert!(root.join("producer-manifest-v1.json").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    fn edge_test_writer(label: &str) -> (EdgeCanonicalWriterV1, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "base-edge-writer-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos(),
        ));
        let output_root_handle =
            EdgeCliProducerConfigV1::open_validated_output_root(&root).expect("test output");
        let digest = B256::with_last_byte(1);
        let measurement_sender = Address::with_last_byte(1);
        let v2_adapter = Address::with_last_byte(2);
        let v3_adapter = Address::with_last_byte(3);
        let aerodrome_adapter = Address::with_last_byte(4);
        let executor_runtime_hash = alloy_primitives::keccak256([]);
        let v2_adapter_runtime_hash = executor_runtime_hash;
        let v3_adapter_runtime_hash = executor_runtime_hash;
        let aerodrome_adapter_runtime_hash = executor_runtime_hash;
        let mut identity_bytes = b"edge-deployment-identities/v1\0".to_vec();
        identity_bytes.extend_from_slice(&4_u32.to_be_bytes());
        for (address, runtime_hash) in [
            (base_mev_trader::MEASUREMENT_EXECUTOR, executor_runtime_hash),
            (v2_adapter, v2_adapter_runtime_hash),
            (v3_adapter, v3_adapter_runtime_hash),
            (aerodrome_adapter, aerodrome_adapter_runtime_hash),
        ] {
            identity_bytes.extend_from_slice(address.as_slice());
            identity_bytes.extend_from_slice(runtime_hash.as_slice());
        }
        let g0_code_identity_digest =
            B256::new(EdgeMeasurementDurabilityV1::sha256(&identity_bytes));
        let reject_schema_digest = base_mev_trader::BlinkRejectClassifierV3::reject_schema_digest();
        let raw_reject_inventory_sha256 =
            EdgeMeasurementOwnerConfigV1::raw_reject_inventory_sha256();
        let raw_reject_source_sha256 = B256::new(EdgeMeasurementDurabilityV1::sha256(
            include_bytes!("../../mev-trader/src/edge_measurement.rs"),
        ));
        let measurement_tx_source_sha256 = B256::new(EdgeMeasurementDurabilityV1::sha256(
            include_bytes!("../../mev-trader/src/measurement_tx.rs"),
        ));
        let config = EdgeCliProducerConfigV1 {
            output_root: root.clone(),
            output_root_handle: Arc::clone(&output_root_handle),
            producer_epoch: NonZeroU64::new(9).expect("nonzero"),
            producer_digest: digest,
            reject_schema_digest,
            prereg_digest: digest,
            policy_digest: digest,
            config_digest: digest,
            owner_approval_receipt_digest: digest,
            flash_event_capacity: 2048,
            flash_active_capacity: 2048,
            flash_registry_capacity: 2048,
            blink_record_capacity: 2048,
            blink_candidate_capacity: 2048,
            measurement_sender,
            executor_runtime_hash,
            v2_adapter,
            v2_adapter_runtime_hash,
            v3_adapter,
            v3_adapter_runtime_hash,
            aerodrome_adapter,
            aerodrome_adapter_runtime_hash,
            g0_code_identity_digest,
            raw_reject_inventory_sha256,
            raw_reject_source_sha256,
            measurement_tx_source_sha256,
        };
        let recorder =
            EdgeMeasurementRecorderV1::new_deterministic_test(EdgeMeasurementInstallConfigV1 {
                producer_epoch: config.producer_epoch,
                event_queue_capacity: config.flash_event_capacity,
                active_state_capacity: config.flash_active_capacity,
                pending_registry_capacity: config.flash_registry_capacity,
                terminal_record_capacity: PENDING_TERMINAL_RECORD_CAPACITY_MAX_V2,
            })
            .expect("test recorder");
        let owner = EdgeMeasurementOwnerV1::new(EdgeMeasurementOwnerConfigV1 {
            producer_epoch: config.producer_epoch.get(),
            output_root: config.output_root.clone(),
            output_root_handle,
            producer_digest: config.producer_digest,
            reject_schema_digest: config.reject_schema_digest,
            prereg_digest: config.prereg_digest,
            policy_digest: config.policy_digest,
            config_digest: config.config_digest,
            owner_approval_receipt_digest: config.owner_approval_receipt_digest,
            record_queue_capacity: config.blink_record_capacity,
            candidate_queue_capacity: config.blink_candidate_capacity,
            measurement_sender: config.measurement_sender,
            executor_runtime_hash: config.executor_runtime_hash,
            v2_adapter: config.v2_adapter,
            v2_adapter_runtime_hash: config.v2_adapter_runtime_hash,
            v3_adapter: config.v3_adapter,
            v3_adapter_runtime_hash: config.v3_adapter_runtime_hash,
            aerodrome_adapter: config.aerodrome_adapter,
            aerodrome_adapter_runtime_hash: config.aerodrome_adapter_runtime_hash,
            g0_code_identity_digest: config.g0_code_identity_digest,
            raw_reject_inventory_sha256: config.raw_reject_inventory_sha256,
            raw_reject_source_sha256: config.raw_reject_source_sha256,
            measurement_tx_source_sha256: config.measurement_tx_source_sha256,
        })
        .expect("test owner");
        (EdgeCanonicalWriterV1::new(config, recorder, owner), root)
    }
    #[cfg(feature = "edge-measurement")]
    fn seal_empty_test_writer(writer: &mut EdgeCanonicalWriterV1) {
        loop {
            for ledger in writer.ledgers.values_mut() {
                if !ledger.records.is_empty() {
                    ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
                }
            }
            if !writer.drain_once().expect("startup drain") {
                break;
            }
        }
        writer.recorder.prepare_cutoff();
        let (blink_count, candidate_bounds) = writer.owner.prepare_cutoff().expect("Blink cutoff");
        assert_eq!(blink_count, 0);
        assert_eq!(candidate_bounds.count, 0);
        let cutoff = writer.recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        writer.owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
            producer_epoch: cutoff.producer_epoch,
            cutoff_clock_observation_ordinal: cutoff.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: cutoff.last_admitted_wire_ordinal,
            last_admitted_source_generation: cutoff.last_admitted_source_generation,
            last_admitted_blink_generation: cutoff.last_admitted_blink_generation,
            last_pending_snapshot_sequence: cutoff.last_pending_snapshot_sequence,
            last_coverage_sequence: cutoff.last_coverage_sequence,
            last_candidate_sequence: cutoff.last_candidate_sequence,
            latch_mono_ns: cutoff.latch_mono_ns,
        });
        while writer.drain_once().expect("cutoff drain") {}
    }

    #[cfg(feature = "edge-measurement")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn campaign_interruption_awaits_fired_cutoff_and_preserves_finals() {
        let (mut writer, root) = edge_test_writer("campaign-interruption");
        writer
            .recorder
            .connection_transition(base_flashblocks::SourceConnectionTransitionV1::OwnerStart);
        writer.recorder.connection_transition(
            base_flashblocks::SourceConnectionTransitionV1::InitialConnectAttemptStarted,
        );
        writer
            .recorder
            .connection_transition(base_flashblocks::SourceConnectionTransitionV1::Established);
        let rejected = writer
            .recorder
            .observe_wire(b"campaign-interruption-rejected")
            .expect("interruption wire");
        writer.recorder.decode_rejected(rejected);
        let latched = Arc::new(EdgeCutoffLatchV1::default());
        let task_latched = Arc::clone(&latched);
        let recorder = Arc::clone(&writer.recorder);
        let owner = Arc::clone(&writer.owner);
        let cutoff_handle = tokio::task::spawn(async move {
            tokio::task::yield_now().await;
            latch_edge_cutoff_once(&task_latched, &recorder, &owner);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !latched.started.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "cutoff task did not start");
            tokio::task::yield_now().await;
        }
        let cutoff_join = tokio::task::spawn(abort_and_await_edge_cutoff_task(Some(cutoff_handle)));
        while !cutoff_join.is_finished() {
            writer.drain_once().expect("campaign interruption drain");
            for ledger in writer.ledgers.values_mut() {
                if !ledger.records.is_empty() {
                    ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
                }
            }
            assert!(Instant::now() < deadline, "in-progress cutoff task did not finish");
            tokio::task::yield_now().await;
        }
        cutoff_join.await.expect("cutoff join observer");
        assert!(latched.completed.load(Ordering::Acquire));

        loop {
            for ledger in writer.ledgers.values_mut() {
                if !ledger.records.is_empty() {
                    ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
                }
            }
            if !writer.drain_once().expect("post-interruption durable drain") {
                break;
            }
        }
        writer.recorder.verify_source_final().expect("source final");
        assert!(writer.owner.finalization_ready().expect("Blink readiness"));
        writer.finalize().expect("campaign interruption finalization");

        let blink: JsonValue = serde_json::from_slice(
            &fs::read(root.join("blink-final-v1.json")).expect("Blink final artifact"),
        )
        .expect("Blink final JSON");
        assert_eq!(blink["schemaVersion"], "edge-blink-final-v1");
        assert!(blink.get("candidateBounds").is_some());
        assert!(blink.get("candidate_bounds").is_none());
        assert!(blink["blink"].get("victimIngressObserved").is_some());
        assert!(blink["blink"].get("victim_ingress_observed").is_none());
        assert!(root.join("source-final-v1.json").is_file());
        assert!(root.join("producer-manifest-v1.json").is_file());

        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn blink_terminal_completing_after_cutoff_fence_reaches_final() {
        let (writer, root) = edge_test_writer("blink-cutoff-late-terminal");
        writer.owner.with_blink_admission(|owner, authoritative| {
            assert!(authoritative);
            owner.observe_ledger_result_admitted(owner.ledger().record_observed());
            owner.record_submission_admitted(0, base_mev_trader::SlotSubmit::Accepted);
        });
        let cutoff_owner = Arc::clone(&writer.owner);
        let cutoff = std::thread::spawn(move || cutoff_owner.prepare_cutoff());
        while writer.owner.is_accepting() {
            std::thread::yield_now();
        }
        writer.owner.record_terminal_and_resolve(
            0,
            base_mev_trader::BlinkGenerationTerminalV1::Processed,
            None,
        );
        let (blink_count, candidate_bounds) =
            cutoff.join().expect("cutoff thread").expect("late terminal cutoff");
        assert_eq!(blink_count, 1);
        assert_eq!(candidate_bounds.count, 0);
        writer.owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
            producer_epoch: 9,
            cutoff_clock_observation_ordinal: 0,
            last_admitted_wire_ordinal: 0,
            last_admitted_source_generation: 0,
            last_admitted_blink_generation: 0,
            last_pending_snapshot_sequence: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
            latch_mono_ns: 1,
        });
        let final_record = writer.owner.final_record().expect("Blink final");
        assert_eq!(final_record.candidate_bounds.count, 0);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn valid_empty_authority_coverage_finalizes_explicitly_without_a_ledger() {
        let (mut writer, root) = edge_test_writer("empty-coverage-final");
        loop {
            for ledger in writer.ledgers.values_mut() {
                if !ledger.records.is_empty() {
                    ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
                }
            }
            if !writer.drain_once().expect("empty campaign startup drain") {
                break;
            }
        }
        writer.recorder.prepare_cutoff();
        let (blink_count, candidate_bounds) = writer.owner.prepare_cutoff().expect("Blink cutoff");
        assert_eq!(blink_count, 0);
        assert_eq!(candidate_bounds.count, 0);
        assert!(writer.recorder.cutoff_drain_complete());
        let cutoff = writer.recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        writer.owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
            producer_epoch: cutoff.producer_epoch,
            cutoff_clock_observation_ordinal: cutoff.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: cutoff.last_admitted_wire_ordinal,
            last_admitted_source_generation: cutoff.last_admitted_source_generation,
            last_admitted_blink_generation: cutoff.last_admitted_blink_generation,
            last_pending_snapshot_sequence: cutoff.last_pending_snapshot_sequence,
            last_coverage_sequence: cutoff.last_coverage_sequence,
            last_candidate_sequence: cutoff.last_candidate_sequence,
            latch_mono_ns: cutoff.latch_mono_ns,
        });
        while writer.drain_once().expect("empty durable drain") {}
        assert!(!writer.ledgers.contains_key("coverage"));
        assert!(writer.owner.finalization_ready().expect("Blink readiness"));
        writer.finalize().expect("empty coverage finalization");

        let coverage: JsonValue = serde_json::from_slice(
            &fs::read(root.join("source-coverage-final-v1.json")).expect("coverage final"),
        )
        .expect("coverage final JSON");
        assert_eq!(coverage["coverageEmpty"], true);
        assert_eq!(coverage["coverageCount"], "0");
        assert_eq!(coverage["lastSequenceInclusive"], JsonValue::Null);
        assert_eq!(coverage["finalRecordHash"], EDGE_ZERO_HASH);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn timer_cutoff_finalizes_without_shutdown_stop() {
        let (writer, root) = edge_test_writer("timer-cutoff");
        let recorder = Arc::clone(&writer.recorder);
        let owner = Arc::clone(&writer.owner);
        let handle = std::thread::spawn(move || writer.run());

        let latched = EdgeCutoffLatchV1::default();
        latch_edge_cutoff_once(&latched, &recorder, &owner);
        handle.join().expect("writer join").expect("writer finalization");

        assert!(root.join("producer-health-final-v1.json").is_file());
        assert!(root.join("producer-manifest-v1.json").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn missing_evidence_counter_mutation_does_not_abort_finalize() {
        let (mut writer, root) = edge_test_writer("missing-evidence-mutation");
        seal_empty_test_writer(&mut writer);

        let recorder = Arc::clone(&writer.recorder);
        let mutating = Arc::new(AtomicBool::new(true));
        let mutation_started = Arc::new(AtomicBool::new(false));
        let thread_started = Arc::clone(&mutation_started);
        let mutating_thread = Arc::clone(&mutating);
        let mutation_handle = std::thread::spawn(move || {
            while mutating_thread.load(Ordering::Acquire) {
                recorder.latch_coordinator_failure("SyntheticCounterMutationDuringFinalize");
                thread_started.store(true, Ordering::Release);
                std::thread::yield_now();
            }
        });
        while !mutation_started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let result = writer.finalize();
        mutating.store(false, Ordering::Release);
        mutation_handle.join().expect("mutation join");

        result.expect("counter mutation must not abort finalization");
        assert!(root.join("producer-manifest-v1.json").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn finalize_succeeds_with_independent_registry_and_source_cursors() {
        let (mut writer, root) = edge_test_writer("finalize-success");
        let flashblock = pending_blocks().get_flashblocks().remove(0);
        let admission = writer.recorder.observe_wire(b"finalize").expect("wire admission");
        let generation =
            writer.recorder.decoded_flashblock(admission, &flashblock).expect("source generation");
        writer.recorder.actor_enqueue(generation, false);

        writer.recorder.prepare_cutoff();
        let (blink_count, candidate_bounds) = writer.owner.prepare_cutoff().expect("Blink cutoff");
        assert_eq!(blink_count, 0);
        assert_eq!(candidate_bounds.count, 0);
        assert!(writer.drain_once().expect("buffer source evidence"));
        for ledger in writer.ledgers.values_mut() {
            if !ledger.records.is_empty() {
                ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
            }
        }
        writer.drain_once().expect("flush source evidence");
        assert!(writer.recorder.cutoff_drain_complete());
        let cutoff = writer.recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 0,
            last_candidate_sequence: 0,
        });
        writer.owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
            producer_epoch: cutoff.producer_epoch,
            cutoff_clock_observation_ordinal: cutoff.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: cutoff.last_admitted_wire_ordinal,
            last_admitted_source_generation: cutoff.last_admitted_source_generation,
            last_admitted_blink_generation: cutoff.last_admitted_blink_generation,
            last_pending_snapshot_sequence: cutoff.last_pending_snapshot_sequence,
            last_coverage_sequence: cutoff.last_coverage_sequence,
            last_candidate_sequence: cutoff.last_candidate_sequence,
            latch_mono_ns: cutoff.latch_mono_ns,
        });

        while writer.drain_once().expect("durable drain") {}
        writer.recorder.verify_source_final().expect("source final");
        assert!(writer.owner.finalization_ready().expect("Blink readiness"));
        writer.finalize().expect("successful finalization");
        assert!(writer.finalized);
        assert!(root.join("source-final-v1.json").is_file());
        assert!(root.join("registry-final-v1.json").is_file());
        assert!(root.join("candidate-detail-final-v1.json").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn registry_final_uses_exact_non_authority_send_count_beyond_diagnostic_ring() {
        let (writer, root) = edge_test_writer("exact-nonauthority-count");
        let registry = writer.recorder.registry();
        for _ in 0..100 {
            registry.begin_unregistered_send();
            registry
                .record_unregistered_send(
                    base_flashblocks::PendingSendJournalMarkerV2::PassthroughNonAdvanced,
                    None,
                )
                .expect("non-authority disposition");
        }

        let summary = registry.final_summary();
        assert_eq!(EdgeCanonicalWriterV1::registry_final_non_authority_send_count(&summary), 100);
        assert!(registry.snapshot().unregistered_send_records.len() < 100);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn rolling_descriptor_and_artifact_indexes_have_no_campaign_hard_cap() {
        const FORMER_DESCRIPTOR_CAPACITY: usize = 65_814;
        let (mut writer, root) = edge_test_writer("uncapped-segment-index");
        let descriptor = EdgeSegmentDescriptorV1 {
            filename: String::new(),
            first_sequence: String::new(),
            last_sequence: String::new(),
            record_count: String::new(),
            segment_seal_sha256: String::new(),
            file_sha256: String::new(),
        };
        let ledger = writer
            .ledgers
            .entry("source-detail")
            .or_insert_with(|| EdgeRollingLedgerV1::new("source-detail", "edge-source-detail/v1"));
        ledger.descriptors.resize(FORMER_DESCRIPTOR_CAPACITY + 1, descriptor);
        for index in 0..=FORMER_DESCRIPTOR_CAPACITY {
            writer
                .persisted_artifacts
                .insert(format!("prior-{index:020}.ndjson"), EDGE_ZERO_HASH.to_owned());
        }
        writer
            .push_rolling_record(
                "source-detail",
                "edge-source-detail/v1",
                "Observed",
                json!({"schema": "edge-source-detail/v1"}),
            )
            .expect("record beyond former cap");
        writer
            .flush_rolling_ledger("source-detail")
            .expect("flush beyond former cap")
            .expect("sealed segment beyond former cap");
        assert_eq!(
            writer.ledgers["source-detail"].descriptors.len(),
            FORMER_DESCRIPTOR_CAPACITY + 2
        );
        assert_eq!(writer.persisted_artifacts.len(), FORMER_DESCRIPTOR_CAPACITY + 2);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn candidate_join_validation_releases_more_than_total_capacity_incrementally() {
        let (mut writer, root) = edge_test_writer("candidate-join-release");
        let connection_hash = B256::with_last_byte(1);
        let registry_hash = B256::with_last_byte(2);
        writer.connection_records.push(connection_hash);
        writer.connection_segment_sha.push((0, B256::with_last_byte(3)));
        writer.registry_segment_sha.push(B256::with_last_byte(4));
        writer.registry_record_sha.push(registry_hash);
        let descriptor = EdgeSegmentDescriptorV1 {
            filename: "candidate-test.ndjson".to_owned(),
            first_sequence: "0".to_owned(),
            last_sequence: "0".to_owned(),
            record_count: "0".to_owned(),
            segment_seal_sha256: EDGE_ZERO_HASH.to_owned(),
            file_sha256: EDGE_ZERO_HASH.to_owned(),
        };
        let target = u64::try_from(EDGE_CANDIDATE_JOIN_CAPACITY).expect("capacity fits u64") + 1;
        while writer.candidate_validated_count < target {
            let first = writer.candidate_validated_count;
            let last = first
                .saturating_add(u64::try_from(EDGE_SEGMENT_MAX_RECORDS).expect("records fit u64"))
                .min(target);
            let member_sequences = (first..last).collect::<Vec<_>>();
            for sequence in &member_sequences {
                assert!(
                    writer
                        .candidate_joins
                        .insert(
                            *sequence,
                            EdgeCandidateJoinV1 {
                                candidate_sequence: *sequence,
                                coverage_generation: 0,
                                connection_sequence: 0,
                                connection_record_hash: connection_hash,
                                registry_terminal_record_hash: registry_hash,
                            },
                        )
                        .is_none()
                );
            }
            assert!(writer.candidate_joins.len() <= EDGE_SEGMENT_MAX_RECORDS);
            writer
                .validate_candidate_segment(&EdgeSealedSegmentV1 {
                    descriptor: descriptor.clone(),
                    member_sequences,
                })
                .expect("durable candidate join validation");
            assert!(writer.candidate_joins.is_empty());
        }
        assert_eq!(writer.candidate_validated_count, target);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_two_record_footer_matches_merged_sidecar_contract() {
        let (mut writer, root) = edge_test_writer("two-record");
        assert!(
            writer
                .push_rolling_record("fixture", "fixture/v1", "Observed", json!({"value": "a"}))
                .expect("first")
                .is_empty()
        );
        assert!(
            writer
                .push_rolling_record("fixture", "fixture/v1", "Observed", json!({"value": "b"}))
                .expect("second")
                .is_empty()
        );
        let sealed = writer.flush_rolling_ledger("fixture").expect("flush").expect("segment");
        assert_eq!(sealed.member_sequences, vec![0, 1]);
        assert_eq!(sealed.descriptor.first_sequence, "0");
        assert_eq!(sealed.descriptor.last_sequence, "1");
        assert_eq!(sealed.descriptor.record_count, "2");
        let bytes = fs::read(root.join(&sealed.descriptor.filename)).expect("segment bytes");
        let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        let footer: JsonValue = serde_json::from_slice(lines[2]).expect("footer JSON");
        assert_eq!(footer["recordCount"], "2");
        assert_eq!(footer["firstSequence"], "0");
        assert_eq!(footer["lastSequence"], "1");
        assert_eq!(footer["segmentSealSha256"], sealed.descriptor.segment_seal_sha256);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn strict_terminal_coverage_has_exact_s2_shape_and_authority_hash() {
        let (mut writer, root) = edge_test_writer("strict-coverage");
        let value = EdgeCanonicalWriterV1::terminal_coverage_value(SourceTerminalCoverageV3 {
            producer_epoch: 9,
            coverage_sequence: 0,
            route: EpochRouteV1::Authority,
            source_generation: Some(3),
            terminal: base_flashblocks::SourceCoverageTerminalV3::CliReceivedLookupSucceeded,
            terminal_hash: B256::with_last_byte(7),
            payload_first_record_hash: Some(B256::with_last_byte(8)),
            pending_snapshot_sequence: Some(4),
        });
        assert!(
            writer
                .push_rolling_record(
                    "coverage",
                    "edge-source-coverage/v3",
                    "CliReceivedLookupSucceeded",
                    value,
                )
                .expect("coverage append")
                .is_empty()
        );
        assert!(!writer.rolling_flush_due("coverage", Instant::now()));
        let ledger = writer.ledgers.get_mut("coverage").expect("coverage ledger");
        ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
        assert!(writer.rolling_flush_due("coverage", Instant::now()));

        let sealed = writer.flush_rolling_ledger("coverage").expect("flush").expect("segment");
        let bytes = fs::read(root.join(sealed.descriptor.filename)).expect("segment bytes");
        let first_line = bytes.split(|byte| *byte == b'\n').next().expect("record line");
        let record: JsonValue = serde_json::from_slice(first_line).expect("coverage JSON");
        let keys =
            record.as_object().expect("coverage object").keys().cloned().collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            [
                "coverageSequence",
                "payloadFirstRecordHash",
                "pendingSnapshotSequence",
                "previousRecordHash",
                "producerEpoch",
                "recordHash",
                "schema",
                "sequence",
                "sourceGeneration",
                "state",
                "terminal",
                "terminalHash",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
        assert_eq!(record["schema"], "edge-source-coverage/v3");
        assert_eq!(record["sequence"], record["coverageSequence"]);
        assert_eq!(
            record["recordHash"],
            EdgeCanonicalWriterV1::authority_record_hash("edge-source-coverage/v3", &record)
                .expect("authority hash")
        );
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn oversized_candidate_detail_is_compacted_without_losing_sequence() {
        let (mut writer, root) = edge_test_writer("oversized-candidate");
        let hash = EDGE_ZERO_HASH;
        let value = json!({
            "candidateGeneration": "0",
            "candidateSequence": "0",
            "connectionRecordHashAtCapture": hash,
            "connectionSequenceAtCapture": "0",
            "coverageGeneration": "0",
            "materializedState": "x".repeat(EDGE_SEGMENT_MAX_RECORD_BYTES),
            "payloadFirstRecordHash": hash,
            "payloadFirstRecordSequence": "0",
            "pendingSnapshotSequence": "0",
            "registryTerminalRecordHash": hash,
            "selectedPlanDigest": hash,
            "sourceGeneration": "0",
            "structuralTerminalHash": hash,
            "victimHash": hash,
        });
        assert!(
            writer
                .push_rolling_record(
                    "candidate",
                    "edge-candidate-detail/v1",
                    "CandidateDetail",
                    value,
                )
                .expect("oversized candidate compaction")
                .is_empty()
        );
        let ledger = writer.ledgers.get("candidate").expect("candidate ledger");
        assert_eq!(ledger.next_sequence, 1);
        assert_eq!(ledger.records[0]["schema"], "edge-candidate-detail-exclusion/v1");
        assert_eq!(ledger.records[0]["reason"], "CandidateDetailRecordTooLarge");
        assert!(writer.missing_evidence.contains("CandidateDetailRecordTooLarge"));
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_rolling_segment_rolls_at_record_and_byte_bounds() {
        let (mut writer, root) = edge_test_writer("rollover");
        let mut count_seal = None;
        for index in 0..EDGE_SEGMENT_MAX_RECORDS {
            let sealed = writer
                .push_rolling_record(
                    "count",
                    "fixture/count",
                    "Observed",
                    json!({"index": index.to_string()}),
                )
                .expect("count append");
            if !sealed.is_empty() {
                count_seal = sealed.last().map(|value| value.descriptor.clone());
            }
        }
        assert_eq!(count_seal.expect("count seal").record_count, "1024");

        let padding = "x".repeat(64 * 1024);
        let mut byte_seal = None;
        for index in 0..100 {
            let sealed = writer
                .push_rolling_record(
                    "bytes",
                    "fixture/bytes",
                    "Observed",
                    json!({"index": index.to_string(), "padding": padding}),
                )
                .expect("byte append");
            if let Some(segment) = sealed.last() {
                byte_seal = Some(segment.descriptor.clone());
                break;
            }
        }
        let byte_descriptor = byte_seal.expect("byte rollover");
        assert!(
            fs::metadata(root.join(byte_descriptor.filename)).expect("byte segment").len()
                <= u64::try_from(EDGE_SEGMENT_MAX_RECORD_BYTES).expect("bound")
        );
        assert!(byte_descriptor.record_count.parse::<usize>().expect("count") < 1024);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn registry_h1_pending_sequence_index_allows_sparse_keys_in_coverage_order() {
        let (mut writer, root) = edge_test_writer("sparse-registry-h1");
        let first_hash = B256::with_last_byte(1);
        let second_hash = B256::with_last_byte(2);
        assert!(writer.registry_h1_segment_sha.insert(4, (0, first_hash)).is_none());
        assert!(writer.registry_h1_segment_sha.insert(9, (1, second_hash)).is_none());
        assert_eq!(writer.registry_h1_segment_sha.get(&4), Some(&(0, first_hash)));
        assert_eq!(writer.registry_h1_segment_sha.get(&9), Some(&(1, second_hash)));
        let mut coverage_order =
            writer.registry_h1_segment_sha.values().copied().collect::<Vec<_>>();
        coverage_order.sort_by_key(|(coverage_sequence, _)| *coverage_sequence);
        assert_eq!(coverage_order, vec![(0, first_hash), (1, second_hash)]);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn snapshot_join_cleanup_terminalizes_outstanding_delivery() {
        let (writer, root) = edge_test_writer("snapshot-join-cleanup");
        let registry = writer.recorder.registry();
        let pending = Arc::new(pending_blocks());
        let registration = registry.register(&pending, None);
        registry.record_send(registration, Some(1)).expect("published outstanding delivery");
        assert_eq!(registry.final_summary().set_cardinalities.pending_delivery_final, 1);

        registry.cli_cancelled().expect("snapshot task registry cleanup");

        let summary = registry.final_summary();
        assert_eq!(summary.set_cardinalities.pending_delivery_final, 0);
        assert_eq!(summary.counters.cli_cancelled_attributed, 1);
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn clock_observation_capacity_plus_one_still_finalizes() {
        let (mut writer, root) = edge_test_writer("clock-capacity-plus-one");
        writer.clock_observations = vec![json!({}); EDGE_CLOCK_OBSERVATION_CAPACITY];

        seal_empty_test_writer(&mut writer);

        assert_eq!(writer.clock_observations.len(), EDGE_CLOCK_OBSERVATION_CAPACITY);
        assert!(writer.missing_evidence.contains("ClockObservationCapacityExcluded"));
        writer.finalize().expect("clock capacity finalization");
        let health: JsonValue = serde_json::from_slice(
            &fs::read(root.join("producer-health-final-v1.json")).expect("health final"),
        )
        .expect("health JSON");
        assert_eq!(
            health["coordinatorMissingEvidenceCounts"]["ClockObservationCapacityExcluded"],
            "1"
        );
        assert!(root.join("producer-manifest-v1.json").is_file());
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_clock_both_failed_omits_failed_values() {
        let (writer, root) = edge_test_writer("clock-failed");
        let value = writer.clock_anchor_value(ClockAnchorRecordV1 {
            producer_epoch: 9,
            anchor_sequence: 0,
            observation: base_flashblocks::WireObservationV1 {
                clock_observation_ordinal: 0,
                utc_status: base_flashblocks::ClockStatusV1::Failed(
                    base_flashblocks::ClockFailureV1 { status: -1, errno: 5 },
                ),
                utc_ns: None,
                mono_status: base_flashblocks::ClockStatusV1::Failed(
                    base_flashblocks::ClockFailureV1 { status: -1, errno: 5 },
                ),
                mono_ns: None,
                wire_digest: B256::ZERO,
            },
            startup: true,
            due_mono_ns: 1,
            sampled_mono_ns: 1,
            previous_anchor_hash: B256::ZERO,
            record_hash: B256::ZERO,
        });
        assert_eq!(value["pairStatus"], "BothFailed");
        assert!(value.get("utcNs").is_none());
        assert!(value.get("monoNs").is_none());
        drop(writer);
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[cfg(feature = "edge-measurement")]
    #[test]
    fn payload_first_failed_clocks_are_nullable_and_authority_hashes_survive_enveloping() {
        let (mut writer, root) = edge_test_writer("payload-first-failed-clock");
        let authority_hash_placeholder = B256::ZERO;
        let mut value = EdgeCanonicalWriterV1::payload_first_value(PayloadFirstObservationV1 {
            key: base_flashblocks::PayloadFirstKeyV1 {
                producer_epoch: 9,
                block_number: 10,
                payload_id: [3; 8],
            },
            source_generation: 4,
            observation: base_flashblocks::WireObservationV1 {
                clock_observation_ordinal: 2,
                utc_status: base_flashblocks::ClockStatusV1::Failed(
                    base_flashblocks::ClockFailureV1 { status: -1, errno: 5 },
                ),
                utc_ns: None,
                mono_status: base_flashblocks::ClockStatusV1::Failed(
                    base_flashblocks::ClockFailureV1 { status: -1, errno: 5 },
                ),
                mono_ns: None,
                wire_digest: B256::with_last_byte(6),
            },
            boot_id: *b"00000000-0000-0000-0000-000000000000",
            realtime_resolution_ns: 1,
            monotonic_resolution_ns: 1,
            record_sequence: 0,
            previous_record_hash: B256::ZERO,
            record_hash: authority_hash_placeholder,
        });
        let authority_hash = EdgeCanonicalWriterV1::authority_record_hash(
            "edge-payload-first-observation/v1",
            &value,
        )
        .expect("payload-first authority hash");
        value["recordHash"] = JsonValue::String(authority_hash);
        assert_eq!(value["utcNs"], JsonValue::Null);
        assert_eq!(value["monoNs"], JsonValue::Null);
        let original_previous = value["previousRecordHash"].clone();
        let original_hash = value["recordHash"].clone();
        writer
            .push_rolling_record(
                "payload-first",
                "edge-payload-first-envelope/v1",
                "edge-payload-first-envelope/v1",
                json!({
                    "payloadFirst": value,
                    "payloadFirstRecordHash": original_hash,
                    "schema": "edge-payload-first-envelope/v1",
                }),
            )
            .expect("payload-first envelope");
        let segment =
            writer.flush_rolling_ledger("payload-first").expect("flush").expect("segment");
        let bytes = fs::read(root.join(segment.descriptor.filename)).expect("segment bytes");
        let envelope: JsonValue =
            serde_json::from_slice(bytes.split(|byte| *byte == b'\n').next().expect("record"))
                .expect("envelope JSON");
        assert_eq!(envelope["payloadFirst"]["previousRecordHash"], original_previous);
        assert_eq!(envelope["payloadFirst"]["recordHash"], original_hash);
        assert_eq!(envelope["payloadFirstRecordHash"], original_hash);
        drop(writer);
        if let Some(fixture_dir) =
            std::env::var_os("EDGE_FAILED_CLOCK_FIXTURE_DIR").map(PathBuf::from)
        {
            assert!(!fixture_dir.exists(), "EDGE_FAILED_CLOCK_FIXTURE_DIR must not already exist");
            fs::rename(root, fixture_dir)
                .expect("publish failed-clock production serializer output");
        } else {
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[cfg(feature = "edge-measurement")]
    fn assert_committed_fixture_matches(root: &Path) {
        let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/edge-measurement-v1");
        let manifest: JsonValue = serde_json::from_slice(
            &fs::read(committed.join("sha256-manifest-v1.json"))
                .expect("committed fixture SHA-256 manifest"),
        )
        .expect("fixture SHA-256 manifest JSON");
        let entries = manifest["artifacts"].as_array().expect("fixture artifact entries");
        let expected_names = entries
            .iter()
            .map(|entry| entry["filename"].as_str().expect("fixture filename"))
            .collect::<BTreeSet<_>>();
        let actual_names = fs::read_dir(root)
            .expect("generated fixture directory")
            .map(|entry| {
                entry
                    .expect("generated fixture entry")
                    .file_name()
                    .into_string()
                    .expect("UTF-8 fixture filename")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_names,
            expected_names.iter().map(|name| (*name).to_owned()).collect(),
            "production fixture artifact set changed"
        );
        for entry in entries {
            let filename = entry["filename"].as_str().expect("fixture filename");
            let expected = fs::read(committed.join(filename)).expect("committed fixture artifact");
            let actual = fs::read(root.join(filename)).expect("generated fixture artifact");
            assert_eq!(actual, expected, "production fixture bytes changed for {filename}");
            let digest = EdgeMeasurementDurabilityV1::sha256(&actual);
            let digest_hex = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            assert_eq!(
                entry["sha256"].as_str(),
                Some(digest_hex.as_str()),
                "fixture SHA-256 changed for {filename}"
            );
            let size = actual.len().to_string();
            assert_eq!(
                entry["sizeBytes"].as_str(),
                Some(size.as_str()),
                "fixture size changed for {filename}"
            );
        }
    }

    #[cfg(feature = "edge-measurement")]
    #[test]
    fn edge_s2_consumer_fixture_regenerates_committed_production_bytes() {
        let fixture_dir = std::env::var_os("EDGE_S2_FIXTURE_DIR").map(PathBuf::from);
        if let Some(fixture_dir) = &fixture_dir {
            assert!(!fixture_dir.exists(), "EDGE_S2_FIXTURE_DIR must not already exist");
        }
        let (mut writer, root) = edge_test_writer("s2-consumer-production");
        writer
            .recorder
            .connection_transition(base_flashblocks::SourceConnectionTransitionV1::OwnerStart);
        writer.recorder.connection_transition(
            base_flashblocks::SourceConnectionTransitionV1::InitialConnectAttemptStarted,
        );
        writer
            .recorder
            .connection_transition(base_flashblocks::SourceConnectionTransitionV1::Established);

        let pending = Arc::new(pending_blocks());
        let flashblock = pending.get_flashblocks().remove(0);
        let admission =
            writer.recorder.observe_wire(b"fixture-decode-success").expect("wire admission");
        let generation =
            writer.recorder.decoded_flashblock(admission, &flashblock).expect("source generation");
        writer.recorder.actor_enqueue(generation, true);
        writer.recorder.actor_delivered(generation);
        assert_eq!(
            writer.recorder.begin_state_handoff(
                base_flashblocks::DecodedFlashblockKeyV1::from_flashblock(&flashblock),
            ),
            Some(generation),
        );
        assert_eq!(writer.recorder.take_source_generation(&flashblock), Some(generation));
        let rejected =
            writer.recorder.observe_wire(b"fixture-decode-rejected").expect("rejected admission");
        writer.recorder.decode_rejected(rejected);

        let registry = writer.recorder.registry();
        let registration = registry.register(&pending, Some(generation));
        let pending_snapshot_sequence =
            registration.pending_snapshot_sequence.expect("registered pending sequence");
        writer.recorder.record_deterministic_test_product(generation, pending_snapshot_sequence);
        registry.record_send(registration, Some(1)).expect("production registry send");
        let metadata = registry
            .cli_received(&pending)
            .expect("production registry lookup")
            .expect("authority registry receipt");
        let (metadata, payload_first, processor, connection, registry_terminal) =
            writer.recorder.snapshot_evidence(metadata).expect("truthful source evidence");
        let registry_terminal_record_hash = B256::new(EdgeMeasurementDurabilityV1::sha256(
            &EdgeCanonicalWriterV1::canonical_bytes(&EdgeCanonicalWriterV1::registry_h2_value(
                registry_terminal,
            ))
            .expect("registry terminal canonical bytes"),
        ));
        writer
            .owner
            .stage_deterministic_test_candidate(EdgeSnapshotEvidenceV1 {
                source_generation: processor.source_generation,
                pending_snapshot_sequence: metadata.identity.pending_snapshot_sequence,
                coverage_sequence: registry_terminal.coverage_sequence,
                payload_first_record_sequence: payload_first.record_sequence,
                payload_first_record_hash: payload_first.record_hash,
                structural_terminal_hash: processor.structural_terminal_hash,
                connection_sequence: connection.connection_sequence,
                connection_record_hash: connection.record_hash,
                registry_terminal_record_hash,
            })
            .expect("production candidate staging and terminal resolution");
        assert!(writer.drain_once().expect("production recorder and candidate drain"));
        loop {
            for ledger in writer.ledgers.values_mut() {
                if !ledger.records.is_empty() {
                    ledger.batch_started_at = Some(Instant::now() - EDGE_SEGMENT_MAX_FLUSH_LATENCY);
                }
            }
            if !writer.drain_once().expect("production durable source drain") {
                break;
            }
        }

        writer.recorder.prepare_cutoff();
        let (blink_count, candidate_bounds) = writer.owner.prepare_cutoff().expect("Blink cutoff");
        assert_eq!(blink_count, 1);
        assert_eq!(candidate_bounds.count, 1);
        assert!(
            writer.recorder.cutoff_drain_complete(),
            "source counters: {:?}",
            writer.recorder.source_final_counters()
        );

        let cutoff = writer.recorder.latch_cutoff(ProducerExternalBoundsV1 {
            last_admitted_blink_generation: 0,
            last_coverage_sequence: 1,
            last_candidate_sequence: 0,
        });
        writer.owner.latch_cutoff(base_mev_trader::ProducerEpochCutoffFieldsV1 {
            producer_epoch: cutoff.producer_epoch,
            cutoff_clock_observation_ordinal: cutoff.cutoff_clock_observation_ordinal,
            last_admitted_wire_ordinal: cutoff.last_admitted_wire_ordinal,
            last_admitted_source_generation: cutoff.last_admitted_source_generation,
            last_admitted_blink_generation: cutoff.last_admitted_blink_generation,
            last_pending_snapshot_sequence: cutoff.last_pending_snapshot_sequence,
            last_coverage_sequence: cutoff.last_coverage_sequence,
            last_candidate_sequence: cutoff.last_candidate_sequence,
            latch_mono_ns: cutoff.latch_mono_ns,
        });
        while writer.drain_once().expect("post-cutoff production drain") {}
        writer.recorder.verify_source_final().expect("source final");
        assert!(writer.owner.finalization_ready().expect("Blink readiness"));
        writer.finalize().expect("production finalization");

        let source_coverage: JsonValue = serde_json::from_slice(
            &fs::read(root.join("source-coverage-final-v1.json")).expect("source coverage final"),
        )
        .expect("source coverage JSON");
        assert_eq!(source_coverage["coverageCount"], "3");
        assert_eq!(source_coverage["coverageEmpty"], false);
        assert_eq!(source_coverage["lastSequenceInclusive"], "2");
        let connection_final: JsonValue = serde_json::from_slice(
            &fs::read(root.join("connection-final-v1.json")).expect("connection final"),
        )
        .expect("connection final JSON");
        assert!(connection_final.get("orderedPersistedSegments").is_none());
        assert!(connection_final.get("orderedRecordHashes").is_none());
        assert_eq!(connection_final["segmentSetSha256"].as_str().map(str::len), Some(64));
        assert_eq!(connection_final["finalRecordHash"].as_str().map(str::len), Some(64));
        let producer_health: JsonValue = serde_json::from_slice(
            &fs::read(root.join("producer-health-final-v1.json")).expect("producer health final"),
        )
        .expect("producer health JSON");
        let missing_evidence: JsonValue = serde_json::from_slice(
            &fs::read(root.join("missing-evidence-final.json")).expect("missing evidence final"),
        )
        .expect("missing evidence JSON");
        for value in [&producer_health, &missing_evidence] {
            let source_counts =
                value["sourceMissingEvidenceCounts"].as_object().expect("source reason counts");
            let registry_counts = value["registryTerminalExclusionCounts"]
                .as_object()
                .expect("registry exclusion counts");
            assert_eq!(source_counts.len(), 24);
            assert_eq!(registry_counts.len(), 3);
            assert!(source_counts.values().all(|count| count == "0"));
            assert!(registry_counts.values().all(|count| count == "0"));
            assert_eq!(value["missingEvidenceCount"], "0");
            assert_eq!(value["registryTerminalExclusionCount"], "0");
        }
        let candidate_detail: JsonValue = serde_json::from_slice(
            &fs::read(root.join("candidate-detail-final-v1.json")).expect("candidate detail final"),
        )
        .expect("candidate detail JSON");
        assert_eq!(candidate_detail["candidateReconcileVeto"], "CandidateFreezeV5InputUnavailable");
        assert_eq!(candidate_detail["candidateCount"], "1");
        assert!(!candidate_detail["candidateEmpty"].as_bool().expect("candidateEmpty"));
        assert_ne!(candidate_detail["finalRecordHash"], EDGE_ZERO_HASH);
        assert!(!candidate_detail["strictCandidateSourceAvailable"].as_bool().expect("source"));
        assert!(!root.join("candidate-final-v1.json").exists());
        assert!(root.join("candidate-00000000000000000000.ndjson").is_file());
        assert!(!root.join("reconcile-input.json").exists());

        let artifact_index: JsonValue = serde_json::from_slice(
            &fs::read(root.join("artifact-index-v1.json")).expect("artifact index"),
        )
        .expect("artifact index JSON");
        let indexed = artifact_index["artifacts"]
            .as_array()
            .expect("artifact entries")
            .iter()
            .filter_map(|entry| entry["filename"].as_str())
            .collect::<BTreeSet<_>>();
        for required in [
            "candidate-detail-final-v1.json",
            "clock-observations-v1.json",
            "missing-evidence-final.json",
            "producer-manifest-v1.json",
            "registry-final-v1.json",
            "segment-descriptors-v1.json",
            "source-coverage-final-v1.json",
            "source-final-v1.json",
        ] {
            assert!(indexed.contains(required), "production index missing {required}");
        }
        assert!(indexed.iter().any(|name| name.starts_with("coverage-")));
        assert!(indexed.iter().any(|name| name.starts_with("payload-first-")));
        drop(writer);
        if let Some(fixture_dir) = fixture_dir {
            fs::rename(root, fixture_dir).expect("publish exact production output directory");
        } else {
            assert_committed_fixture_matches(&root);
            fs::remove_dir_all(root).expect("cleanup production fixture");
        }
    }
}
