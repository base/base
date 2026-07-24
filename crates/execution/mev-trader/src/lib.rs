#![doc = include_str!("../README.md")]

mod blink_ingress;
pub use blink_ingress::{
    A1Counters, A1Outcome, A1Status, BlinkCredential, BlinkFeedClient, BlinkIngressConfig,
    BlinkVictim, QueuedBlinkVictim, RuntimeShutdown,
};
mod port;
#[cfg(feature = "edge-measurement")]
pub use port::EdgeSnapshotEvidenceV1;
pub use port::{
    BundleVisitor, PayloadVisitor, PendingAccountNonce, PendingSnapshotView, PortError,
    SnapshotCaptureCoordinator, SnapshotHandle, SnapshotHandleFactory, TraderSnapshotPort,
    TransactionVisitor, VisitControl, VisitSummary,
};
#[cfg(feature = "edge-measurement")]
mod measurement_tx;
#[cfg(feature = "edge-measurement")]
pub use measurement_tx::{
    BackrunMeasurementTxV1, MEASUREMENT_CHAIN_ID, MEASUREMENT_EXECUTOR, MEASUREMENT_GAS_LIMIT,
    MeasurementExecutionHopV1, MeasurementNonceWitnessV1, MeasurementTxDeriverV1,
    MeasurementTxError, MeasurementTxInputV1,
};
#[cfg(feature = "edge-measurement")]
mod edge_measurement;
#[cfg(feature = "edge-measurement")]
pub use edge_measurement::{
    BLINK_LEDGER_CAPACITY, BLINK_REJECT_BRANCH_INVENTORY_V3, BlinkGenerationTerminalV1,
    BlinkLedgerSnapshotV1, BlinkMeasurementLedgerV1, BlinkRejectClassifierV3,
    BlinkRejectDispositionV3, BlinkRejectReasonV3, BlinkRejectRecordV3,
    CandidatePreEnqueueDropReasonV3, CheckedCandidateBoundsV1, EDGE_MAX_VICTIM_RAW_BYTES,
    EdgeCandidateEvidenceV3, EdgeCandidateStageInputV3, EdgeCandidateV3,
    EdgeMeasurementDurabilityV1, EdgeMeasurementError, EdgeMeasurementFinalV1,
    EdgeMeasurementOwnerConfigV1, EdgeMeasurementOwnerV1, EdgeProducerError, EdgeProducerRecordV1,
    ProducerEpochCutoffFieldsV1, ProducerEpochCutoffLatchV1, ProducerEpochCutoffV1,
    SelectedDtoTerminalV1,
};

mod frame;
pub use frame::{
    DeltaError, DirtyPoolSet, FrameCommitGuard, FrameProcessor, MAX_FRAME_AGE_MILLIS,
    MAX_RAW_FRAME_BYTES, ProcessedFrame, SnapshotCoherence, ValidatedFrameDelta, VictimFrame,
};

mod registry;
pub use registry::{
    AuditedWriteCodec, AuditedWriteKey, BitmapWordRead, CanonicalDigest, CanonicalEncoder,
    CoverageHasher, DescriptorHasher, DescriptorPlanDigest, ExactProtocol, FieldKind, FieldRead,
    FixturePoolRegistry, FrameAuditPlan, InitializedTickRead, PoolDescriptor,
    PoolDescriptorVisitor, PoolRegistry, PoolUniverseSnapshot, ProvisionedPoolRegistry,
    RegistryDigest, RegistryError, RegistryHasher, SnapshotCollector, StoragePlanCodec,
    StoragePlanValidator, StorageReadPlan, V3ReadPlan,
};

mod preparation;
pub use preparation::{PoolStatePreparer, PreparationError};

mod storage;
pub use storage::{
    DeltaGuard, MAX_V3_TICK, MIN_V3_TICK, MaterializedState, MaterializedWrite, StateMaterializer,
    V3PreparedState, V3StorageValidator,
};

mod latency;
pub use latency::{
    LATENCY_THRESHOLD_NS, LATENCY_TIMED_RUNS, LATENCY_WARMUP_RUNS, LatencyAccounting, LatencyError,
    LatencyRecorder, LatencyReport, StageLatencyRecorder, StageLatencyReport, StageLatencySample,
    StageQuantiles,
};

