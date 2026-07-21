//! Witness generation for Succinct proof pipelines.

/// Collects witness data by driving derivation and execution pipelines.
pub mod generator;
pub use generator::{DefaultOracleBase, WitnessGenerator};

/// Blob store that records blobs fetched online.
pub mod online_blob_store;
pub use online_blob_store::OnlineBlobStore;

/// Preimage oracle wrapper that collects witness data.
pub mod preimage_witness_collector;
pub use base_proof_succinct_client_utils::witness::DefaultWitnessData;
pub use preimage_witness_collector::PreimageWitnessCollector;
