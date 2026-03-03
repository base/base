#![doc = include_str!("../README.md")]

mod error;
pub use error::{HostError, Result};

mod eth;
pub use eth::rpc_provider;

mod server;
pub use server::{PreimageServer, PreimageServerError};

mod kv;
#[cfg(feature = "disk")]
pub use kv::DiskKeyValueStore;
pub use kv::{KeyValueStore, MemoryKeyValueStore, SharedKeyValueStore, SplitKeyValueStore};

mod backend;
#[cfg(feature = "single")]
pub use backend::{HintHandler, OnlineHostBackend, OnlineHostBackendCfg};
pub use backend::{OfflineHostBackend, store_ordered_trie};

#[cfg(feature = "precompiles")]
mod precompiles;
#[cfg(feature = "precompiles")]
pub use precompiles::execute;

#[cfg(feature = "single")]
mod single;
#[cfg(feature = "single")]
pub use single::{
    SingleChainHintHandler, SingleChainHost, SingleChainHostError, SingleChainLocalInputs,
    SingleChainProviders, parse_blob_hint,
};
