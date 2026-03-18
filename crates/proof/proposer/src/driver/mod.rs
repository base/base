//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the prover server, and contract
//! interactions to generate and submit output proposals as dispute games.
//!
//! # Lifecycle control
//!
//! The [`Driver`] itself runs a single polling loop via [`Driver::run`].
//! [`DriverHandle`] wraps a `Driver` and exposes start/stop/is-running
//! semantics through the [`ProposerDriverControl`] trait, which is consumed
//! by the admin JSON-RPC server.

mod core;
pub use self::core::{Driver, DriverConfig, RecoveredGame};

mod handle;
pub use handle::{DriverHandle, ProposerDriverControl};
