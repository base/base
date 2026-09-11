//! Base execution accounts, memory databases, journals, and state transitions.

pub(crate) use crate::Bytecode;

mod stored_account;
pub use stored_account::{
    EIP7702_BYTECODE_ID, LEGACY_ANALYZED_BYTECODE_ID, LEGACY_RAW_BYTECODE_ID, REMOVED_BYTECODE_ID,
    StoredAccount, StoredBytecode,
};

mod database;
pub use database::{
    BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BENCH_TARGET_BALANCE, DBErrorMarker,
    Database, DatabaseCommit, DatabaseCommitExt, DatabaseRef, EEADDRESS, FFADDRESS, TEST_BALANCE,
    WrapDatabaseRef,
};

mod bal_database;
pub use bal_database::{BalDatabase, BalState, EvmDatabaseError};

mod either;

mod empty_db;
pub use empty_db::{EmptyDB, EmptyDBTyped};

mod erased_error;
pub use erased_error::ErasedError;

mod state_hook;
pub use state_hook::OnStateHook;

mod cached;
pub use cached::{CachedAccount, CachedReads, CachedReadsDbMut};

mod in_memory_db;
pub use in_memory_db::{AccountState, BenchmarkDB, Cache, CacheDB, DbAccount, InMemoryDB};

mod states;
pub use states::*;

mod account_info;
pub use account_info::{AccountId, AccountInfo};

mod types;
pub use types::{EvmState, EvmStorage, TransientStorage};

mod journal_account;
pub use journal_account::{Account, EvmStorageSlot, JournalAccountStatus, TransactionId};

/// Block access lists and state-change indexing.
pub mod bal;
