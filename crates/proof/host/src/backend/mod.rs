//! Backend for the preimage server.

mod offline;
pub use offline::OfflineHostBackend;

#[cfg(feature = "single")]
mod online;
#[cfg(feature = "single")]
pub use online::{HintHandler, OnlineHostBackend, OnlineHostBackendCfg};

mod util;
pub use util::store_ordered_trie;
