#![doc = include_str!("../README.md")]

#[cfg(feature = "service")]
mod config;
#[cfg(feature = "service")]
pub use config::ShadowIndexerConfig;

#[cfg(feature = "service")]
mod exex;
#[cfg(feature = "service")]
pub use exex::ShadowIndexerExEx;

#[cfg(feature = "service")]
mod metrics;
#[cfg(feature = "service")]
pub use metrics::{ShadowExExMetrics, ShadowWriterMetrics};

#[cfg(feature = "service")]
mod retention;
#[cfg(feature = "service")]
pub use retention::{ShadowRetention, ShadowRetentionConfig};

#[cfg(feature = "service")]
mod writer;
#[cfg(feature = "service")]
pub use writer::ShadowWriter;

mod database_config;
pub use database_config::{
    DEFAULT_DATABASE, DEFAULT_PORT, DEFAULT_USERNAME, PgConnectionParams, ShadowDbConfig,
};
mod repo;
pub use repo::{ShadowBlockRepo, ShadowFlushOutcome, ShadowSummaryRow, ShadowUnresolvedBacklog};
mod database_retention;
pub use database_retention::{
    SHADOW_RETENTION_LOCK_KEY, ShadowRetentionRepo, ShadowRetentionSweep,
};
mod models;
pub use models::{ShadowBlockPayload, ShadowBlockRow, ShadowCanonicalRef, ShadowWrite};
