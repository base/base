//! Contains the host <-> client communication utilities.

mod in_memory_oracle;
pub use in_memory_oracle::InMemoryOracle;

mod store_oracle;
pub use store_oracle::StoreOracle;

mod blob_provider;
pub use blob_provider::OPSuccinctOracleBlobProvider;
