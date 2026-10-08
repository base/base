//! A program to verify a Base L2 block STF with Ethereum DA in the zkVM.
//!
//! OpenVM analog of the Succinct range guest. Same rkyv witness encoding and
//! `base-proof-zk-utils` STF; I/O goes through `openvm::io` instead of
//! `sp1_zkvm::io`.

use base_proof_openvm_range_utils::run_range_program;
#[cfg(feature = "tracing-subscriber")]
use base_proof_openvm_range_utils::setup_tracing;
use base_proof_zk_utils::witness::{DefaultWitnessData, WitnessData};
// std guests use a normal `fn main()`. `openvm::entry!` is a no-op when std is
// on; `#![no_main]` then leaves RISC-V `_start` with no `main` symbol.
use openvm::{self as _, io::read_vec};
use rkyv::rancor::Error;

fn main() {
    #[cfg(feature = "tracing-subscriber")]
    setup_tracing();

    base_proof::block_on(async move {
        let witness_rkyv_bytes: Vec<u8> = read_vec();
        let witness_data = rkyv::from_bytes::<DefaultWitnessData, Error>(&witness_rkyv_bytes)
            .expect("Failed to deserialize witness data.");

        let (oracle, beacon) = witness_data
            .get_oracle_and_blob_provider()
            .await
            .expect("Failed to load oracle and blob provider");

        run_range_program(oracle, beacon).await;
    });
}
