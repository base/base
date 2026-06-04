//! Action tests for consensus sync-status L1 reporting.

use std::sync::Arc;

use base_action_harness::{ActionL1BlockFetcher, ActionTestHarness, SharedL1Chain};
use base_consensus_node::L1WatcherQueryExecutor;
use base_consensus_rpc::L1WatcherQueries;
use tokio::sync::{oneshot, watch};

#[tokio::test]
async fn sync_status_current_l1_tracks_verifier_depth_origin_not_l1_head() {
    const L1_HEAD: u64 = 100;
    const VERIFIER_L1_CONFS: u64 = 4;

    let mut harness = ActionTestHarness::default();
    harness.mine_l1_blocks(L1_HEAD);

    let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
    let derivation_origin = harness.l1.block_info_at(L1_HEAD - VERIFIER_L1_CONFS);
    let live_head = harness.l1.tip_info();
    let (_derivation_origin_tx, derivation_origin_rx) = watch::channel(Some(derivation_origin));
    let executor = L1WatcherQueryExecutor::new(
        Arc::new(harness.rollup_config.clone()),
        Arc::new(ActionL1BlockFetcher::new(l1_chain)),
        derivation_origin_rx,
    );
    let (sender, receiver) = oneshot::channel();

    executor.execute(L1WatcherQueries::L1State(sender)).await;

    let state = receiver.await.expect("state query should return a response");
    assert_eq!(state.current_l1, Some(derivation_origin));
    assert_eq!(state.head_l1, Some(live_head));
    assert_ne!(
        state.current_l1, state.head_l1,
        "verifier_l1_confs should make current_l1 report derivation origin, not live L1 head"
    );
}
