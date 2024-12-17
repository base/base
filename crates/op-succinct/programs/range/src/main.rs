//! A program to verify a Optimism L2 block STF in the zkVM.
//!
//! This binary contains the client program for executing the Optimism rollup state transition
//! across a range of blocks, which can be used to generate an on chain validity proof. Depending on
//! the compilation pipeline, it will compile to be run either in native mode or in zkVM mode. In
//! native mode, the data for verifying the batch validity is fetched from RPC, while in zkVM mode,
//! the data is supplied by the host binary to the verifiable program.

#![cfg_attr(target_os = "zkvm", no_main)]

extern crate alloc;

use alloc::sync::Arc;

use alloy_consensus::{BlockBody, Sealable};
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use cfg_if::cfg_if;
use core::fmt::Debug;
use kona_derive::{
    errors::{PipelineError, PipelineErrorKind},
    prelude::{Pipeline, SignalReceiver},
    types::Signal,
};
use kona_driver::{
    DriverError, DriverPipeline, DriverResult, Executor, ExecutorConstructor, PipelineCursor,
    TipCursor,
};
use kona_preimage::CommsClient;
use kona_proof::{
    executor::KonaExecutorConstructor,
    l1::{OracleBlobProvider, OracleL1ChainProvider},
    BootInfo, FlushableCache,
};
use op_alloy_consensus::{OpBlock, OpTxEnvelope, OpTxType};
use op_alloy_genesis::RollupConfig;
use op_alloy_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::OpAttributesWithParent;
use op_succinct_client_utils::{
    l2_chain_provider::{new_pipeline_cursor, MultiblockOracleL2ChainProvider},
    pipeline::MultiblockOraclePipeline,
    precompiles::zkvm_handle_register,
};
use tracing::{error, info, warn};

cfg_if! {
    if #[cfg(target_os = "zkvm")] {
        sp1_zkvm::entrypoint!(main);

        use op_succinct_client_utils::{
            BootInfoWithBytesConfig, boot::BootInfoStruct,
            InMemoryOracle
        };
        use alloc::vec::Vec;
        use serde_json;
    } else {
        use kona_proof::CachingOracle;
        use op_succinct_client_utils::pipes::{ORACLE_READER, HINT_WRITER};
    }
}

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

                println!("cycle-tracker-report-start: oracle-verify");
                oracle.verify().expect("key value verification failed");
                println!("cycle-tracker-report-end: oracle-verify");
            }
            // If we are compiling for online mode, create a caching oracle that speaks to the
            // fetcher via hints, and gather boot info from this oracle.
            else {
                let oracle = Arc::new(CachingOracle::new(1024, ORACLE_READER, HINT_WRITER));
                let boot = Arc::new(BootInfo::load(oracle.as_ref()).await.unwrap());
            }
        }

        let l1_provider = OracleL1ChainProvider::new(boot.clone(), oracle.clone());
        let mut l2_provider = MultiblockOracleL2ChainProvider::new(boot.clone(), oracle.clone());
        let beacon = OracleBlobProvider::new(oracle.clone());

        // If the genesis block is claimed, we can exit early.
        // The agreed upon prestate is consented to by all parties, and there is no state
        // transition, so the claim is valid if the claimed output root matches the agreed
        // upon output root.
        if boot.claimed_l2_block_number == 0 {
            warn!("Genesis block claimed. Exiting early.");
            assert_eq!(boot.agreed_l2_output_root, boot.claimed_l2_output_root);
        }

        ////////////////////////////////////////////////////////////////
        //                   DERIVATION & EXECUTION                   //
        ////////////////////////////////////////////////////////////////

        let mut cursor = match new_pipeline_cursor(
            oracle.clone(),
            &boot,
            &mut l1_provider.clone(),
            &mut l2_provider.clone(),
        )
        .await
        {
            Ok(cursor) => cursor,
            Err(_) => {
                error!(target: "client", "Failed to find sync start");
                panic!("Failed to find sync start");
            }
        };

        let cfg = Arc::new(boot.rollup_config.clone());
        let mut pipeline = MultiblockOraclePipeline::new(
            cfg.clone(),
            cursor.clone(),
            oracle.clone(),
            beacon,
            l1_provider.clone(),
            l2_provider.clone(),
        );
        let executor = KonaExecutorConstructor::new(
            &cfg,
            l2_provider.clone(),
            l2_provider.clone(),
            zkvm_handle_register,
        );

        // Run the derivation pipeline until we are able to produce the output root of the claimed
        // L2 block.
        let target = boot.claimed_l2_block_number;

        let (number, output_root) = advance_to_target(
            &mut pipeline,
            &executor,
            &mut cursor,
            &mut l2_provider,
            &boot,
            target,
            &cfg,
        )
        .await
        .expect("Failed to advance to target L2 block");

        ////////////////////////////////////////////////////////////////
        //                          EPILOGUE                          //
        ////////////////////////////////////////////////////////////////

        if output_root != boot.claimed_l2_output_root {
            error!(
                target: "client",
                "Failed to validate L2 block #{number} with output root {output_root}",
                number = number,
                output_root = output_root
            );
            panic!("Failed to validate L2 block #{number} with output root {output_root}");
        }

        info!(
            target: "client",
            "Successfully validated L2 block #{number} with output root {output_root}",
            number = number,
            output_root = output_root
        );

        // Manually forget large objects to avoid allocator overhead
        std::mem::forget(pipeline);
        std::mem::forget(executor);
        std::mem::forget(l2_provider);
        std::mem::forget(l1_provider);
        std::mem::forget(oracle);
        std::mem::forget(cfg);
        std::mem::forget(cursor);
        std::mem::forget(boot);
    });
}

