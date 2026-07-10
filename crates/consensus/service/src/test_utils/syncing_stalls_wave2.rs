//! Syncing-stall wave-2 edge-case tests (E22, E26–E31) for Tier-0 CL/EL invariants.

use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use base_protocol::BlockInfo;
use rstest::rstest;

use super::{Driver, EngineClientCall, HarnessBuilder, NodeConfig, ScriptedForkchoiceResponse};
use crate::NodeMode;

/// Number of explicit Tier-0 syncing-stall wave-2 tests in this module.
pub const SYNCING_STALL_WAVE2_TEST_COUNT: usize = 7;

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build helper runtime")
        .block_on(future)
}

fn valid_fcu() -> ScriptedForkchoiceResponse {
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(B256::ZERO),
        },
        payload_id: None,
    })
}

fn syncing_fcu() -> ScriptedForkchoiceResponse {
    syncing_fcu_with_latest(B256::ZERO)
}

fn syncing_fcu_with_latest(latest_valid_hash: B256) -> ScriptedForkchoiceResponse {
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: Some(latest_valid_hash),
        },
        payload_id: None,
    })
}

fn hash_for(number: u64) -> B256 {
    B256::from([number as u8; 32])
}

fn block(number: u64, parent_hash: B256, hash: B256, timestamp: u64) -> BlockInfo {
    BlockInfo { number, hash, parent_hash, timestamp }
}

fn fcu_v3_states(calls: &[EngineClientCall]) -> Vec<ForkchoiceState> {
    calls
        .iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(*fcs),
            _ => None,
        })
        .collect()
}

#[test]
fn s_b3_sequencer_own_newpayload_syncing_no_wedge() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Sequencer,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 1).unwrap_or(false)
            },
            30,
        )
        .expect("sequencer wedged under self new_payload Syncing scenario");
}

#[test]
#[ignore = "Harness gap: deterministic Tier-0 harness does not expose a reliable Syncing->Valid retry-flip path for 100 consecutive FCU outcomes with observable unsafe-head convergence. Test body kept for follow-up."]
fn s_d1_long_syncing_chain_100_no_retry_storm() {
    let mut driver = Driver::new();
    let scripted =
        (0..100).map(|_| syncing_fcu()).chain((0..8).map(|_| valid_fcu())).collect::<Vec<_>>();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig { builder: HarnessBuilder::new().with_scripted_el_responses(scripted) },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    // Liveness gate: must eventually observe the injected L1 signal in harness snapshots.
    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 1).unwrap_or(false)
            },
            300,
        )
        .expect("did not observe liveness under long Syncing FCU window");
    driver.tick(10);

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_calls = fcu_v3_states(&calls);
    let total_fcu = fcu_calls.len();

    // S3 proxy: retries are bounded (no retry storm / no exponential blow-up).
    assert!(
        total_fcu < 130,
        "expected bounded FCU retry count after 100 Syncing responses, got {total_fcu}",
    );
    assert!(
        total_fcu >= 100,
        "expected to observe the scripted Syncing FCU window, got {total_fcu} calls",
    );
}

#[test]
fn s_d2_alternating_valid_syncing_el_sync_finished_sticky() {
    let mut driver = Driver::new();
    let scripted = (0..20)
        .map(|idx| if idx % 2 == 0 { valid_fcu() } else { syncing_fcu() })
        .chain((0..16).map(|_| valid_fcu()))
        .collect::<Vec<_>>();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig { builder: HarnessBuilder::new().with_scripted_el_responses(scripted) },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        for number in 1..=8 {
            fake_l1
                .extend(block(number, hash_for(number.saturating_sub(1)), hash_for(number), number))
                .await;
        }
    });

    // Liveness gate: alternating statuses should not wedge harness-level progression.
    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 8).unwrap_or(false)
            },
            120,
        )
        .expect("alternating Valid/Syncing FCU responses wedged progression");
    driver.tick(10);

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_states = fcu_v3_states(&calls);
    let distinct_heads = fcu_states
        .iter()
        .map(|state| state.head_block_hash)
        .collect::<std::collections::HashSet<_>>()
        .len();

    // S3: el_sync_finished sticky-true proxy — forward-progress FCU heads keep advancing,
    // proving we do not restart from scratch on every Syncing flip.
    assert!(
        distinct_heads >= 8,
        "expected FCU trace to include advancing heads, got {distinct_heads}"
    );
}

