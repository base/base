//! Integration tests verifying that `verifier_l1_confs` correctly constrains the derivation
//! pipeline's view of L1.
//!
//! These tests wire together the [`L1WatcherActor`] (which updates the shared L1 head atomic)
//! and the [`ConfDepthProvider`] (which reads it to gate `block_info_by_number` calls),
//! verifying the end-to-end behaviour that was previously broken: the pipeline's chain
//! provider was not constrained by the L1 head signal, so `verifier_l1_confs` had no effect
//! on the safe head.

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_eth::{Block, Filter, Log};
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineErrorKind};
use base_consensus_genesis::RollupConfig;
use base_consensus_node::{
    DerivationClientResult, L1BlockFetcher, L1WatcherActor, L1WatcherDerivationClient, NodeActor,
};
use base_consensus_providers::{AlloyChainProviderError, ConfDepthProvider, L1HeadNumber};
use base_protocol::BlockInfo;
use futures::Stream;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock types
// ---------------------------------------------------------------------------

type BoxedBlockStream = Pin<Box<dyn Stream<Item = BlockInfo> + Unpin + Send>>;

/// A minimal [`L1BlockFetcher`] that returns configurable blocks.
struct MockL1Fetcher {
    blocks: std::collections::HashMap<u64, BlockInfo>,
}

impl MockL1Fetcher {
    fn with_blocks(blocks: impl IntoIterator<Item = BlockInfo>) -> Self {
        Self { blocks: blocks.into_iter().map(|b| (b.number, b)).collect() }
    }
}

#[async_trait]
impl L1BlockFetcher for MockL1Fetcher {
    type Error = String;

