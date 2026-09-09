//! The implementation of sparse MPT.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "std")]
mod state;
#[cfg(feature = "std")]
pub use state::*;

#[cfg(feature = "std")]
mod trie;
#[cfg(feature = "std")]
pub use trie::*;

mod types;
pub use types::*;

#[cfg(feature = "std")]
mod arena;
#[cfg(feature = "std")]
pub use arena::*;

#[cfg(feature = "metrics")]
mod metrics;

#[cfg(feature = "trie-debug")]
pub mod debug_recorder;
#[cfg(feature = "trie-debug")]
use serde_json as _;

/// Re-export sparse trie error types.
pub mod errors {
    pub use base_execution_state_types::{
        SparseStateTrieError, SparseStateTrieErrorKind, SparseStateTrieResult, SparseTrieError,
        SparseTrieErrorKind, SparseTrieResult,
    };
}
