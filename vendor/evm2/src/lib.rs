#![doc = include_str!("../README.md")]
#![cfg_attr(tco, feature(explicit_tail_calls), allow(incomplete_features))]
#![cfg_attr(all(tco, not(cranelift)), feature(rust_preserve_none_cc))]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate self as evm2;

extern crate alloc;

pub mod bytecode;
pub mod constants;
mod error;
pub mod ethereum;
pub mod interpreter;
pub mod utils;
pub use error::{AnyError, ErrorCode};

pub mod evm;
#[doc(hidden)]
pub use evm::config::EvmTypesHost;
pub use evm::{
    AccountInfo, BlockStateAccumulator, Evm, ExecutedTx, InterpreterRunner, JournalEntry,
    PendingState, TxResult, TxResultExt, TxResultWithState, config,
    config::{
        BaseEvmConfig, BaseEvmConfigSelector, BaseEvmTypes, EvmConfig, EvmConfigSelector, EvmTypes,
        ExecutionConfig,
    },
    env, handler, inspector, precompile, registry,
};
pub use inspector::{Inspector, NoopInspector};

pub mod precompiles;
pub use precompiles::{
    Crypto, PrecompileError, PrecompileHalt, Precompiles, crypto, install_crypto,
};

pub(crate) mod trustme;

#[cfg(test)]
pub(crate) mod test_utils;

pub mod version;
pub use version::{EvmFeatures, OpcodeConfig, Version};

mod spec_id;
pub use spec_id::SpecId;

mod once_lock;
mod storage_key;
pub use storage_key::{StorageKey, StorageKeyMap, StorageKeySet};

#[cfg(test)]
mod tests;