    async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
        Ok(vec![])
    }

    async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error> {
        match id {
            BlockId::Number(BlockNumberOrTag::Number(number)) => {
                if self.blocks.contains_key(&number) {
                    Ok(Some(Block::default()))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }
}

/// Records derivation messages sent by the L1 watcher.
#[derive(Debug, Clone, Default)]
struct RecordingDerivationClient {
    heads: Arc<Mutex<Vec<BlockInfo>>>,
    finalized: Arc<Mutex<Vec<BlockInfo>>>,
}

#[async_trait]
impl L1WatcherDerivationClient for RecordingDerivationClient {
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
        self.finalized.lock().unwrap().push(block);
        Ok(())
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        self.heads.lock().unwrap().push(block);
        Ok(())
    }
}

fn block_at(number: u64) -> BlockInfo {
    BlockInfo {
        hash: B256::from([number as u8; 32]),
        number,
        parent_hash: B256::ZERO,
        timestamp: number * 12,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// End-to-end test: the L1 watcher updates the shared atomic, and the
/// `ConfDepthProvider` reads it to block the pipeline from advancing past
/// `l1_head - conf_depth`.
///
/// This test reproduces the exact scenario that was broken before the fix:
/// the user sets `verifier_l1_confs=4`, the L1 head is at block 100, and
/// the pipeline should only be able to read L1 blocks up to 96.
#[tokio::test]
async fn l1_watcher_and_conf_depth_provider_end_to_end() {
    let conf_depth: u64 = 4;
    let l1_head_number: L1HeadNumber = Arc::new(AtomicU64::new(0));

    // 1. Create blocks for the L1 watcher to fetch (delayed blocks).
    let blocks: Vec<BlockInfo> = (90..=100).map(block_at).collect();
    let fetcher = MockL1Fetcher::with_blocks(blocks.clone());

    // 2. Create the L1 watcher actor.
    let derivation_client = RecordingDerivationClient::default();
    let (l1_head_tx, _l1_head_rx) = watch::channel(None);
    let cancel = CancellationToken::new();

    // Stream the head at block 100.
    let head_stream: BoxedBlockStream = Box::pin(futures::stream::iter(vec![block_at(100)]));
    let finalized_stream: BoxedBlockStream = Box::pin(futures::stream::pending());

    let actor = L1WatcherActor::new(
        Arc::new(RollupConfig::default()),
        fetcher,
        l1_head_tx,
        derivation_client.clone(),
        None,
        cancel,
        head_stream,
        finalized_stream,
        conf_depth,
        Arc::clone(&l1_head_number),
    );

    // Run the actor to completion (stream ends).
    let _ = actor.start(()).await;

    // 3. Verify: the shared atomic holds the real L1 head (100).
    assert_eq!(l1_head_number.load(Ordering::Relaxed), 100);

    // 4. Now simulate what the derivation pipeline does: try to fetch L1
    //    blocks via the ConfDepthProvider. Blocks beyond head - conf_depth
    //    should be gated.

    // Use a dummy inner provider — the conf depth check happens BEFORE the
    // inner call, so it won't be reached for gated blocks.
    let dummy_inner = base_consensus_providers::AlloyChainProvider::new(
        alloy_provider::RootProvider::new_http("http://localhost:1".parse().unwrap()),
        1,
    );
    let mut provider = ConfDepthProvider::new(dummy_inner, Arc::clone(&l1_head_number), conf_depth);

    // Block 97: 97 + 4 = 101 > 100 → GATED (BlockNotFound, maps to Temporary).
    let err = provider.block_info_by_number(97).await.unwrap_err();
    assert!(
        matches!(err, AlloyChainProviderError::BlockNotFound(_)),
        "block 97 should be gated (97 + 4 > 100)"
    );
    // Verify it maps to a Temporary pipeline error (pipeline yields, doesn't reset).
    let pipeline_err: PipelineErrorKind = err.into();
    assert!(
        matches!(pipeline_err, PipelineErrorKind::Temporary(_)),
        "gated BlockNotFound must map to Temporary, not Reset"
    );

    // Block 96: 96 + 4 = 100 ≤ 100 → ALLOWED (passes to inner, which fails with
    // a transport error since there's no real RPC — but NOT a BlockNotFound).
    let err = provider.block_info_by_number(96).await.unwrap_err();
    assert!(
        !matches!(err, AlloyChainProviderError::BlockNotFound(_)),
        "block 96 should NOT be gated (96 + 4 <= 100)"
    );
}

/// Verify that with `verifier_l1_confs = 0`, no blocks are gated.
#[tokio::test]
async fn zero_conf_depth_does_not_gate_any_blocks() {
    let l1_head_number: L1HeadNumber = Arc::new(AtomicU64::new(100));

    let dummy_inner = base_consensus_providers::AlloyChainProvider::new(
        alloy_provider::RootProvider::new_http("http://localhost:1".parse().unwrap()),
        1,
    );
    let mut provider = ConfDepthProvider::new(dummy_inner, l1_head_number, 0);

    // With conf_depth=0, even blocks at the head should not be gated.
    let err = provider.block_info_by_number(200).await.unwrap_err();
    assert!(
        !matches!(err, AlloyChainProviderError::BlockNotFound(_)),
        "zero conf depth must never gate"
    );
}

/// Verify that the L1 watcher always stores the real L1 head in the atomic,
/// even when `verifier_l1_confs` delays the derivation signal.
#[tokio::test]
async fn l1_head_atomic_holds_real_head_not_delayed() {
    let conf_depth: u64 = 10;
    let l1_head_number: L1HeadNumber = Arc::new(AtomicU64::new(0));

    // The fetcher needs to return blocks for delayed lookups.
    let blocks: Vec<BlockInfo> = (0..=50).map(block_at).collect();
    let fetcher = MockL1Fetcher::with_blocks(blocks);

    let derivation_client = RecordingDerivationClient::default();
    let (l1_head_tx, _) = watch::channel(None);
    let cancel = CancellationToken::new();

    // Stream multiple heads.
    let head_stream: BoxedBlockStream =
        Box::pin(futures::stream::iter(vec![block_at(20), block_at(30), block_at(50)]));
    let finalized_stream: BoxedBlockStream = Box::pin(futures::stream::pending());

    let actor = L1WatcherActor::new(
        Arc::new(RollupConfig::default()),
        fetcher,
        l1_head_tx,
        derivation_client.clone(),
        None,
        cancel,
        head_stream,
        finalized_stream,
        conf_depth,
        Arc::clone(&l1_head_number),
    );

    let _ = actor.start(()).await;

    // The atomic should hold the REAL last head (50), not a delayed value (40).
    assert_eq!(l1_head_number.load(Ordering::Relaxed), 50);

    // Meanwhile, derivation should have received delayed heads.
    let heads = derivation_client.heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 3, "all three heads should have been forwarded to derivation");
    // Each head is fetched as Block::default() which maps to block number 0.
    // The important thing is that derivation received delayed blocks, not the real heads.
}
