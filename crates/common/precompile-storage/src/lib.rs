#![doc = include_str!("../README.md")]
// Allow macro-generated code inside this crate to use `::base_precompile_storage::` paths.
extern crate self as base_precompile_storage;

/// Error types for native precompile operations.
pub mod error;
/// Bit-level packing utilities for EVM storage slots.
pub mod packing;
/// Core storage provider traits and type system.
pub mod provider;
/// Precompile registration trait.
pub mod registration;
/// Thread-local storage context.
pub mod storage_ctx;
/// Storage types: `Slot`, `Mapping`, `Vec`, `Set`, primitives.
pub mod types;

/// In-memory storage backend for tests.
pub mod hashmap;
/// Production EVM-backed storage provider (stub).
pub mod evm;

pub use error::{BasePrecompileError, IntoPrecompileResult, Result};
pub use packing::FieldLocation;
pub use provider::{
    ContractStorage, FromWord, Handler, Layout, LayoutCtx, Packable, PrecompileStorageProvider,
    Storable, StorableType, StorageKey, StorageOps,
};
pub use registration::NativePrecompile;
pub use storage_ctx::{CheckpointGuard, StorageCtx};
pub use types::{
    Mapping, Set, SetHandler, Slot,
    array::ArrayHandler,
    vec::VecHandler,
};
