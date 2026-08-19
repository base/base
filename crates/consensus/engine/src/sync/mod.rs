//! Sync start algorithm for the Base rollup node.

use std::time::Instant;

use alloy_eips::BlockNumberOrTag;
use base_common_genesis::RollupConfig;
use base_protocol::L2BlockInfo;

mod forkchoice;
pub use forkchoice::L2ForkchoiceState;

mod error;
pub use error::SyncStartError;
use tracing::info;

mod checkpoint;
pub use checkpoint::{
    ForkchoiceCheckpointError, ForkchoiceCheckpointLabel, ForkchoiceCheckpointReader,
    NoopForkchoiceCheckpointReader,
};

use crate::{EngineClient, Metrics};

/// Maximum supported L1 reorg depth, expressed as sequencing windows to match op-node.
const MAX_REORG_SEQ_WINDOWS: u64 = 5;

/// Searches for the latest [`L2ForkchoiceState`] that we can use to start the sync process with.
///
///   - The *unsafe L2 block*: This is the highest L2 block whose L1 origin is a *plausible*
///     extension of the canonical L1 chain (as known to the rollup node).
///   - The *safe L2 block*: This is the highest L2 block whose epoch's sequencing window is
///     complete within the canonical L1 chain (as known to the rollup node).
///   - The *finalized L2 block*: This is the L2 block which is known to be fully derived from
///     finalized L1 block data.
///
/// Plausible: meaning that the blockhash of the L2 block's L1 origin
/// (as reported in the L1 Attributes deposit within the L2 block) is not canonical at another
/// height in the L1 chain, and the same holds for all its ancestors.
pub async fn find_starting_forkchoice<EngineClient_: EngineClient>(
    cfg: &RollupConfig,
    engine_client: &EngineClient_,
) -> Result<L2ForkchoiceState, SyncStartError> {
    find_starting_forkchoice_with_checkpoint_reader(
        cfg,
        engine_client,
        &NoopForkchoiceCheckpointReader,
    )
    .await
}

/// Like [`find_starting_forkchoice`], but consults `checkpoint_reader` when reth-labeled blocks
/// cannot be hydrated because their bodies have been pruned (see [`ForkchoiceCheckpointReader`]).
pub async fn find_starting_forkchoice_with_checkpoint_reader<
    EngineClient_: EngineClient,
    CheckpointReader: ForkchoiceCheckpointReader + ?Sized,
