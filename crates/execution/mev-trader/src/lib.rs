#![doc = include_str!("../README.md")]

mod port;
pub use port::{
    BundleVisitor, PayloadVisitor, PendingSnapshotView, PortError, SnapshotCaptureCoordinator,
    SnapshotHandle, SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl,
    VisitSummary,
};

mod frame;
pub use frame::{
    FrameProcessor, MAX_FRAME_AGE_MILLIS, MAX_RAW_FRAME_BYTES, SnapshotCoherence, VictimFrame,
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
mod runtime;
