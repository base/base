#![doc = include_str!("../README.md")]

#[allow(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    unused_imports,
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod mdbx_sys;

mod mdbx;
pub use mdbx::{Cursor, Dbi, Entry, Env, EnvInner, EnvOptions, MdbxError, Ro, Rw, Txn};

mod fault;
pub use fault::{Fault, FaultConfig, FaultMode};

mod codec;
pub use codec::{
    Account, AccountBeforeTx, BytecodeCodec, CodecError, Decoded, Flags, Reader, StorageEntry, TrieCodec,
    put_compact_u64, put_compact_u128, put_compact_u256, put_varuint,
};

mod tables;
pub use tables::{DB_VERSION, DB_VERSION_FILE, Db, OpenError, Table};
