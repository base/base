#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowIndexerConfig, ShadowIndexerExtension};

mod exex;
pub use exex::ShadowIndexerExEx;

mod write;
pub use write::ShadowWrite;

mod writer;
pub use writer::ShadowWriter;
