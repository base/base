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
pub use backend::{OfflineHostBackend, OnlineHostBackend, store_ordered_trie};

#[cfg(feature = "precompiles")]
mod precompiles;
#[cfg(feature = "precompiles")]
pub use precompiles::execute;

mod host;
pub use host::Host;

mod providers;
pub use providers::HostProviders;

mod handler;
pub use handler::{handle_hint, parse_blob_hint};

mod local_kv;
pub use local_kv::LocalInputs;
