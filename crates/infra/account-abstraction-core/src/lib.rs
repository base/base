//! High-level services that orchestrate domain logic.
//! Designed to be reused by other binaries (ingress-rpc, workers, etc.)

pub mod domain;
pub mod factories;
pub mod infrastructure;
pub mod services;

// Convenient re-exports for common imports
pub use domain::{
    events::MempoolEvent,
    mempool::{Mempool, PoolConfig},
    types::{ValidationResult, VersionedUserOperation, WrappedUserOperation},
};

pub use infrastructure::in_memory::InMemoryMempool;

pub use services::{
    interfaces::{event_source::EventSource, user_op_validator::UserOperationValidator},
    mempool_engine::MempoolEngine,
};

pub use factories::kafka_engine::create_mempool_engine;
