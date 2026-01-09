//! Primitive types and traits for Base node runner extensions.
//!
//! This crate provides shared types and traits that enable extension modules
//! to be defined in their respective domain crates while avoiding circular
//! dependencies with the runner crate.

mod config;
pub use config::{FlashblocksConfig, TracingConfig};

mod extension;
pub use extension::{BaseNodeExtension, ConfigurableBaseNodeExtension};

mod types;
pub use types::{FlashblocksCell, OpBuilder, OpProvider};
