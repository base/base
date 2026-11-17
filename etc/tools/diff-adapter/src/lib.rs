#![doc = include_str!("../README.md")]

mod adapter;
pub use adapter::{BASE_MAINNET, BASE_MAINNET_NODE_CHAIN, DiffAdapter, Outcome};

mod coverage;
pub use coverage::{Coverage, CoverageFrame};

#[cfg(feature = "edge-coverage")]
mod edges;
#[cfg(feature = "edge-coverage")]
pub use edges::{__sanitizer_cov_8bit_counters_init, EdgeCounters};

mod witness;
pub use witness::{OpenTries, TrieNodes, WitnessError, WitnessState, WitnessStore};
