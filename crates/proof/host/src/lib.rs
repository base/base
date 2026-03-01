#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub)]
#![warn(rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]

mod error;
pub use error::{HostError, Result};

mod eth;
pub use eth::rpc_provider;

mod server;
pub use server::{PreimageServer, PreimageServerError};

mod kv;
pub use kv::{KeyValueStore, MemoryKeyValueStore, SharedKeyValueStore, SplitKeyValueStore};

mod backend;
pub use backend::{HintHandler, OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg, store_ordered_trie};
