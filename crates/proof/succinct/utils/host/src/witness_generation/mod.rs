//! Witness generation traits and collectors.

mod traits;
pub use traits::{DefaultOracleBase, WitnessGenerator};

mod online_blob_store;
pub use online_blob_store::OnlineBlobStore;

mod preimage_witness_collector;
pub use preimage_witness_collector::PreimageWitnessCollector;
