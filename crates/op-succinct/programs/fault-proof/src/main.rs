//! A program to verify a OP Stack chain's block STF in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a single block, which can be used in an on chain dispute game. Depending on the
//! compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In native
//! mode, the data for verifying the execute of the Optimism rollup's state transition is fetched
//! from RPC, while in zkVM mode, the data is supplied by the host binary to the verifiable program.

#![cfg_attr(target_os = "zkvm", no_main)]

extern crate alloc;

use alloc::sync::Arc;

use cfg_if::cfg_if;
use kona_client::{
    l1::{DerivationDriver, OracleBlobProvider, OracleL1ChainProvider},
    l2::OracleL2ChainProvider,
    BootInfo,
};
use op_succinct_client_utils::precompiles::zkvm_handle_register;

cfg_if! {
    if #[cfg(target_os = "zkvm")] {
        sp1_zkvm::entrypoint!(main);
        use op_succinct_client_utils::{InMemoryOracle, boot::BootInfoStruct, BootInfoWithBytesConfig};
        use op_alloy_genesis::RollupConfig;
        use alloc::vec::Vec;
        use serde_json;
    } else {
        use kona_client::CachingOracle;
        use op_succinct_client_utils::pipes::{ORACLE_READER, HINT_WRITER};
    }
}

fn main() {
    op_succinct_client_utils::block_on(async move {
        ////////////////////////////////////////////////////////////////
        //                          PROLOGUE                          //
        ////////////////////////////////////////////////////////////////

        cfg_if! {
            // If we are compiling for the zkVM, read inputs from SP1 to generate boot info
            // and in memory oracle.
            if #[cfg(target_os = "zkvm")] {
                println!("cycle-tracker-start: boot-load");
                let boot_info_with_bytes_config = sp1_zkvm::io::read::<BootInfoWithBytesConfig>();

                // BootInfoStruct is identical to BootInfoWithBytesConfig, except it replaces
                // the rollup_config_bytes with a hash of those bytes (rollupConfigHash). Securely
                // hashes the rollup config bytes.
                let boot_info_struct = BootInfoStruct::from(boot_info_with_bytes_config.clone());
                sp1_zkvm::io::commit::<BootInfoStruct>(&boot_info_struct);

                let rollup_config: RollupConfig = serde_json::from_slice(&boot_info_with_bytes_config.rollup_config_bytes).expect("failed to parse rollup config");
                let boot: Arc<BootInfo> = Arc::new(BootInfo {
                    l1_head: boot_info_with_bytes_config.l1_head,
                    agreed_l2_output_root: boot_info_with_bytes_config.l2_output_root,
                    claimed_l2_output_root: boot_info_with_bytes_config.l2_claim,
                    claimed_l2_block_number: boot_info_with_bytes_config.l2_claim_block,
                    chain_id: boot_info_with_bytes_config.chain_id,
                    rollup_config,
                });
                println!("cycle-tracker-end: boot-load");

                println!("cycle-tracker-start: oracle-load");
                let in_memory_oracle_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
                let oracle = Arc::new(InMemoryOracle::from_raw_bytes(in_memory_oracle_bytes));
                println!("cycle-tracker-end: oracle-load");

                println!("cycle-tracker-start: oracle-verify");
                oracle.verify().expect("key value verification failed");
                println!("cycle-tracker-end: oracle-verify");
            }
            // If we are compiling for online mode, create a caching oracle that speaks to the
            // fetcher via hints, and gather boot info from this oracle.
            else {
                let oracle = Arc::new(CachingOracle::new(1024, ORACLE_READER, HINT_WRITER));
                let boot = Arc::new(BootInfo::load(oracle.as_ref()).await.unwrap());
            }
        }

        let l1_provider = OracleL1ChainProvider::new(boot.clone(), oracle.clone());
        let l2_provider = OracleL2ChainProvider::new(boot.clone(), oracle.clone());
        let beacon = OracleBlobProvider::new(oracle.clone());

        ////////////////////////////////////////////////////////////////
        //                   DERIVATION & EXECUTION                   //
        ////////////////////////////////////////////////////////////////

        println!("cycle-tracker-start: derivation-instantiation");
        let mut driver = DerivationDriver::new(
            boot.as_ref(),
            oracle.as_ref(),
            beacon,
            l1_provider,
            l2_provider.clone(),
        )
        .await
        .unwrap();
        println!("cycle-tracker-end: derivation-instantiation");

        println!("cycle-tracker-start: produce-output");
        let (number, output_root) = driver
            .produce_output(
                &boot.rollup_config,
                &l2_provider.clone(),
                &l2_provider.clone(),
                zkvm_handle_register,
            )
            .await
            .unwrap();
        println!("cycle-tracker-end: produce-output");

        ////////////////////////////////////////////////////////////////
        //                          EPILOGUE                          //
        ////////////////////////////////////////////////////////////////

        assert_eq!(number, boot.claimed_l2_block_number);
        assert_eq!(output_root, boot.claimed_l2_output_root);

        println!("Validated derivation and STF. Output Root: {}", output_root);
    });
}
