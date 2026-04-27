//! Utilities for running the range program.

use std::sync::Arc;

use base_proof::{OracleL1ChainProvider, OracleL2ChainProvider};
use base_proof_succinct_client_utils::{
    BlobStore,
    boot::BootInfoStruct,
    witness::{
        executor::{WitnessExecutor, get_inputs_for_pipeline},
        preimage_store::PreimageStore,
    },
};

/// Sets up tracing for the range program
#[cfg(feature = "tracing-subscriber")]
pub fn setup_tracing() {
    use anyhow::anyhow;
    use tracing::Level;

    let subscriber = tracing_subscriber::fmt().with_max_level(Level::INFO).finish();
    tracing::subscriber::set_global_default(subscriber).map_err(|e| anyhow!(e)).unwrap();
}

/// Runs the range program.
pub async fn run_range_program<E>(
    executor: E,
    oracle: Arc<PreimageStore>,
    beacon: BlobStore,
    intermediate_root_interval: u64,
) where
    E: WitnessExecutor<
            O = PreimageStore,
            B = BlobStore,
            L1 = OracleL1ChainProvider<PreimageStore>,
            L2 = OracleL2ChainProvider<PreimageStore>,
        > + Send
        + Sync,
{
    ////////////////////////////////////////////////////////////////
    //                          PROLOGUE                          //
    ////////////////////////////////////////////////////////////////
    let (boot_info, input, l2_pre_block_number) =
        get_inputs_for_pipeline(Arc::clone(&oracle)).await.unwrap();
    let (boot_info, intermediate_roots) = match input {
        Some((cursor, l1_provider, l2_provider)) => {
            let rollup_config = Arc::new(boot_info.rollup_config.clone());
            let l1_config = Arc::new(boot_info.l1_config.clone());

            let pipeline = executor
                .create_pipeline(
                    rollup_config,
                    l1_config,
                    Arc::clone(&cursor),
                    oracle,
                    beacon,
                    l1_provider,
                    l2_provider.clone(),
                )
                .await
                .unwrap();

            executor
                .run(boot_info, pipeline, cursor, l2_provider, intermediate_root_interval)
                .await
                .unwrap()
        }
        None => (boot_info, Vec::new()),
    };

    sp1_zkvm::io::commit(&BootInfoStruct::new(boot_info, l2_pre_block_number, intermediate_roots));
}
