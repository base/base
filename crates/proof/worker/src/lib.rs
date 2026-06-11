#![doc = include_str!("../README.md")]

mod claimed_job;
pub use claimed_job::{
    ClaimedProofJobHandler, ClaimedProofJobMetadata, ClaimedProofJobMetadataError,
};

mod heartbeat;
pub use heartbeat::{
    DEFAULT_WORKER_HEARTBEAT_INTERVAL, DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_WORKER_MAX_CONSECUTIVE_HEARTBEAT_FAILURES, MIN_WORKER_HEARTBEAT_INTERVAL,
    WorkerHeartbeat, WorkerHeartbeatConfig,
};
