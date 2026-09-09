//! Sparse Merkle Patricia trie implementation.
mod state;
pub use state::*;
mod trie;
pub use trie::*;
mod arena;
pub use arena::*;
#[cfg(feature = "trie-debug")]
mod debug_recorder;
#[cfg(feature = "metrics")]
mod metrics;
#[cfg(feature = "trie-debug")]
pub use debug_recorder::*;
