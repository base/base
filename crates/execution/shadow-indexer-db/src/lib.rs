#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    DEFAULT_DATABASE, DEFAULT_PORT, DEFAULT_USERNAME, PgConnectionParams, ShadowDbConfig,
};

mod repo;
pub use repo::{ShadowBlockRepo, ShadowFlushOutcome, ShadowSummaryRow, ShadowUnresolvedBacklog};

mod retention;
pub use retention::{SHADOW_RETENTION_LOCK_KEY, ShadowRetentionRepo, ShadowRetentionSweep};

mod models;
pub use models::{ShadowBlockPayload, ShadowBlockRow, ShadowCanonicalRef, ShadowWrite};

mod hash;
pub use hash::ShadowHash;
