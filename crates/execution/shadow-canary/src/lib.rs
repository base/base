#![doc = include_str!("../README.md")]

mod extension;
pub use extension::{ShadowCanaryConfig, ShadowCanaryExtension};

mod exex;
pub use exex::{run_exex, ShadowCanaryExEx};

mod writer;
pub use writer::{spawn_writer, ShadowWriter};
