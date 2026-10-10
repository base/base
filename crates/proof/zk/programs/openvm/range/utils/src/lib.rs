//! Utilities for running the OpenVM range program.

use std::sync::Arc;

use alloy_primitives::keccak256;
use alloy_sol_types::SolValue;
use base_proof_zk_utils::{
    BlobStore,
    boot::BootInfoStruct,
    witness::{WitnessExecutor, executor::get_inputs_for_pipeline, preimage_store::WitnessOracle},
};
use openvm::io::reveal_bytes32;

/// Sets up tracing for the range program
#[cfg(feature = "tracing-subscriber")]
pub fn setup_tracing() {
    use anyhow::anyhow;
    use tracing::Level;

    let subscriber = tracing_subscriber::fmt().with_max_level(Level::INFO).finish();
    tracing::subscriber::set_global_default(subscriber).map_err(|e| anyhow!(e)).unwrap();
}

/// Keccak-256 of the ABI-encoded boot info, used as the 32-byte OpenVM public value.
///
/// SP1 commits the full `BootInfoStruct` via `sp1_zkvm::io::commit`. OpenVM public
/// values are a single `bytes32`, so this is the digest the host/verifier should check.
pub fn public_values_digest(boot: &BootInfoStruct) -> [u8; 32] {
    keccak256(boot.abi_encode()).into()
}

/// Runs the range program.
pub async fn run_range_program(oracle: Arc<WitnessOracle>, beacon: BlobStore) {
    let executor = WitnessExecutor::<WitnessOracle, BlobStore>::new();

    let (boot_info, input, l2_pre_block_number) =
        get_inputs_for_pipeline(Arc::clone(&oracle)).await.unwrap();
    let (cursor, l1_provider, l2_provider) = input;
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

    let (boot_info, l2_block_number, intermediate_roots) =
        executor.run(boot_info, pipeline, cursor, l2_provider).await.unwrap();

    let boot =
        BootInfoStruct::new(boot_info, l2_pre_block_number, l2_block_number, intermediate_roots);
    reveal_bytes32(public_values_digest(&boot));
}
