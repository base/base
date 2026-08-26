#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    DEFAULT_DATABASE, DEFAULT_PORT, DEFAULT_USERNAME, PgConnectionParams, ShadowDbConfig,
};

mod cursor;
pub use cursor::{ShadowBlockCursor, ShadowMetricsCursorRepo};

mod repo;
pub use repo::{ShadowBlockRepo, ShadowFlushOutcome, ShadowSummaryRow, ShadowUnresolvedBacklog};

mod models;
pub use models::{ShadowBlockPayload, ShadowBlockRow, ShadowCanonicalRef, ShadowWrite};
