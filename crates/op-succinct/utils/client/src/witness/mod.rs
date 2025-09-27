pub mod executor;
pub mod preimage_store;

use std::{fmt::Debug, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use kzg_rs::{Blob, Bytes48};
use preimage_store::PreimageStore;
use serde::{Deserialize, Serialize};

use crate::BlobStore;

#[async_trait]
pub trait WitnessData: Sized {
    /// Creates a new WitnessData from the given preimage store and blob data.
    fn from_parts(preimage_store: PreimageStore, blob_data: BlobData) -> Self;

    /// Consumes the WitnessData to extract its core components.
    fn into_parts(self) -> (PreimageStore, BlobData);

    /// Gets the oracle and blob provider from the witness data and validates the correctness of the
    /// preimages.
    async fn get_oracle_and_blob_provider(self) -> Result<(Arc<PreimageStore>, BlobStore)> {
        let (owned_preimage_store, owned_blob_data) = self.into_parts();

        println!("cycle-tracker-report-start: oracle-verify");
        // Check the preimages in the witness are valid.
        owned_preimage_store.check_preimages().expect("Failed to validate preimages");
        println!("cycle-tracker-report-end: oracle-verify");

        // Create an Arc of the preimage store.
        let oracle = Arc::new(owned_preimage_store);

        // Create a BlobStore from the blobs in the witness and verifies them for correctness.
        println!("cycle-tracker-report-start: blob-verification");
        let beacon = BlobStore::from(owned_blob_data);
        println!("cycle-tracker-report-end: blob-verification");

        Ok((oracle, beacon))
    }
}

#[derive(Clone, Debug, Default, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct DefaultWitnessData {
    pub preimage_store: PreimageStore,
    pub blob_data: BlobData,
}

#[async_trait]
impl WitnessData for DefaultWitnessData {
    fn from_parts(preimage_store: PreimageStore, blob_data: BlobData) -> Self {
        Self { preimage_store, blob_data }
    }

    fn into_parts(self) -> (PreimageStore, BlobData) {
        (self.preimage_store, self.blob_data)
    }
}

#[derive(Clone, Debug, Default, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct EigenDAWitnessData {
    pub preimage_store: PreimageStore,
    pub blob_data: BlobData,
    // EigenDAWitness.
    // See https://github.com/Layr-Labs/hokulea/blob/0a6200cd0f22caa28aca040e47860dd42893ae26/crates/proof/src/eigenda_blob_witness.rs.
    pub eigenda_data: Option<Vec<u8>>,
}

#[async_trait]
impl WitnessData for EigenDAWitnessData {
    fn from_parts(preimage_store: PreimageStore, blob_data: BlobData) -> Self {
        Self { preimage_store, blob_data, eigenda_data: None }
    }

    fn into_parts(self) -> (PreimageStore, BlobData) {
        (self.preimage_store, self.blob_data)
    }
}

#[derive(
    Clone, Debug, Default, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct BlobData {
    pub blobs: Vec<Blob>,
    pub commitments: Vec<Bytes48>,
    pub proofs: Vec<Bytes48>,
}
