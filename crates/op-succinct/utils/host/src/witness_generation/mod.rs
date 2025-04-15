mod online_blob_store;
mod preimage_witness_collector;

use anyhow::Result;
use kona_derive::prelude::BlobProvider;
use kona_preimage::CommsClient;
use kona_proof::{BootInfo, FlushableCache};
use online_blob_store::OnlineBlobStore;
use op_succinct_client_utils::{
    client::run_opsuccinct_client,
    witness::{preimage_store::PreimageStore, BlobData, WitnessData},
};
use preimage_witness_collector::PreimageWitnessCollector;
use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

/// Generate a witness with the given oracle and blob provider.
pub async fn generate_opsuccinct_witness<O, B>(
    preimage_oracle: Arc<O>,
    blob_provider: B,
) -> Result<(BootInfo, WitnessData)>
where
    O: CommsClient + FlushableCache + Send + Sync + Debug,
    B: BlobProvider + Send + Sync + Debug + Clone,
{
    let preimage_witness_store = Arc::new(Mutex::new(PreimageStore::default()));
    let blob_data = Arc::new(Mutex::new(BlobData::default()));

    let oracle = Arc::new(PreimageWitnessCollector {
        preimage_oracle: preimage_oracle.clone(),
        preimage_witness_store: preimage_witness_store.clone(),
    });
    let beacon = OnlineBlobStore { provider: blob_provider.clone(), store: blob_data.clone() };

    let boot = run_opsuccinct_client(oracle, beacon).await?;

    let witness = WitnessData {
        preimage_store: preimage_witness_store.lock().unwrap().clone(),
        blob_data: blob_data.lock().unwrap().clone(),
    };

    Ok((boot, witness))
}