#[test]
fn s_d3_task_retried_after_syncing_identical_input_idempotent() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot
                    .nodes
                    .get(node_id)
                    .map(|node| node.unsafe_head_number >= 1)
                    .unwrap_or(false)
            },
            30,
        )
        .expect("unsafe head did not converge after Syncing->Valid retry");
    driver.tick(5);

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_states = fcu_v3_states(&calls);
    assert!(fcu_states.len() >= 2, "expected at least two FCU-v3 calls for retry check");

    // S6: idempotence proxy — retry after Syncing must preserve head/safe target intent.
    assert_eq!(
        fcu_states[0].head_block_hash, fcu_states[1].head_block_hash,
        "expected retry to preserve FCU head target after Syncing",
    );
    assert_eq!(
        fcu_states[0].safe_block_hash, fcu_states[1].safe_block_hash,
        "expected retry to preserve FCU safe target after Syncing",
    );

    let final_state = driver.harness(node_id).latest_engine_state();
    // S6: resulting safe head equals the single-Valid outcome for this one-block scenario.
    assert_eq!(
        final_state.sync_state.safe_head().block_info.number,
        1,
        "safe head must converge to the single Valid-equivalent result",
    );
}

#[test]
#[ignore = "Harness gap: test_utils cannot inject a direct all-four-fields EngineSyncStateUpdate (unsafe/local_safe/safe/finalized) to assert composite Syncing commit matrix. Test body kept for follow-up."]
fn s_e1_composite_all_four_fields_syncing() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    driver.tick(20);
}

#[rstest]
#[case::el_sync_finished_true(true)]
#[case::el_sync_finished_false(false)]
#[ignore = "Harness gap: finalized-only EngineSyncStateUpdate path is not directly triggerable/observable through frozen Tier-0 harness; cannot parameterize el_sync_finished branch behavior without new seam. Test body kept for follow-up."]
fn s_e2_composite_finalized_only_syncing(#[case] el_sync_finished: bool) {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    let timestamp = if el_sync_finished { 1 } else { 2 };
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), timestamp)).await;
    });

    driver.tick(20);
}

#[test]
fn s_e3_latest_valid_hash_thrashing_across_syncing() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu_with_latest(B256::from([0xAA; 32])),
                syncing_fcu_with_latest(B256::from([0xBB; 32])),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    let initial_state = driver.harness(node_id).latest_engine_state().sync_state;

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 1).unwrap_or(false)
            },
            30,
        )
        .expect("did not observe post-signal liveness while processing Syncing responses");
    driver.tick(10);

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_states = fcu_v3_states(&calls);
    assert!(fcu_states.len() >= 2, "expected at least two FCU-v3 calls for latestValidHash thrash");
    let final_state = driver.harness(node_id).latest_engine_state().sync_state;
    // S5: Syncing responses commit no heads; latestValidHash in Syncing must be ignored.
    assert_eq!(
        final_state.unsafe_head().block_info,
        initial_state.unsafe_head().block_info,
        "unsafe head changed across Syncing-only responses",
    );
    assert_eq!(
        final_state.safe_head().block_info,
        initial_state.safe_head().block_info,
        "safe head changed across Syncing-only responses",
    );
    assert_eq!(
        final_state.local_safe_head().block_info,
        initial_state.local_safe_head().block_info,
        "local safe head changed across Syncing-only responses",
    );
    assert_eq!(
        final_state.finalized_head().block_info,
        initial_state.finalized_head().block_info,
        "finalized head changed across Syncing-only responses",
    );
}
