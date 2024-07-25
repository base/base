//! A program to verify a Optimism L2 block STF in the zkVM.
#![cfg_attr(target_os = "zkvm", no_main)]

use client_utils::{MultiBlockDerivationDriver, MultiblockOracleL2ChainProvider};
use kona_client::{
    l1::{OracleBlobProvider, OracleL1ChainProvider},
    BootInfo,
};
use kona_executor::{NoPrecompileOverride, StatelessL2BlockExecutor};

use alloy_eips::eip2718::Decodable2718;
use kona_primitives::{L2ExecutionPayloadEnvelope, OpBlock};
use op_alloy_consensus::OpTxEnvelope;

use alloc::sync::Arc;
use alloy_consensus::Sealed;
use cfg_if::cfg_if;

extern crate alloc;

cfg_if! {
    // If the target OS is zkVM, set everything up to read input data
    // from SP1 and compile to a program that can be run in zkVM.
    if #[cfg(target_os = "zkvm")] {
        sp1_zkvm::entrypoint!(main);

        use client_utils::{
            RawBootInfo,
            InMemoryOracle
        };
        use alloc::vec::Vec;
    } else {
        use kona_client::CachingOracle;
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
                let boot = sp1_zkvm::io::read::<RawBootInfo>();
                sp1_zkvm::io::commit_slice(&boot.abi_encode());
                let boot: Arc<BootInfo> = Arc::new(boot.into());

                let kv_store_bytes: Vec<u8> = sp1_zkvm::io::read_vec();
                let oracle = Arc::new(InMemoryOracle::from_raw_bytes(kv_store_bytes));

                oracle.verify().expect("key value verification failed");

            // If we are compiling for online mode, create a caching oracle that speaks to the
            // fetcher via hints, and gather boot info from this oracle.
            } else {
                let oracle = Arc::new(CachingOracle::new(1024));
                let boot = Arc::new(BootInfo::load(oracle.as_ref()).await.unwrap());
            }
        }

        let l1_provider = OracleL1ChainProvider::new(boot.clone(), oracle.clone());
        let mut l2_provider = MultiblockOracleL2ChainProvider::new(boot.clone(), oracle.clone());
        let beacon = OracleBlobProvider::new(oracle.clone());

        ////////////////////////////////////////////////////////////////
        //                   DERIVATION & EXECUTION                   //
        ////////////////////////////////////////////////////////////////

        let mut driver = MultiBlockDerivationDriver::new(
            boot.as_ref(),
            oracle.as_ref(),
            beacon,
            l1_provider,
            l2_provider.clone(),
        )
        .await
        .unwrap();

        let mut l2_block_info = driver.l2_safe_head;
        let mut new_block_header = &driver.l2_safe_head_header.inner().clone();

        let mut executor = StatelessL2BlockExecutor::builder(&boot.rollup_config)
            .with_parent_header(driver.clone_l2_safe_head_header())
            .with_fetcher(l2_provider.clone())
            .with_hinter(l2_provider.clone())
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        'step: loop {
            let l2_attrs_with_parents = driver.produce_payloads().await.unwrap();
            if l2_attrs_with_parents.is_empty() {
                continue;
            }

            for payload in l2_attrs_with_parents {
                // Execute the payload to generate a new block header.
                println!(
                    "Executing Payload for L2 Block: {}",
                    payload.parent.block_info.number + 1
                );
                new_block_header = executor
                    .execute_payload(payload.attributes.clone())
                    .unwrap();
                let new_block_number = new_block_header.number;
                assert_eq!(new_block_number, payload.parent.block_info.number + 1);

                // Generate the Payload Envelope, which can be used to derive cached data.
                let l2_payload_envelope: L2ExecutionPayloadEnvelope = OpBlock {
                    header: new_block_header.clone(),
                    body: payload
                        .attributes
                        .transactions
                        .iter()
                        .map(|raw_tx| OpTxEnvelope::decode_2718(&mut raw_tx.as_ref()).unwrap())
                        .collect::<Vec<OpTxEnvelope>>(),
                    withdrawals: boot
                        .rollup_config
                        .is_canyon_active(new_block_header.timestamp)
                        .then(Vec::new),
                    ..Default::default()
                }
                .into();

                // Add all data from this block's execution to the cache.
                l2_block_info = l2_provider
                    .update_cache(new_block_header, l2_payload_envelope, &boot.rollup_config)
                    .unwrap();

                // Increment last_block_num and check if we have reached the claim block.
                if new_block_number == boot.l2_claim_block {
                    break 'step;
                }
            }

            // Update data for the next iteration.
            driver.update_safe_head(
                l2_block_info,
                Sealed::new_unchecked(new_block_header.clone(), new_block_header.hash_slow()),
            );
        }

        let output_root = executor.compute_output_root().unwrap();
        println!("Completed Proof. Output Root: {}", output_root);

        ////////////////////////////////////////////////////////////////
        //                          EPILOGUE                          //
        ////////////////////////////////////////////////////////////////

        // Note: We don't need the last_block_num == claim_block check, because it's the only way to exit the above loop
        assert_eq!(output_root, boot.l2_claim);
    });
}
