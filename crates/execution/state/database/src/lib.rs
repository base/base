#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod implementation;
pub mod lockfile;
#[cfg(feature = "mdbx")]
mod metrics;
pub mod static_file;
pub use static_file::KeyOrNumber;
#[cfg(feature = "mdbx")]
mod utils;
pub mod version;

#[cfg(feature = "mdbx")]
pub mod mdbx;
#[cfg(feature = "mdbx")]
mod native_mdbx;

pub use base_execution_state_types::{DatabaseError, DatabaseWriteOperation};
#[cfg(feature = "mdbx")]
pub use mdbx::{DatabaseEnv, DatabaseEnvKind, create_db, init_db, open_db, open_db_read_only};
pub use models::ClientVersion;
extern crate alloc;

mod common;
pub use common::*;
mod cursor;
pub use cursor::*;
mod database;
pub use database::*;
mod transaction;
pub use transaction::*;
mod database_metrics;
pub use database_metrics::*;
mod mock;
pub use mock::*;
mod table;
pub use table::*;
pub mod tables;
pub use tables::*;
mod codec_utils;
pub mod models;
mod unwind;
pub use unwind::DbTxUnwindExt;
#[cfg(feature = "mdbx")]
pub use utils::is_database_empty;

/// Temporary databases and database fixtures.
#[cfg(all(feature = "mdbx", any(test, feature = "test-utils")))]
pub mod test_utils;

mod nippy_jar;
pub use nippy_jar::{
    CHANGESET_OFFSETS_FILE_EXTENSION, CONFIG_FILE_EXTENSION, ColumnResult,
    Compression as NippyCompression, Compressors as NippyCompressors, DataReader,
    DecoderDictionary as NippyDecoderDictionary, Decompressor as NippyDecompressor,
    Functions as NippyJarFunctions, InclusionFilters as NippyJarInclusionFilters, Lz4 as NippyLz4,
    NippyJar, NippyJarChecker, NippyJarCursor, NippyJarError, NippyJarHeader, NippyJarWriter,
    Zstd as NippyZstd, ZstdState as NippyZstdState,
};
