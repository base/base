#![doc = include_str!("../README.md")]

mod error;
pub use error::{HostError, Result};

mod eth;
pub use eth::{ACCELERATED_PRECOMPILES, RpcProviderFactory, execute};

mod server;
pub use server::{PreimageServer, PreimageServerError};

mod kv;
pub use kv::{
    DiskKeyValueStore, KeyValueStore, MemoryKeyValueStore, SharedKeyValueStore, SplitKeyValueStore,
};

mod backend;
pub use backend::{OfflineHostBackend, OnlineHostBackend, store_ordered_trie};

mod args;
pub use args::HostArgs;

mod providers;
pub use providers::HostProviders;

mod host;
pub use host::Host;

mod local_kv;
pub use local_kv::LocalInputs;

mod handler;
pub use handler::{fetch_hint, parse_blob_hint};
