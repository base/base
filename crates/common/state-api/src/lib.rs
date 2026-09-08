#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "reth-codec")]
extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub use revm_primitives as primitives;
pub use revm_state as state;

mod account;
pub use account::{
    Account, Bytecode, EIP7702_BYTECODE_ID, LEGACY_ANALYZED_BYTECODE_ID, LEGACY_RAW_BYTECODE_ID,
    REMOVED_BYTECODE_ID,
};

mod database;
pub use database::{
    BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BENCH_TARGET_BALANCE, DBErrorMarker,
    Database, DatabaseCommit, DatabaseCommitExt, DatabaseRef, EEADDRESS, FFADDRESS, TEST_BALANCE,
    WrapDatabaseRef,
};

#[cfg(feature = "asyncdb")]
mod async_db;
#[cfg(feature = "asyncdb")]
pub use async_db::{DatabaseAsync, DatabaseAsyncRef, WrapDatabaseAsync};

mod bal;
pub use bal::{BalDatabase, BalState, EvmDatabaseError};

mod either;

mod empty_db;
pub use empty_db::{EmptyDB, EmptyDBTyped};

mod erased_error;
pub use erased_error::ErasedError;

mod state_hook;
pub use state_hook::{NoopHook, OnStateHook};

mod try_commit;
pub use try_commit::{ArcUpgradeError, TryDatabaseCommit};

mod cached;
pub use cached::{CachedAccount, CachedReads, CachedReadsDbMut};
