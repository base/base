#![doc = include_str!("../README.md")]

mod blink_ingress;
pub use blink_ingress::{
    A1Counters, A1Outcome, A1Status, BlinkCredential, BlinkFeedClient, BlinkIngressConfig,
    BlinkVictim, QueuedBlinkVictim, RuntimeShutdown,
};
mod port;
pub use port::{
    BundleVisitor, PayloadVisitor, PendingSnapshotView, PortError, SnapshotCaptureCoordinator,
    SnapshotHandle, SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl,
    VisitSummary,
};

mod frame;
pub use frame::{
    FrameProcessor, MAX_FRAME_AGE_MILLIS, MAX_RAW_FRAME_BYTES, ProcessedFrame, SnapshotCoherence,
    VictimFrame,
};

mod registry;
pub use registry::{
    AuditedWriteCodec, AuditedWriteKey, BitmapWordRead, CanonicalDigest, CanonicalEncoder,
    CoverageHasher, DescriptorHasher, DescriptorPlanDigest, ExactProtocol, FieldKind, FieldRead,
    FixturePoolRegistry, InitializedTickRead, PoolDescriptor, PoolDescriptorVisitor, PoolRegistry,
    RegistryDigest, RegistryError, RegistryHasher, StoragePlanCodec, StoragePlanValidator,
    StorageReadPlan,
};

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
    SlotSubmit, SoleWorker, TaskRun, TaskRunner, TaskState, Watchdog, WatchdogStatus, WorkCaps,
    WorkerClaim, WorkloadSize,
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
pub use runtime::{MevTraderRuntime, MevTraderRuntimeConfig, RuntimeInstallError};

mod victim_claim;
pub use victim_claim::{
    CampaignId, ClaimResult, ClaimStoreError, StoreIdentity, VictimClaim, VictimClaimConfig,
    VictimClaimStore,
};

mod safety;
pub use safety::{
    ArmedCriteria, ClosedReason, CRITERIA_SHA, CriteriaArtifact, Decision, DrawdownInput,
    EXPECTED_CRITERIA_COMMIT, EXPECTED_CRITERIA_VERSION, FileKillStateStore, GuardReason, KillReason,
    KillState, KillStateStore, KillStoreError, LossProvenance, OWNER_ATTEST_ADDRESS, ResetAttestation,
    SubmitContext, SubmitDecision, UnarmedReason, drawdown_floor, kill_switch, per_tx_cap,
    production_arming_criteria, submit_gate,
};
