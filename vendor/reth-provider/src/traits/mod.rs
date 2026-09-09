//! Collection of common provider traits.

// Re-export all the traits
pub use base_execution_chainspec::ChainSpecProvider;
pub use reth_storage_api::*;

mod static_file_provider;
pub use static_file_provider::StaticFileProviderFactory;

mod rocksdb_provider;
pub use rocksdb_provider::RocksDBProviderFactory;

