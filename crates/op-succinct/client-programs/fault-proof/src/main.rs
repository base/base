//! A program to verify a Optimism L2 block STF in the zkVM.
#![cfg_attr(target_os = "zkvm", no_main)]

use kona_client::{
    l1::{DerivationDriver, OracleBlobProvider, OracleL1ChainProvider},
    l2::OracleL2ChainProvider,
    BootInfo,
};

use kona_executor::{NoPrecompileOverride, StatelessL2BlockExecutor};
use kona_primitives::L2AttributesWithParent;

use alloc::sync::Arc;
use alloy_consensus::Header;
use cfg_if::cfg_if;

extern crate alloc;

cfg_if! {
    // If the target OS is zkVM, set everything up to read input data
    // from SP1 and compile to a program that can be run in zkVM.
    if #[cfg(target_os = "zkvm")] {
        sp1_zkvm::entrypoint!(main);
        use client_utils::{RawBootInfo, InMemoryOracle};
        use alloc::vec::Vec;
    } else {
        use kona_client::CachingOracle;
        use client_utils::pipes::{ORACLE_READER, HINT_WRITER};
    }
}

fn main() {
    client_utils::block_on(async move {
        ////////////////////////////////////////////////////////////////
        //                          PROLOGUE                          //
        ////////////////////////////////////////////////////////////////

        cfg_if! {
            // If we are compiling for the zkVM, read inputs from SP1 to generate boot info
            // and in memory oracle.
            if #[cfg(target_os = "zkvm")] {
                println!("cycle-tracker-start: boot-load");
                let raw_boot_info = sp1_zkvm::io::read::<RawBootInfo>();
                sp1_zkvm::io::commit_slice(&raw_boot_info.abi_encode());
                let boot: Arc<BootInfo> = Arc::new(raw_boot_info.into());
                println!("cycle-tracker-end: boot-load");

                println!("cycle-tracker-start: oracle-load");
                let kv_store_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
                let oracle = Arc::new(InMemoryOracle::from_raw_bytes(kv_store_bytes));
                println!("cycle-tracker-end: oracle-load");

                println!("cycle-tracker-start: oracle-verify");
                oracle.verify().expect("key value verification failed");
                println!("cycle-tracker-end: oracle-verify");

                let precompile_overrides = NoPrecompileOverride;

            // If we are compiling for online mode, create a caching oracle that speaks to the
            // fetcher via hints, and gather boot info from this oracle.
            } else {
                let oracle = Arc::new(CachingOracle::new(1024, ORACLE_READER, HINT_WRITER));
                let boot = Arc::new(BootInfo::load(oracle.as_ref()).await.unwrap());
                let precompile_overrides = NoPrecompileOverride;
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

        println!("cycle-tracker-start: payload-derivation");
        let L2AttributesWithParent { attributes, .. } =
            driver.produce_disputed_payload().await.unwrap();
        println!("cycle-tracker-end: payload-derivation");

        println!("cycle-tracker-start: execution-instantiation");
        let mut executor = StatelessL2BlockExecutor::builder(&boot.rollup_config)
            .with_parent_header(driver.take_l2_safe_head_header())
            .with_fetcher(l2_provider.clone())
            .with_hinter(l2_provider)
            .with_precompile_overrides(precompile_overrides)
            .build()
            .unwrap();
        println!("cycle-tracker-end: execution-instantiation");

        println!("cycle-tracker-start: execution");
        let Header { number, .. } = *executor.execute_payload(attributes).unwrap();
        println!("cycle-tracker-end: execution");

        println!("cycle-tracker-start: output-root");
        let output_root = executor.compute_output_root().unwrap();
        println!("cycle-tracker-end: output-root");

        // ////////////////////////////////////////////////////////////////
        // //                          EPILOGUE                          //
        // ////////////////////////////////////////////////////////////////

        assert_eq!(number, boot.l2_claim_block);
        assert_eq!(output_root, boot.l2_claim);
    });
}
