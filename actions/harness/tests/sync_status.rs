//! Action tests for consensus sync-status L1 reporting.

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use base_action_harness::{ActionL1BlockFetcher, ActionTestHarness, SharedL1Chain};
use base_consensus_node::{
    DerivationClientResult, L1WatcherActor, L1WatcherDerivationClient, L1WatcherQueryExecutor,
    NodeActor,
};
use base_consensus_rpc::L1WatcherQueries;
use base_protocol::BlockInfo;
use futures::Stream;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

type BoxedBlockStream = Pin<Box<dyn Stream<Item = BlockInfo> + Unpin + Send>>;

#[derive(Debug, Clone, Default)]
struct RecordingDerivationClient {
    heads: Arc<Mutex<Vec<BlockInfo>>>,
}

#[async_trait]
impl L1WatcherDerivationClient for RecordingDerivationClient {
    async fn send_finalized_l1_block(&self, _: BlockInfo) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        self.heads.lock().unwrap().push(block);
        Ok(())
    }
}

#[tokio::test]
async fn sync_status_current_l1_should_track_verifier_depth_origin_not_l1_head() {
    const L1_HEAD: u64 = 100;
    const VERIFIER_L1_CONFS: u64 = 4;

    let mut harness = ActionTestHarness::default();
    harness.mine_l1_blocks(L1_HEAD);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let live_head = harness.l1.tip_info();
    let expected_derivation_origin = harness.l1.block_info_at(L1_HEAD - VERIFIER_L1_CONFS);

    let derivation_client = RecordingDerivationClient::default();
    let l1_head_number = Arc::new(AtomicU64::new(0));
    let (l1_head_tx, l1_head_rx) = watch::channel(None);
    let head_stream: BoxedBlockStream = Box::pin(futures::stream::iter(vec![live_head]));
    let finalized_stream: BoxedBlockStream = Box::pin(futures::stream::pending());
    let actor = L1WatcherActor::new(
        Arc::new(harness.rollup_config.clone()),
        ActionL1BlockFetcher::new(l1_chain.clone()),
        l1_head_tx,
        derivation_client.clone(),
        None,
        CancellationToken::new(),
        head_stream,
        finalized_stream,
        VERIFIER_L1_CONFS,
        Arc::clone(&l1_head_number),
    );
    let _ = actor.start(()).await;

    assert_eq!(l1_head_number.load(Ordering::Relaxed), L1_HEAD);
    assert_eq!(
        derivation_client.heads.lock().unwrap().last().copied(),
        Some(expected_derivation_origin)
    );

    let executor = L1WatcherQueryExecutor::new(
        Arc::new(harness.rollup_config.clone()),
        Arc::new(ActionL1BlockFetcher::new(l1_chain)),
        l1_head_rx,
    );
    let (sender, receiver) = oneshot::channel();

    executor.execute(L1WatcherQueries::L1State(sender)).await;

    let state = receiver.await.expect("state query should return a response");
    assert_eq!(state.current_l1, Some(expected_derivation_origin));
    assert_eq!(state.head_l1, Some(live_head));
    assert_ne!(
        state.current_l1, state.head_l1,
        "verifier_l1_confs should make current_l1 report derivation origin, not live L1 head"
    );
}
