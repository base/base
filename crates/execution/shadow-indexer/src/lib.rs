#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowIndexerConfig, ShadowIndexerExtension};

mod exex;
pub use exex::{ShadowIndexerExEx, run_exex};

mod writer;
pub use writer::{ShadowWriter, spawn_writer};
