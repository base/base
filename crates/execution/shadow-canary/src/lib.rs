#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowCanaryConfig, ShadowCanaryExtension};

mod exex;
pub use exex::{ShadowCanaryExEx, run_exex};

mod writer;
pub use writer::{ShadowWriter, spawn_writer};
