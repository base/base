//! A program to verify a Optimism L2 block STF with Ethereum DA in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a range of blocks, which can be used to generate an on chain validity proof. Depending on
//! the compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In
//! native mode, the data for verifying the batch validity is fetched from RPC, while in zkVM mode,
//! the data is supplied by the host binary to the verifiable program.

#![no_main]
sp1_zkvm::entrypoint!(main);

use op_succinct_client_utils::witness::{DefaultWitnessData, WitnessData};
use op_succinct_ethereum_client_utils::executor::ETHDAWitnessExecutor;
use op_succinct_range_utils::run_range_program;
#[cfg(feature = "tracing-subscriber")]
use op_succinct_range_utils::setup_tracing;
use rkyv::rancor::Error;

fn main() {
    #[cfg(feature = "tracing-subscriber")]
    setup_tracing();

    kona_proof::block_on(async move {
        let witness_rkyv_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
        let witness_data = rkyv::from_bytes::<DefaultWitnessData, Error>(&witness_rkyv_bytes)
            .expect("Failed to deserialize witness data.");

        let (oracle, beacon) = witness_data
            .get_oracle_and_blob_provider()
            .await
            .expect("Failed to load oracle and blob provider");

        run_range_program(ETHDAWitnessExecutor::new(), oracle, beacon).await;
    });
}
