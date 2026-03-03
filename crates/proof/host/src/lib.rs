#![doc = include_str!("../README.md")]

mod error;
pub use error::{HostError, Result};

mod eth;
pub use eth::RpcProviderFactory;

mod server;
pub use server::{PreimageServer, PreimageServerError};

mod kv;
pub use kv::{KeyValueStore, MemoryKeyValueStore, SharedKeyValueStore, SplitKeyValueStore};

mod backend;
pub use backend::{
    HintHandler, OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg, store_ordered_trie,
};