// Sourced from kona/crates/driver/src/core.rs with modifications to use the L2 provider's caching system.
// After each block execution, we update the L2 provider's caches (header_by_number, block_by_number,
// system_config_by_number, l2_block_info_by_number) with the new block data. This ensures subsequent
// lookups for this block number can be served directly from cache rather than requiring oracle queries.
pub async fn advance_to_target<E, EC, DP, P, O>(
    pipeline: &mut DP,
    executor: &EC,
    cursor: &mut PipelineCursor,
    l2_provider: &mut MultiblockOracleL2ChainProvider<O>,
    boot: &BootInfo,
    mut target: u64,
    cfg: &RollupConfig,
) -> DriverResult<(u64, B256), E::Error>
where
    E: Executor + Send + Sync + Debug,
    EC: ExecutorConstructor<E> + Send + Sync + Debug,
    DP: DriverPipeline<P> + Send + Sync + Debug,
    P: Pipeline + SignalReceiver + Send + Sync + Debug,
    O: CommsClient + FlushableCache + FlushableCache + Send + Sync + Debug,
{
    loop {
        info!(target: "client", "Deriving L2 block #{} with current safe head #{}", target, cursor.l2_safe_head().block_info.number);
        // Check if we have reached the target block number.
        if cursor.l2_safe_head().block_info.number >= target {
            info!(target: "client", "Derivation complete, reached L2 safe head.");
            return Ok((
                cursor.l2_safe_head().block_info.number,
                *cursor.l2_safe_head_output_root(),
            ));
        }

        println!("cycle-tracker-report-start: payload-derivation");
        let OpAttributesWithParent { mut attributes, .. } = match pipeline
            .produce_payload(*cursor.l2_safe_head())
            .await
        {
            Ok(attrs) => attrs,
            Err(PipelineErrorKind::Critical(PipelineError::EndOfSource)) => {
                warn!(target: "client", "Exhausted data source; Halting derivation and using current safe head.");

                // Adjust the target block number to the current safe head, as no more blocks
                // can be produced.
                target = cursor.l2_safe_head().block_info.number;
                continue;
            }
            Err(e) => {
                error!(target: "client", "Failed to produce payload: {:?}", e);
                return Err(DriverError::Pipeline(e));
            }
        };
        println!("cycle-tracker-report-end: payload-derivation");

        println!("cycle-tracker-report-start: block-execution");
        let mut block_executor = executor.new_executor(cursor.l2_safe_head_header().clone());

        println!("cycle-tracker-report-start: block-execution");
        let res = block_executor.execute_payload(attributes.clone());
        println!("cycle-tracker-report-end: block-execution");
        let header = match res {
            Ok(header) => header,
            Err(e) => {
                error!(target: "client", "Failed to execute L2 block: {}", e);

                if cfg.is_holocene_active(attributes.payload_attributes.timestamp) {
                    println!("cycle-tracker-report-start: block-execution");
                    // Retry with a deposit-only block.
                    warn!(target: "client", "Flushing current channel and retrying deposit only block");

                    // Flush the current batch and channel - if a block was replaced with a
                    // deposit-only block due to execution failure, the
                    // batch and channel it is contained in is forwards
                    // invalidated.
                    pipeline.signal(Signal::FlushChannel).await?;

                    // Strip out all transactions that are not deposits.
                    attributes.transactions = attributes.transactions.map(|txs| {
                        txs.into_iter()
                            .filter(|tx| (!tx.is_empty() && tx[0] == OpTxType::Deposit as u8))
                            .collect::<Vec<_>>()
                    });

                    // Retry the execution.
                    block_executor = executor.new_executor(cursor.l2_safe_head_header().clone());
                    let res = block_executor.execute_payload(attributes.clone());
                    println!("cycle-tracker-report-end: block-execution");
                    match res {
                        Ok(header) => header,
                        Err(e) => {
                            error!(
                                target: "client",
                                "Critical - Failed to execute deposit-only block: {e}",
                            );
                            return Err(DriverError::Executor(e));
                        }
                    }
                } else {
                    // Pre-Holocene, discard the block if execution fails.
                    continue;
                }
            }
        };

        // Construct the block.
        let block = OpBlock {
            header: header.clone(),
            body: BlockBody {
                transactions: attributes
                    .transactions
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tx| OpTxEnvelope::decode(&mut tx.as_ref()).map_err(DriverError::Rlp))
                    .collect::<DriverResult<Vec<OpTxEnvelope>, _>>()?,
                ommers: Vec::new(),
                withdrawals: None,
            },
        };

        // Add the block which has been successfully executed to the L2 provider's cache.
        let _ = l2_provider
            .update_cache(header, &block, &boot.rollup_config)
            .unwrap();

        // Get the pipeline origin and update the cursor.
        let origin = pipeline
            .origin()
            .ok_or(PipelineError::MissingOrigin.crit())?;
        let l2_info =
            L2BlockInfo::from_block_and_genesis(&block, &pipeline.rollup_config().genesis)?;
        println!("cycle-tracker-report-start: output-root");
        let tip_cursor = TipCursor::new(
            l2_info,
            header.clone().seal_slow(),
            block_executor
                .compute_output_root()
                .map_err(DriverError::Executor)?,
        );
        cursor.advance(origin, tip_cursor);
        println!("cycle-tracker-report-end: output-root");
    }
}
