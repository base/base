//! A program to verify a Optimism L2 block STF in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a range of blocks, which can be used to generate an on chain validity proof. Depending on
//! the compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In
//! native mode, the data for verifying the batch validity is fetched from RPC, while in zkVM mode,
//! the data is supplied by the host binary to the verifiable program.

#![no_main]
sp1_zkvm::entrypoint!(main);

extern crate alloc;

use alloc::sync::Arc;

use alloy_primitives::B256;
use kona_preimage::{CommsClient, PreimageKeyType};
use kona_proof::{errors::OracleProviderError, BootInfo, HintType};
use op_succinct_client_utils::{
    boot::BootInfoStruct, client::run_opsuccinct_client, precompiles::zkvm_handle_register,
};

use alloc::vec::Vec;
use op_succinct_client_utils::InMemoryOracle;

fn main() {
    #[cfg(feature = "tracing-subscriber")]
    {
        use anyhow::anyhow;
        use tracing::Level;

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .map_err(|e| anyhow!(e))
            .unwrap();
    }

    kona_proof::block_on(async move {
        ////////////////////////////////////////////////////////////////
        //                          PROLOGUE                          //
        ////////////////////////////////////////////////////////////////
        let in_memory_oracle_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
        let oracle = Arc::new(InMemoryOracle::from_raw_bytes(in_memory_oracle_bytes));

        println!("cycle-tracker-report-start: oracle-verify");
        oracle.verify().expect("key value verification failed");
        println!("cycle-tracker-report-end: oracle-verify");

        let boot_info = run_opsuccinct_client(oracle, Some(zkvm_handle_register))
            .await
            .expect("failed to run client");

        sp1_zkvm::io::commit(&BootInfoStruct::from(boot_info));
    });
}

/// Fetches the safe head hash of the L2 chain based on the agreed upon L2 output root in the
/// [BootInfo].
///
/// Sourced from Kona until it's exposed nicely from a crate that doesn't depend on kona-std-fpvm, which can compile in zkVM mode.
/// https://github.com/op-rs/kona/blob/a59f643d0627320efff49f40f4803741ae9194f1/bin/client/src/single.rs#L153-L155.
pub async fn fetch_safe_head_hash<O>(
    caching_oracle: &O,
    boot_info: &BootInfo,
) -> Result<B256, OracleProviderError>
where
    O: CommsClient,
{
    let mut output_preimage = [0u8; 128];
    HintType::StartingL2Output
        .get_exact_preimage(
            caching_oracle,
            boot_info.agreed_l2_output_root,
            PreimageKeyType::Keccak256,
            &mut output_preimage,
        )
        .await?;

    output_preimage[96..128]
        .try_into()
        .map_err(OracleProviderError::SliceConversion)
}
