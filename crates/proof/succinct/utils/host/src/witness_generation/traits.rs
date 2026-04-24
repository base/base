use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use base_proof::{CachingOracle, OracleBlobProvider, OracleL1ChainProvider, OracleL2ChainProvider};
use base_proof_preimage::{HintWriter, NativeChannel, OracleReader};
use base_proof_succinct_client_utils::{
    client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL,
    witness::{
        BlobData, WitnessData,
        executor::{WitnessExecutor, get_inputs_for_pipeline},
        preimage_store::PreimageStore,
    },
};
use sp1_sdk::SP1Stdin;

use crate::witness_generation::{OnlineBlobStore, PreimageWitnessCollector};

/// Default oracle type backed by native preimage channels.
pub type DefaultOracleBase = CachingOracle<OracleReader<NativeChannel>, HintWriter<NativeChannel>>;

/// Generates witness data by driving derivation and execution pipelines.
#[async_trait]
pub trait WitnessGenerator {
    /// Output witness data type.
    type WitnessData: WitnessData;
    /// Executor that creates and runs the derivation pipeline.
    type WitnessExecutor: WitnessExecutor<
            O = PreimageWitnessCollector<DefaultOracleBase>,
            B = OnlineBlobStore<OracleBlobProvider<DefaultOracleBase>>,
            L1 = OracleL1ChainProvider<PreimageWitnessCollector<DefaultOracleBase>>,
            L2 = OracleL2ChainProvider<PreimageWitnessCollector<DefaultOracleBase>>,
        > + Sync
        + Send;

    /// Return a reference to the witness executor.
    fn get_executor(&self) -> &Self::WitnessExecutor;

    /// Run witness generation over the given preimage and hint channels.
    async fn run(
        &self,
        preimage_chan: NativeChannel,
        hint_chan: NativeChannel,
    ) -> Result<Self::WitnessData> {
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

        let (boot_info, input, _safe_head_number) =
            get_inputs_for_pipeline(Arc::clone(&oracle)).await?;
        if let Some((cursor, l1_provider, l2_provider)) = input {
            let rollup_config = Arc::new(boot_info.rollup_config.clone());
            let l1_config = Arc::new(boot_info.l1_config.clone());
            let pipeline = self
                .get_executor()
                .create_pipeline(
                    rollup_config,
                    l1_config,
                    Arc::clone(&cursor),
                    Arc::clone(&oracle),
                    beacon,
                    l1_provider.clone(),
                    l2_provider.clone(),
                )
                .await
                .unwrap();
            let _ = self
                .get_executor()
                .run(boot_info, pipeline, cursor, l2_provider, DEFAULT_INTERMEDIATE_ROOT_INTERVAL)
                .await?;
        }

        let witness = Self::WitnessData::from_parts(
            preimage_witness_store.lock().unwrap().clone(),
            blob_data.lock().unwrap().clone(),
        );

        Ok(witness)
    }

    /// Build SP1 stdin from the collected witness data.
    fn get_sp1_stdin(
        &self,
        witness: Self::WitnessData,
        intermediate_root_interval: u64,
    ) -> Result<SP1Stdin>;
}
