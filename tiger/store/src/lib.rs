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
    Account, AccountBeforeTx, BlockBodyIndices, BytecodeCodec, HeaderCodec, StageCheckpointCodec, StorageBeforeTx, CodecError, Decoded, Flags, Reader, StorageEntry, TrieCodec,
    put_compact_u64, put_compact_u128, put_compact_u256, put_varuint,
};

mod tables;
pub use tables::{DB_VERSION, DB_VERSION_FILE, Db, OpenError, Table};

mod static_files;
pub use static_files::{BLOCKS_PER_FILE, Jar, JarConfig, Lz4, Range, Segment, SegmentHeader, StaticFiles};

mod rocks;
pub use rocks::{BlockList, History, Rocks, RocksBatch, RocksError, RocksTable, SHARD_SIZE};

mod datadir;
pub use datadir::{Datadir, STAGES, STORAGE_SETTINGS, STORAGE_SETTINGS_KEY, StoreError, WriteTxn, put_scale_bytes, scale_bytes};

mod block_codec;
pub use block_codec::{ReceiptCodec, TxCodec};
