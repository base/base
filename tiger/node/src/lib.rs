#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::Adapter;

mod witness;
pub use witness::{NodeSource, StateUpdate, WitnessDb, WitnessError, WitnessNodes, WitnessState};

mod exec;
pub use exec::{BlockExecution, Built, Executed, Validated};

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::EdgeCounters;
