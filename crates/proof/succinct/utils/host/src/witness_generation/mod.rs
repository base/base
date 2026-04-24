//! Witness generation traits and collectors.

/// Core witness generation trait and type aliases.
pub mod traits;
pub use traits::{DefaultOracleBase, WitnessGenerator};

/// Blob store that records blobs fetched online.
pub mod online_blob_store;
pub use online_blob_store::OnlineBlobStore;

/// Preimage oracle wrapper that collects witness data.
pub mod preimage_witness_collector;
pub use preimage_witness_collector::PreimageWitnessCollector;
