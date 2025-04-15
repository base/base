//! A program to verify a Optimism L2 block STF in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a range of blocks, which can be used to generate an on chain validity proof. Depending on
//! the compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In
//! native mode, the data for verifying the batch validity is fetched from RPC, while in zkVM mode,
//! the data is supplied by the host binary to the verifiable program.

#![no_main]
sp1_zkvm::entrypoint!(main);

use op_succinct_client_utils::{
    boot::BootInfoStruct, client::run_witness_client, witness::WitnessData,
};
use rkyv::rancor::Error;

fn main() {
    #[cfg(feature = "tracing-subscriber")]
    {
        use anyhow::anyhow;
        use tracing::Level;

        let subscriber = tracing_subscriber::fmt().with_max_level(Level::INFO).finish();
        tracing::subscriber::set_global_default(subscriber).map_err(|e| anyhow!(e)).unwrap();
    }

    kona_proof::block_on(async move {
        ////////////////////////////////////////////////////////////////
        //                          PROLOGUE                          //
        ////////////////////////////////////////////////////////////////
        let witness_rkyv_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
        let witness_data = rkyv::from_bytes::<WitnessData, Error>(&witness_rkyv_bytes)
            .expect("Failed to deserialize witness data.");

        let boot_info = run_witness_client(witness_data)
            .await
            .expect("Failed to run client with witness data.");

        sp1_zkvm::io::commit(&BootInfoStruct::from(boot_info));
    });
}
