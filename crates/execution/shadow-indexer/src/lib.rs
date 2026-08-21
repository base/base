#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowIndexerConfig, ShadowIndexerExtension};

mod metrics;
pub use metrics::ShadowIndexerMetrics;

mod exex;
pub use exex::ShadowIndexerExEx;

mod writer;
pub use writer::ShadowWriter;
