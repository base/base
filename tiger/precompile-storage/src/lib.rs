#![doc = include_str!("../README.md")]

extern crate alloc;

mod error;
pub use error::{BasePrecompileError, DelegateCallNotAllowed, IntoPrecompileResult, Result};

mod neutral;
pub use neutral::{
    AccountInfo, Bytecode, Checkpoint, PrecompileError, PrecompileHalt, PrecompileOutput,
    PrecompileResult, PrecompileStatus,
};

mod gas;
pub use gas::{GasSchedule, SstoreValues};

mod layout;
pub use layout::{StorageLayout, StorageValue};

mod provider;
pub use provider::{PrecompileStorageProvider, StorageFeatures};

mod storage_ctx;
pub use storage_ctx::{CheckpointGuard, StorageCtx};

#[cfg(any(test, feature = "test-utils"))]
mod hashmap;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::HashMapStorageProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::setup_storage;
