#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::Adapter;

mod witness;
pub use witness::{WitnessDb, WitnessError, WitnessState};

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::EdgeCounters;
