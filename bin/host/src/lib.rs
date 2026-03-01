#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use base_proof_host::{
    HintHandler, KeyValueStore, MemoryKeyValueStore, OfflineHostBackend, OnlineHostBackend,
    OnlineHostBackendCfg, PreimageServer, PreimageServerError, Result, SharedKeyValueStore,
    SplitKeyValueStore,
};

mod kv;
pub use kv::DiskKeyValueStore;

pub use base_proof_host::HostError;

pub mod eth;

#[cfg(feature = "single")]
pub mod single;
