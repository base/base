use std::sync::Arc;

use kona_proof::{l1::OracleL1ChainProvider, l2::OracleL2ChainProvider};
use op_succinct_client_utils::{
    boot::BootInfoStruct,
    witness::{
        executor::{get_inputs_for_pipeline, WitnessExecutor},
        preimage_store::PreimageStore,
        WitnessData,
    },
    BlobStore,
};

/// Sets up tracing for the range program
#[cfg(feature = "tracing-subscriber")]
pub fn setup_tracing() {
    use anyhow::anyhow;
    use tracing::Level;

    let subscriber = tracing_subscriber::fmt().with_max_level(Level::INFO).finish();
    tracing::subscriber::set_global_default(subscriber).map_err(|e| anyhow!(e)).unwrap();
}

pub async fn run_range_program<E, W>(executor: E, witness_data: W)
where
    E: WitnessExecutor<
            O = PreimageStore,
            B = BlobStore,
            L1 = OracleL1ChainProvider<PreimageStore>,
            L2 = OracleL2ChainProvider<PreimageStore>,
        > + Send
        + Sync,
    W: WitnessData + Send + Sync,
{
    ////////////////////////////////////////////////////////////////
    //                          PROLOGUE                          //
    ////////////////////////////////////////////////////////////////
    let (oracle, beacon) = witness_data.get_oracle_and_blob_provider().await.unwrap();

    let (boot_info, input) = get_inputs_for_pipeline(oracle.clone()).await.unwrap();
    let boot_info = match input {
        Some((cursor, l1_provider, l2_provider)) => {
            let rollup_config = Arc::new(boot_info.rollup_config.clone());
            let l1_config = Arc::new(boot_info.l1_config.clone());

            let pipeline = executor
                .create_pipeline(
                    rollup_config,
                    l1_config,
                    cursor.clone(),
                    oracle,
                    beacon,
                    l1_provider,
                    l2_provider.clone(),
                )
                .await
                .unwrap();

            executor.run(boot_info, pipeline, cursor, l2_provider).await.unwrap()
        }
        None => boot_info,
    };

    sp1_zkvm::io::commit(&BootInfoStruct::from(boot_info));
}
