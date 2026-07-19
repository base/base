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
pub use registry::AuditedWriteKey;

mod storage;
pub use storage::{DeltaGuard, MaterializedState, MaterializedWrite, StateMaterializer};

mod latency;
mod lifecycle;
mod oracle;
pub use oracle::{
    ExactPrefixCoordinator, ExactPrefixOracle, FrozenPredecessor, IndependentOracle, OracleDigest,
    OracleEvaluation, OracleObservation, OracleOutcome, PredecessorOracle,
};
mod pairwise;
mod runtime;
