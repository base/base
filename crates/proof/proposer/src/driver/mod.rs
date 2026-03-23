//! Proving pipeline for the proposer.
//!
//! The [`ProvingPipeline`] coordinates between RPC clients, the prover server,
//! and contract interactions to generate and submit output proposals as dispute
//! games. It runs multiple proofs concurrently (plan → prove → submit) while
//! maintaining strictly sequential on-chain submission.
//!
//! [`PipelineHandle`] wraps a pipeline and exposes start/stop/is-running
//! semantics through the [`ProposerDriverControl`] trait, which is consumed
//! by the admin JSON-RPC server.

mod core;
pub use self::core::{DriverConfig, RecoveredState};

mod handle;
pub use handle::{PipelineHandle, ProposerDriverControl};

mod pipeline;
pub use pipeline::{PipelineConfig, ProvingPipeline};
