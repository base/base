//! Contains various traversal stages for the derivation pipeline.
//!
//! The traversal stage sits at the bottom of the pipeline, and is responsible for
//! providing the next block to the next stage in the pipeline.
//!
//! ## Types
//!
//! - [`PollingTraversal`]: An active traversal stage that polls for the next block through its
//!   provider.

mod polling;
pub use polling::PollingTraversal;
