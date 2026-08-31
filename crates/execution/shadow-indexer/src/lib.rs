#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowIndexerConfig, ShadowIndexerExtension};

mod exex;
pub use exex::ShadowIndexerExEx;

mod metrics;
pub use metrics::{ShadowExExMetrics, ShadowWriterMetrics};

mod retention;
pub use retention::{ShadowRetention, ShadowRetentionConfig};

mod writer;
pub use writer::ShadowWriter;
