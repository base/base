#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
// Allow macro-generated code inside this crate to use `::base_precompile_storage::` paths.
extern crate self as base_precompile_storage;

mod error;
pub use error::{BasePrecompileError, DelegateCallNotAllowed, IntoPrecompileResult, Result};

mod neutral;
pub use neutral::{
    AccountInfo, IntoEnginePrecompileResult, PrecompileError, PrecompileHalt, PrecompileOutput,
    PrecompileResult, PrecompileStatus,
};

mod packing;
pub use packing::{
    FieldLocation, PackedSlot, Word, calc_element_loc, calc_element_offset, calc_element_slot,
    calc_packed_slot_count,
};

mod prefetch;
pub use prefetch::{PrefetchHint, PrefetchRequest, StatePrefetcher};

mod provider;
pub use provider::{
    ContractStorage, FromWord, Handler, Layout, LayoutCtx, Packable, PrecompileStorageProvider,
    Storable, StorableType, StorageFeatures, StorageKey, StorageOps, sealed,
    validate_loaded_code_presence,
};

mod registration;
pub use registration::NativePrecompile;

mod storage_ctx;
pub use storage_ctx::{CheckpointGuard, StorageCtx};

mod types;
pub use types::{
    ArrayHandler, BytesLikeHandler, HandlerCache, Mapping, MappingHandler, Set, SetHandler, Slot,
    VecHandler,
};

mod evm;
pub use evm::EvmPrecompileStorageProvider;

mod journal;
pub use journal::JournalStorageProvider;

#[cfg(any(test, feature = "test-utils"))]
mod hashmap;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::HashMapStorageProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::setup_storage;