mod lifecycle;
pub use lifecycle::{
    ANALYSIS_THREADS, CancellationProbe, CancellationToken, DEADLINE_MILLIS, DedicatedAnalysisPool,
    DisableReason, GlobalLifecycle, GlobalState, HANG_GRACE_MILLIS, LatestSlot, LifecycleError,
    MAX_ACCOUNTS, MAX_CANDIDATES, MAX_CANONICAL_BYTES, MAX_CODE_BYTES, MAX_CODE_ENTRIES, MAX_PAIRS,
    MAX_PLANS_PER_FRAME, MAX_POOLS, MAX_PREFIX_TRANSACTIONS, MAX_STORAGE_SLOTS, MAX_TOTAL_TICKS,
    MAX_V3_BITMAP_WORDS, ShadowLatestSlot, ShadowSlotCounters, ShadowSubmit, SlotSubmit,
    SoleWorker, TaskRun, TaskRunner, TaskState, Watchdog, WatchdogStatus, WorkCaps, WorkerClaim,
    WorkloadSize,
};
mod oracle;
pub use oracle::{
    ExactPrefixCoordinator, ExactPrefixOracle, FrozenPredecessor, IndependentOracle, OracleDigest,
    OracleEvaluation, OracleObservation, OracleOutcome, PredecessorOracle,
};
mod pairwise;
pub use pairwise::{
    BackrunHop, BackrunPlan, BackrunPlanDigest, CachedEvaluator, D44CandidateEncoder,
    D44ContractError, D44ErrorEncoder, FEE_DENOMINATOR, ISSUE76_ENGINE_QUOTE,
    ISSUE76_OBSERVED_QUOTE, ISSUE76_PROVENANCE_BLOB, ISSUE76_QUOTE_BLOB, ISSUE76_QUOTE_GAP, K16,
    MAX_ACTIVATED_TOKENS, MAX_SQRT_RATIO, MAX_SQRT_RATIO_DECIMAL, MIN_SQRT_RATIO,
    MeasurementContext, MeasurementEncoder, OptimizedSize, OptimizerSample, PAIRWISE_MAX_TICK,
    PAIRWISE_MIN_TICK, PAIRWISE_SOURCE_COMMIT, PAIRWISE_SOURCE_TREE, PairwiseCandidate,
    PairwiseEngine, PairwiseError, PairwiseMath, PairwiseOptimizer, PairwiseV3Tick,
    PreparedPoolQuote, PreparedPoolState, PreparedV3QuoteParams, RankedMarket, SizeBounds,
    SwapStep, TICK_MULTIPLIERS, V3QuoteResult, WETH,
};
mod runtime;
#[cfg(feature = "t4b-shadow")]
pub use runtime::{
    CandidateAssemblyView, CandidateTxShapeObserver, T4bOutcome, T4bOutcomeCounters,
};
pub use runtime::{
    MevTraderRuntime, MevTraderRuntimeConfig, PoolCodeHashView, RuntimeInstallError,
    ShadowFrameMeasurement, ShadowOutcome, ShadowOutcomeCounters,
};

mod victim_claim;
pub use victim_claim::{
    CampaignId, ClaimResult, ClaimStoreError, StoreIdentity, VictimClaim, VictimClaimConfig,
    VictimClaimStore,
};

mod killstate_anchor;
pub use killstate_anchor::{
    ANCHOR_DB, ANCHOR_DIR, AnchorError, AnchorStoreIdentity, AnchoredKillStateStore,
    EXPECTED_ANCHOR_IDENTITY, KILLSTATE_DIR, PATHS_MOUNT_DOMAIN, Rollback, SEED_AUTH_DOMAIN,
    StartupError, open_anchored_killstate,
};
#[cfg(any(test, feature = "p0-provisioning"))]
pub use killstate_anchor::{AnchorProvisioner, BootstrapEvidence, SeedAuthorization};
mod safety;
pub use safety::{
    ArmedCriteria, CRITERIA_SHA, ClosedReason, CriteriaArtifact, Decision, DrawdownInput,
    EXPECTED_CRITERIA_COMMIT, EXPECTED_CRITERIA_VERSION, GuardReason, KillReason, KillState,
    KillStateStore, KillStoreError, LossProvenance, OWNER_ATTEST_ADDRESS, ResetAttestation,
    SubmitContext, SubmitDecision, UnarmedReason, drawdown_floor, kill_switch, per_tx_cap,
    production_arming_criteria, submit_gate,
};
