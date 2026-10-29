#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
// Allow macro-generated code inside this crate to use `::base_precompile_storage::` paths.
extern crate self as base_precompile_storage;

mod error;
pub use error::{BasePrecompileError, DelegateCallNotAllowed, IntoPrecompileResult, Result};

mod neutral;
pub use neutral::{
    AccountInfo, Bytecode, CheckpointStack, PrecompileError, PrecompileHalt, PrecompileOutput,
    PrecompileResult, PrecompileStatus, StorageCheckpoint,
};

mod packing;
pub use packing::{
    FieldLocation, PackedSlot, Word, calc_element_loc, calc_element_offset, calc_element_slot,
    calc_packed_slot_count,
};

mod provider;
pub use provider::{
    ContractStorage, FromWord, Handler, KECCAK256_GAS, KECCAK256_WORD_GAS, Layout, LayoutCtx,
    Packable, PrecompileStorageProvider, Storable, StorableType, StorageFeatures, StorageKey,
    StorageOps, sealed, validate_loaded_code_presence,
};

mod registration;
pub use registration::NativePrecompile;

mod storage_ctx;
pub use storage_ctx::{CheckpointGuard, StorageCtx};

mod types;
pub use types::{
    ArrayHandler, BytesLikeHandler, HandlerCache, Mapping, MappingHandler, Slot, VecHandler,
};

#[cfg(feature = "revm")]
mod revm_compat;
#[cfg(feature = "revm")]
pub use revm_compat::IntoRevmPrecompileResult;

#[cfg(feature = "revm")]
mod evm;
#[cfg(feature = "revm")]
pub use evm::EvmPrecompileStorageProvider;

#[cfg(feature = "revm")]
mod journal;
#[cfg(feature = "revm")]
pub use journal::JournalStorageProvider;

#[cfg(feature = "evm2")]
mod evm2_compat;
#[cfg(feature = "evm2")]
pub use evm2_compat::{Evm2TxOrigin, IntoEvm2PrecompileResult};

#[cfg(feature = "evm2")]
mod evm2_provider;
#[cfg(feature = "evm2")]
pub use evm2_provider::Evm2PrecompileStorageProvider;

#[cfg(feature = "evm2")]
mod evm2_journal;
#[cfg(feature = "evm2")]
pub use evm2_journal::Evm2JournalStorageProvider;

#[cfg(any(test, feature = "test-utils"))]
mod hashmap;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::HashMapStorageProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use hashmap::setup_storage;