>(
    cfg: &RollupConfig,
    engine_client: &EngineClient_,
    checkpoint_reader: &CheckpointReader,
) -> Result<L2ForkchoiceState, SyncStartError> {
    let mut current_fc =
        L2ForkchoiceState::current_with_checkpoint_reader(cfg, engine_client, checkpoint_reader)
            .await?;
    info!(
        target: "sync_start",
        unsafe = %current_fc.un_safe.block_info.number,
        safe = %current_fc.safe.block_info.number,
        finalized = %current_fc.finalized.block_info.number,
        "Loaded current L2 EL forkchoice state"
    );

    // Search for the highest `unsafe` block, relative to the initial `unsafe` block's L1 origin.
    // Finalized is the lower correctness boundary. The L1-origin depth guard matches op-node's
    // operational bound without imposing an arbitrary L2-block limit inside a sequencing epoch.
    let previous_unsafe_origin = current_fc.un_safe.l1_origin.number;
    let max_reorg_depth = cfg.seq_window_size.saturating_mul(MAX_REORG_SEQ_WINDOWS);
    let unsafe_walk_started = Instant::now();
    let mut unsafe_walked_blocks = 0_u64;
    let mut visible_l1_head_number = None;
    let unsafe_walk_result = async {
        loop {
            if current_fc.un_safe.block_info.number <= current_fc.finalized.block_info.number
                && current_fc.un_safe.block_info.hash != current_fc.finalized.block_info.hash
            {
                break Err(SyncStartError::MismatchedFinalizedBlock(
                    current_fc.finalized.block_info.hash,
                    current_fc.un_safe.block_info.hash,
                ));
            }

            let origin = current_fc.un_safe.l1_origin;
            if origin.number.saturating_add(max_reorg_depth) < previous_unsafe_origin {
                break Err(SyncStartError::TooDeepReorg {
                    previous_unsafe_origin,
                    walked_origin: origin.number,
                });
            }
            let canonical_l1 =
                engine_client.get_l1_block(BlockNumberOrTag::Number(origin.number).into()).await?;
            info!(
                target: "sync_start",
                l1_origin = origin.number,
                l2_unsafe = %current_fc.un_safe.block_info.number,
                "Searching for L2 unsafe block with canonical L1 origin"
            );

            let origin_is_plausible = match canonical_l1 {
                Some(block) => block.header.hash == origin.hash,
                None => {
                    // A missing block by number is only plausible when the L2 origin is ahead of
                    // the L1 view. A missing block at or below the visible L1 head is
                    // noncanonical. Keep one head snapshot for a coherent reset walk and to avoid
                    // repeating the same latest-head RPC for each missing origin.
                    let l1_head_number = match visible_l1_head_number {
                        Some(number) => number,
                        None => {
                            let number = engine_client
                                .get_l1_block(BlockNumberOrTag::Latest.into())
                                .await?
                                .ok_or(SyncStartError::BlockNotFound(
                                    BlockNumberOrTag::Latest.into(),
                                ))?
                                .header
                                .number;
                            visible_l1_head_number = Some(number);
                            number
                        }
                    };
                    origin.number > l1_head_number
                }
            };

            if origin_is_plausible {
                info!(
                    target: "sync_start",
                    l2_unsafe = %current_fc.un_safe.block_info.number,
                    "Found L2 unsafe block with canonical L1 origin"
                );
                break Ok::<(), SyncStartError>(());
            }

            if current_fc.un_safe.block_info.number <= current_fc.finalized.block_info.number {
                break Err(SyncStartError::FinalizedL1OriginNotCanonical(
                    current_fc.finalized.block_info.hash,
                ));
            }

            let l2_parent_hash = current_fc.un_safe.block_info.parent_hash.into();
            let l2_parent = engine_client
                .get_l2_block(l2_parent_hash)
                .full()
                .await?
                .ok_or(SyncStartError::BlockNotFound(l2_parent_hash))?;

            current_fc.un_safe = L2BlockInfo::from_block_and_genesis(
                &l2_parent
                    .map_header(|header| header.into_inner())
                    .into_consensus()
                    .map_transactions(|tx| tx.inner.inner.into_inner()),
                &cfg.genesis,
            )?;
            unsafe_walked_blocks = unsafe_walked_blocks.saturating_add(1);
        }
    }
    .await;
    Metrics::engine_reset_forkchoice_walked_blocks("unsafe").record(unsafe_walked_blocks as f64);
    Metrics::engine_reset_forkchoice_walk_duration_seconds("unsafe")
        .record(unsafe_walk_started.elapsed());
    unsafe_walk_result?;

    // Search for the highest `safe` block that's L1 origin is at least older than the sequencing
    // window, relative to the L1 origin of the `unsafe` block.
    let mut safe_cursor = current_fc.un_safe;
    let safe_walk_started = Instant::now();
    let mut safe_walked_blocks = 0_u64;
    let safe_walk_result = async {
        loop {
            info!(
                target: "sync_start",
                l1_origin = %safe_cursor.l1_origin.number,
                l2_safe = %safe_cursor.block_info.number,
                "Searching for L2 safe block beyond sequencing window"
            );

            let is_behind_sequence_window =
                current_fc.un_safe.l1_origin.number.saturating_sub(cfg.seq_window_size)
                    > safe_cursor.l1_origin.number;
            let is_labeled_safe = safe_cursor.block_info.hash == current_fc.safe.block_info.hash;
            let is_finalized = safe_cursor.block_info.hash == current_fc.finalized.block_info.hash;
            let is_genesis = safe_cursor.block_info.hash == cfg.genesis.l2.hash;
            if is_behind_sequence_window || is_labeled_safe || is_finalized || is_genesis {
                info!(
                    target: "sync_start",
                    l2_safe = %safe_cursor.block_info.number,
                    is_behind_sequence_window,
                    is_labeled_safe,
                    is_finalized,
                    is_genesis,
                    "Found suitable L2 safe block"
                );
                current_fc.safe = safe_cursor;
                break Ok::<(), SyncStartError>(());
            }
            if safe_cursor.block_info.parent_hash == current_fc.safe.block_info.hash {
                safe_cursor = current_fc.safe;
                safe_walked_blocks = safe_walked_blocks.saturating_add(1);
                continue;
            }
            if safe_cursor.block_info.parent_hash == current_fc.finalized.block_info.hash {
                safe_cursor = current_fc.finalized;
                safe_walked_blocks = safe_walked_blocks.saturating_add(1);
                continue;
            }
            let block = engine_client
                .get_l2_block(safe_cursor.block_info.parent_hash.into())
                .full()
                .await?
                .ok_or(SyncStartError::BlockNotFound(safe_cursor.block_info.parent_hash.into()))?;
            safe_cursor = L2BlockInfo::from_block_and_genesis(
                &block
                    .map_header(|header| header.into_inner())
                    .into_consensus()
                    .map_transactions(|tx| tx.inner.inner.into_inner()),
                &cfg.genesis,
            )?;
            safe_walked_blocks = safe_walked_blocks.saturating_add(1);
        }
    }
    .await;
    Metrics::engine_reset_forkchoice_walked_blocks("safe").record(safe_walked_blocks as f64);
    Metrics::engine_reset_forkchoice_walk_duration_seconds("safe")
        .record(safe_walk_started.elapsed());
    safe_walk_result?;

    // Leave the finalized block as-is, and return the current forkchoice.
    Ok(current_fc)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
    use alloy_primitives::{Address, B256, Sealed, b256};
    use alloy_provider::Network;
    use alloy_rpc_types_eth::{
        Block as RpcBlock, BlockTransactions, Transaction as EthTransaction,
    };
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::ChainGenesis;
    use base_common_network::Base;
    use base_common_rpc_types::Transaction as BaseTransaction;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;

    use crate::{EngineClient, test_utils::test_engine_client_builder};

    const BASE_SEPOLIA_GENESIS_HASH: B256 =
        b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4");
    const BASE_SEPOLIA_GENESIS_RPC_RESPONSE: &str = "{\"hash\":\"0x0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4\",\"parentHash\":\"0x0000000000000000000000000000000000000000000000000000000000000000\",\"sha3Uncles\":\"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347\",\"miner\":\"0x4200000000000000000000000000000000000011\",\"stateRoot\":\"0x907f339ca16b3e45a89a7f4cc29d4430c8d4178d73b370ec9180e04a0dd7fcf3\",\"transactionsRoot\":\"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421\",\"receiptsRoot\":\"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421\",\"logsBloom\":\"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\",\"difficulty\":\"0x0\",\"number\":\"0x0\",\"gasLimit\":\"0x17d7840\",\"gasUsed\":\"0x0\",\"timestamp\":\"0x65135ee0\",\"extraData\":\"0x424544524f434b\",\"mixHash\":\"0x0000000000000000000000000000000000000000000000000000000000000000\",\"nonce\":\"0x0000000000000000\",\"baseFeePerGas\":\"0x3b9aca00\",\"size\":\"0x209\",\"uncles\":[],\"transactions\":[]}";

    /// Sanity regression test - `alloy_rpc_types`' `Block::into_consensus` failed to saturate the
    /// header of the `alloy_consensus::Header` type on an old version. This test covers the
    /// conversion to ensure a Base Sepolia genesis block's conversion to the consensus type works for
    /// the sake of `L2BlockInfo::from_block_and_genesis`.
    #[tokio::test]
    async fn test_genesis_block_hash() {
        let genesis = ChainGenesis {
            l2: BlockNumHash { number: 0, hash: BASE_SEPOLIA_GENESIS_HASH },
            ..Default::default()
        };
        let genesis_block: RpcBlock<<Base as Network>::TransactionResponse> =
            serde_json::from_str(BASE_SEPOLIA_GENESIS_RPC_RESPONSE).unwrap();

        let rpc_reported_hash = genesis_block.header.hash;
        let consensus_block = genesis_block.into_consensus();

        // Check that the genesis block's RPC-reported hash is equal to the manually computed hash.
        assert_eq!(rpc_reported_hash, consensus_block.hash_slow());

        // Convert to `L2BlockInfo` and check the same.
        let l2_block_info =
            L2BlockInfo::from_block_and_genesis(&consensus_block, &genesis).unwrap();
        assert_eq!(rpc_reported_hash, l2_block_info.block_info.hash);
    }

    fn l1_info_rpc_transaction(origin: BlockNumHash, block_number: u64) -> BaseTransaction {
        let envelope = BaseTxEnvelope::Deposit(Sealed::new_unchecked(
            TxDeposit {
                input: L1BlockInfoBedrock::new_from_number_and_block_hash(
                    origin.number,
                    origin.hash,
                )
                .encode_calldata(),
                ..Default::default()
            },
            B256::ZERO,
        ));
        BaseTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, Address::ZERO),
                block_hash: None,
                block_number: Some(block_number),
                block_timestamp: None,
                effective_gas_price: Some(0),
                transaction_index: Some(0),
            },
            block_timestamp_ms: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn l2_block_with_l1_info(
        number: u64,
        parent_hash: B256,
        origin: BlockNumHash,
    ) -> RpcBlock<BaseTransaction> {
        let mut block = RpcBlock::<BaseTransaction>::default();
        block.header.inner.number = number;
        block.header.inner.parent_hash = parent_hash;
        block.header.inner.timestamp = number;
        block.transactions = BlockTransactions::Full(vec![l1_info_rpc_transaction(origin, number)]);
        block
    }

    #[tokio::test]
    async fn current_uses_config_genesis_when_finalized_label_is_unavailable() {
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: BlockNumHash {
                    number: 10,
                    hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
                },
                l2: BlockNumHash {
                    number: 0,
                    hash: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
                },
                l2_time: 20,
                system_config: None,
            },
            ..Default::default()
        };
        let latest =
            l2_block_with_l1_info(1, rollup_config.genesis.l2.hash, BlockNumHash::default());
        let latest_hash = latest.clone().into_consensus().hash_slow();
        let client = test_engine_client_builder()
            .with_l2_block(BlockId::Number(BlockNumberOrTag::Latest), latest)
            .with_l1_block(
                BlockId::Number(BlockNumberOrTag::Number(0)),
                RpcBlock::<EthTransaction>::default(),
            )
            .build();

        let forkchoice = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect("forkchoice should not require fetching the pruned L2 genesis block");
        let expected_genesis = L2BlockInfo {
            block_info: BlockInfo {
                hash: rollup_config.genesis.l2.hash,
                number: rollup_config.genesis.l2.number,
                parent_hash: B256::ZERO,
                timestamp: rollup_config.genesis.l2_time,
            },
            l1_origin: rollup_config.genesis.l1,
            seq_num: 0,
        };

        assert_eq!(forkchoice.un_safe.block_info.number, 1);
        assert_eq!(forkchoice.un_safe.block_info.hash, latest_hash);
        assert_eq!(forkchoice.safe, expected_genesis);
        assert_eq!(forkchoice.finalized, expected_genesis);
    }

    #[tokio::test]
    async fn reset_rewinds_orphaned_l1_origin_and_preserves_finalized() {
        #[cfg(feature = "metrics")]
        let recorder = PrometheusBuilder::new().build_recorder();
        #[cfg(feature = "metrics")]
        let metrics_handle = recorder.handle();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let canonical_parent_origin = BlockNumHash {
            number: 10,
            hash: b256!("1010101010101010101010101010101010101010101010101010101010101010"),
        };
        let orphan_origin = BlockNumHash {
            number: 11,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        };
        let canonical_origin_hash =
            b256!("1212121212121212121212121212121212121212121212121212121212121212");
        let genesis_hash =
            b256!("2020202020202020202020202020202020202020202020202020202020202020");
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: canonical_parent_origin,
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                ..Default::default()
            },
            seq_window_size: 1,
            ..Default::default()
        };

        let parent = l2_block_with_l1_info(1, genesis_hash, canonical_parent_origin);
        let parent_hash = parent.clone().into_consensus().hash_slow();
        let unsafe_head = l2_block_with_l1_info(2, parent_hash, orphan_origin);

        let mut canonical_parent_l1 = RpcBlock::<EthTransaction>::default();
        canonical_parent_l1.header.hash = canonical_parent_origin.hash;
        canonical_parent_l1.header.inner.number = canonical_parent_origin.number;
        let mut canonical_l1 = RpcBlock::<EthTransaction>::default();
        canonical_l1.header.hash = canonical_origin_hash;
        canonical_l1.header.inner.number = orphan_origin.number;
        let mut orphan_l1 = RpcBlock::<EthTransaction>::default();
        orphan_l1.header.hash = orphan_origin.hash;
        orphan_l1.header.inner.number = orphan_origin.number;

        let client = test_engine_client_builder()
            .with_l2_block(BlockNumberOrTag::Latest.into(), unsafe_head)
            .with_l2_block(BlockNumberOrTag::Safe.into(), parent.clone())
            .with_l2_block(BlockNumberOrTag::Finalized.into(), parent.clone())
            .with_l2_block(parent_hash.into(), parent)
            .with_l1_block(BlockNumberOrTag::Number(10).into(), canonical_parent_l1)
            .with_l1_block(BlockNumberOrTag::Number(11).into(), canonical_l1)
            // The orphan remains available by hash, as is common after an L1 reorg.
            .with_l1_block(orphan_origin.hash.into(), orphan_l1)
            .build();

        assert!(
            client
                .get_l1_block(orphan_origin.hash.into())
                .await
                .expect("orphan lookup should succeed")
                .is_some(),
            "orphan must remain retrievable by hash"
        );

        let forkchoice = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect("reset should find the canonical parent");

        assert_eq!(forkchoice.un_safe.block_info.hash, parent_hash);
        assert_eq!(forkchoice.un_safe.l1_origin, canonical_parent_origin);
        assert_eq!(forkchoice.safe.block_info.hash, parent_hash);
        assert_eq!(forkchoice.finalized.block_info.hash, parent_hash);
        assert_eq!(forkchoice.safe, forkchoice.finalized);

        #[cfg(feature = "metrics")]
        {
            let rendered = metrics_handle.render();
            assert!(rendered.contains(
                "base_node_engine_reset_forkchoice_walked_blocks_sum{phase=\"unsafe\"} 1"
            ));
            assert!(
                rendered.contains(
                    "base_node_engine_reset_forkchoice_walked_blocks_sum{phase=\"safe\"} 0"
                )
            );
            assert!(rendered.contains(
                "base_node_engine_reset_forkchoice_walk_duration_seconds_count{phase=\"unsafe\"} 1"
            ));
            assert!(rendered.contains(
                "base_node_engine_reset_forkchoice_walk_duration_seconds_count{phase=\"safe\"} 1"
            ));
        }
    }

    #[tokio::test]
    async fn reset_does_not_rewind_below_finalized_with_orphaned_l1_origin() {
        let genesis_hash =
            b256!("2020202020202020202020202020202020202020202020202020202020202020");
        let genesis_origin = BlockNumHash {
            number: 10,
            hash: b256!("1010101010101010101010101010101010101010101010101010101010101010"),
        };
        let orphan_origin = BlockNumHash {
            number: 11,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        };
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: genesis_origin,
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                ..Default::default()
            },
            ..Default::default()
        };

        let finalized = l2_block_with_l1_info(1, genesis_hash, orphan_origin);
        let finalized_hash = finalized.clone().into_consensus().hash_slow();
        let unsafe_head = l2_block_with_l1_info(2, finalized_hash, orphan_origin);
        let mut canonical_l1 = RpcBlock::<EthTransaction>::default();
        canonical_l1.header.hash =
            b256!("1212121212121212121212121212121212121212121212121212121212121212");
        canonical_l1.header.inner.number = orphan_origin.number;

        let client = test_engine_client_builder()
            .with_l2_block(BlockNumberOrTag::Latest.into(), unsafe_head)
            .with_l2_block(BlockNumberOrTag::Finalized.into(), finalized.clone())
            .with_l2_block(finalized_hash.into(), finalized)
            .with_l1_block(BlockNumberOrTag::Number(orphan_origin.number).into(), canonical_l1)
            .build();

        let error = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect_err("reset must not walk below the finalized L2 head");

        assert!(matches!(
            error,
            super::SyncStartError::FinalizedL1OriginNotCanonical(hash)
                if hash == finalized_hash
        ));
    }

    #[tokio::test]
    async fn reset_rejects_reorg_deeper_than_five_sequencing_windows() {
        let canonical_origin = BlockNumHash {
            number: 10,
            hash: b256!("1010101010101010101010101010101010101010101010101010101010101010"),
        };
        let orphan_origin = BlockNumHash {
            number: 16,
            hash: b256!("1616161616161616161616161616161616161616161616161616161616161616"),
        };
        let genesis_hash =
            b256!("2020202020202020202020202020202020202020202020202020202020202020");
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: canonical_origin,
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                ..Default::default()
            },
            seq_window_size: 1,
            ..Default::default()
        };

        let parent = l2_block_with_l1_info(1, genesis_hash, canonical_origin);
        let parent_hash = parent.clone().into_consensus().hash_slow();
        let unsafe_head = l2_block_with_l1_info(2, parent_hash, orphan_origin);
        let mut canonical_parent_l1 = RpcBlock::<EthTransaction>::default();
        canonical_parent_l1.header.hash = canonical_origin.hash;
        canonical_parent_l1.header.inner.number = canonical_origin.number;
        let mut replacement_l1 = RpcBlock::<EthTransaction>::default();
        replacement_l1.header.hash = B256::with_last_byte(17);
        replacement_l1.header.inner.number = orphan_origin.number;

        let client = test_engine_client_builder()
            .with_l2_block(BlockNumberOrTag::Latest.into(), unsafe_head)
            .with_l2_block(BlockNumberOrTag::Safe.into(), parent.clone())
            .with_l2_block(BlockNumberOrTag::Finalized.into(), parent.clone())
            .with_l2_block(parent_hash.into(), parent)
            .with_l1_block(BlockNumberOrTag::Number(10).into(), canonical_parent_l1)
            .with_l1_block(BlockNumberOrTag::Number(16).into(), replacement_l1)
            .build();

        let error = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect_err("reset must reject a reorg deeper than five sequencing windows");

        assert!(matches!(
            error,
            super::SyncStartError::TooDeepReorg { previous_unsafe_origin: 16, walked_origin: 10 }
        ));
        let storage = client.storage();
        let storage = storage.read().await;
        assert_eq!(storage.l1_block_calls_by_id.get("number:10"), None);
    }

    #[tokio::test]
    async fn reset_preserves_unsafe_origin_ahead_of_visible_l1() {
        let genesis_hash =
            b256!("2020202020202020202020202020202020202020202020202020202020202020");
        let ahead_origin = BlockNumHash {
            number: 12,
            hash: b256!("1212121212121212121212121212121212121212121212121212121212121212"),
        };
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: BlockNumHash {
                    number: 10,
                    hash: b256!("1010101010101010101010101010101010101010101010101010101010101010"),
                },
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                ..Default::default()
            },
            ..Default::default()
        };
        let unsafe_head = l2_block_with_l1_info(1, genesis_hash, ahead_origin);
        let unsafe_hash = unsafe_head.clone().into_consensus().hash_slow();
        let mut visible_l1_head = RpcBlock::<EthTransaction>::default();
        visible_l1_head.header.inner.number = 11;

        let client = test_engine_client_builder()
            .with_l2_block(BlockNumberOrTag::Latest.into(), unsafe_head)
            .with_l1_block(BlockNumberOrTag::Latest.into(), visible_l1_head)
            .build();

        let forkchoice = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect("an origin ahead of the visible L1 head should remain plausible");

        assert_eq!(forkchoice.un_safe.block_info.hash, unsafe_hash);
        assert_eq!(forkchoice.un_safe.l1_origin, ahead_origin);
    }

    #[tokio::test]
    async fn reset_fetches_visible_l1_head_once_while_walking_missing_origins() {
        let genesis_origin = BlockNumHash {
            number: 10,
            hash: b256!("1010101010101010101010101010101010101010101010101010101010101010"),
        };
        let parent_origin = BlockNumHash {
            number: 11,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        };
        let unsafe_origin = BlockNumHash {
            number: 12,
            hash: b256!("1212121212121212121212121212121212121212121212121212121212121212"),
        };

        let genesis = l2_block_with_l1_info(0, B256::ZERO, genesis_origin);
        let genesis_hash = genesis.clone().into_consensus().hash_slow();
        let parent = l2_block_with_l1_info(1, genesis_hash, parent_origin);
        let parent_hash = parent.clone().into_consensus().hash_slow();
        let unsafe_head = l2_block_with_l1_info(2, parent_hash, unsafe_origin);
        let rollup_config = base_common_genesis::RollupConfig {
            genesis: ChainGenesis {
                l1: genesis_origin,
                l2: BlockNumHash { number: 0, hash: genesis_hash },
                ..Default::default()
            },
            seq_window_size: 1,
            ..Default::default()
        };

        let mut canonical_genesis_l1 = RpcBlock::<EthTransaction>::default();
        canonical_genesis_l1.header.hash = genesis_origin.hash;
        canonical_genesis_l1.header.inner.number = genesis_origin.number;
        let mut visible_l1_head = RpcBlock::<EthTransaction>::default();
        visible_l1_head.header.inner.number = unsafe_origin.number;

        let client = test_engine_client_builder()
            .with_l2_block(BlockNumberOrTag::Latest.into(), unsafe_head)
            .with_l2_block(parent_hash.into(), parent)
            .with_l2_block(genesis_hash.into(), genesis)
            .with_l1_block(
                BlockNumberOrTag::Number(genesis_origin.number).into(),
                canonical_genesis_l1,
            )
            .with_l1_block(BlockNumberOrTag::Latest.into(), visible_l1_head)
            .build();

        let forkchoice = super::find_starting_forkchoice(&rollup_config, &client)
            .await
            .expect("reset should walk both missing origins to the canonical genesis origin");

        assert_eq!(forkchoice.un_safe.block_info.hash, genesis_hash);
        let storage = client.storage();
        let storage = storage.read().await;
        assert_eq!(storage.l1_block_calls_by_id.get("number:latest"), Some(&1));
    }
}
