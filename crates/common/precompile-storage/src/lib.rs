#![doc = include_str!("../README.md")]
// Allow macro-generated code inside this crate to use `::base_precompile_storage::` paths.
extern crate self as base_precompile_storage;

mod error;
pub use error::{BasePrecompileError, IntoPrecompileResult, Result};

mod packing;
pub use packing::{
    FieldLocation, PackedSlot, calc_element_loc, calc_element_offset, calc_element_slot,
    calc_packed_slot_count, create_element_mask, delete_from_word, extract_from_word,
    insert_into_word,
};

mod provider;
pub use provider::{
    ContractStorage, FromWord, Handler, Layout, LayoutCtx, Packable, PrecompileStorageProvider,
    Storable, StorableType, StorageKey, StorageOps, sealed,
};

mod registration;
pub use registration::NativePrecompile;

mod storage_ctx;
pub use storage_ctx::{CheckpointGuard, StorageCtx};

mod types;
pub use types::{
    ArrayHandler, BytesLikeHandler, HandlerCache, Mapping, Set, SetHandler, Slot, VecHandler,
};

mod evm;
pub use evm::EvmPrecompileStorageProvider;

mod hashmap;
pub use hashmap::HashMapStorageProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::setup_storage;
#[cfg(any(test, feature = "test-utils"))]
pub use packing::gen_word_from;
