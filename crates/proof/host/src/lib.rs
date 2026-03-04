#![doc = include_str!("../README.md")]

mod error;
pub use error::{HostError, Result};

mod config;
pub use config::{HostConfig, HostProviders};

mod host;
pub use host::Host;

mod server;
pub use server::PreimageServer;

mod handler;
pub use handler::{handle_hint, parse_blob_hint};

mod kv;
#[cfg(feature = "disk")]
pub use kv::DiskKeyValueStore;
pub use kv::{
    BootKeyValueStore, KeyValueStore, MemoryKeyValueStore, SharedKeyValueStore, SplitKeyValueStore,
    store_ordered_trie,
};

mod backend;
pub use backend::{OfflineHostBackend, OnlineHostBackend};

#[cfg(feature = "precompiles")]
mod precompiles;
#[cfg(feature = "precompiles")]
pub use precompiles::execute;
