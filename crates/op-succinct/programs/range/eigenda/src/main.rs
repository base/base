//! A program to verify a Optimism L2 block STF with Ethereum DA in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a range of blocks, which can be used to generate an on chain validity proof. Depending on
//! the compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In
//! native mode, the data for verifying the batch validity is fetched from RPC, while in zkVM mode,
//! the data is supplied by the host binary to the verifiable program.

#![no_main]
sp1_zkvm::entrypoint!(main);

use canoe_sp1_cc_verifier::CanoeSp1CCVerifier;
use canoe_verifier_address_fetcher::CanoeVerifierAddressFetcherDeployedByEigenLabs;
use hokulea_proof::eigenda_witness::EigenDAWitness;
use hokulea_zkvm_verification::eigenda_witness_to_preloaded_provider;
use op_succinct_client_utils::witness::{EigenDAWitnessData, WitnessData};
use op_succinct_eigenda_client_utils::executor::EigenDAWitnessExecutor;
use op_succinct_range_utils::run_range_program;
#[cfg(feature = "tracing-subscriber")]
use op_succinct_range_utils::setup_tracing;
use rkyv::rancor::Error;

fn main() {
    #[cfg(feature = "tracing-subscriber")]
    setup_tracing();

    kona_proof::block_on(async move {
        let witness_rkyv_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
        let witness_data = rkyv::from_bytes::<EigenDAWitnessData, Error>(&witness_rkyv_bytes)
            .expect("Failed to deserialize witness data.");

        let (oracle, _beacon) = witness_data.clone().get_oracle_and_blob_provider().await.unwrap();
        let eigenda_witness: EigenDAWitness = serde_cbor::from_slice(
            &witness_data.eigenda_data.clone().expect("eigenda witness data is not present"),
        )
        .expect("cannot deserialize eigenda witness");
        let preloaded_preimage_provider = eigenda_witness_to_preloaded_provider(
            oracle,
            CanoeSp1CCVerifier {},
            CanoeVerifierAddressFetcherDeployedByEigenLabs {},
            eigenda_witness,
        )
        .await
        .expect("Failed to get preloaded blob provider");

        run_range_program(EigenDAWitnessExecutor::new(preloaded_preimage_provider), witness_data)
            .await;
    });
}
