#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(any(test, feature = "test-utils")), no_std)]

extern crate alloc;

#[macro_use]
extern crate tracing;

mod db;
pub use db::{NoopTrieDBProvider, TrieDB, TrieDBProvider};

mod builder;
pub use builder::{BlockBuildingOutcome, StatelessL2Builder, compute_receipts_root};

mod errors;
pub use errors::{
    Eip1559ValidationError, ExecutorError, ExecutorResult, TrieDBError, TrieDBResult,
};

mod util;

#[cfg(feature = "test-utils")]
pub mod test_utils;
