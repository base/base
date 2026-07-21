//! Witness generation for Succinct proof pipelines.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use base_proof::{CachingOracle, OracleBlobProvider};
use base_proof_preimage::{HintWriter, NativeChannel, OracleReader};
use base_proof_succinct_client_utils::witness::{
    BlobData, DefaultWitnessData, WitnessData, WitnessExecutor, executor::get_inputs_for_pipeline,
    preimage_store::PreimageStore,
};
use rkyv::to_bytes;
use sp1_sdk::SP1Stdin;

use crate::witness_generation::{OnlineBlobStore, PreimageWitnessCollector};

/// Oracle type backed by native preimage channels during host-side witness collection.
pub type DefaultOracleBase = CachingOracle<OracleReader<NativeChannel>, HintWriter<NativeChannel>>;

/// Collects witness data by driving derivation and execution pipelines.
#[derive(Debug, Default)]
pub struct WitnessGenerator;

impl WitnessGenerator {
    /// Create a new witness generator.
    pub const fn new() -> Self {
        Self
    }

    /// Run witness generation over the given preimage and hint channels.
    pub async fn run(
        &self,
        preimage_chan: NativeChannel,
        hint_chan: NativeChannel,
    ) -> Result<DefaultWitnessData> {
        let preimage_witness_store = Arc::new(Mutex::new(PreimageStore::default()));
        let blob_data = Arc::new(Mutex::new(BlobData::default()));

        let preimage_oracle = Arc::new(CachingOracle::new(
            2048,
            OracleReader::new(preimage_chan),
            HintWriter::new(hint_chan),
        ));
        let blob_provider = OracleBlobProvider::new(Arc::clone(&preimage_oracle));

        let oracle = Arc::new(PreimageWitnessCollector {
            preimage_oracle: Arc::clone(&preimage_oracle),
            preimage_witness_store: Arc::clone(&preimage_witness_store),
        });
        let beacon =
            OnlineBlobStore { provider: blob_provider.clone(), store: Arc::clone(&blob_data) };

        let (boot_info, (cursor, l1_provider, l2_provider), _safe_head_number) =
            get_inputs_for_pipeline(Arc::clone(&oracle)).await?;
        let rollup_config = Arc::new(boot_info.rollup_config.clone());
        let l1_config = Arc::new(boot_info.l1_config.clone());
        let executor = WitnessExecutor::new();
        let pipeline = executor
            .create_pipeline(
                rollup_config,
                l1_config,
                Arc::clone(&cursor),
                Arc::clone(&oracle),
                beacon,
                l1_provider.clone(),
                l2_provider.clone(),
            )
            .await?;
        let _ = executor.run(boot_info, pipeline, cursor, l2_provider).await?;

        Ok(DefaultWitnessData::from_parts(
            preimage_witness_store.lock().unwrap().clone(),
            blob_data.lock().unwrap().clone(),
        ))
    }

    /// Build SP1 stdin from the collected witness data.
    ///
    /// The intermediate root sampling interval is sourced from `BootInfo` inside the zkVM
    /// (preimage key 9) — the same channel the TEE enclave reads — so it is intentionally not
    /// passed through stdin.
    pub fn get_sp1_stdin(&self, witness: DefaultWitnessData) -> Result<SP1Stdin> {
        let mut stdin = SP1Stdin::default();
        let buffer = to_bytes::<rkyv::rancor::Error>(&witness)?;
        stdin.write_slice(&buffer);
        Ok(stdin)
    }
}
