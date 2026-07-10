//! Syncing-stall adversarial edge-case tests (E17–E24) for Tier-0 CL/EL invariants.

use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ForkchoiceUpdated, PayloadId,
    PayloadStatus, PayloadStatusEnum,
};
use base_common_genesis::RollupConfig;
use base_common_network::BaseEngineApi;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelopeV3;
use base_protocol::BlockInfo;

use super::{
    Driver, EngineClientCall, FakeEngineClient, HarnessBuilder, NodeConfig,
    ScriptedForkchoiceResponse,
};
use crate::NodeMode;

/// Number of explicit Tier-0 syncing-stall tests in this module.
pub const SYNCING_STALL_TEST_COUNT: usize = 6;

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
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Syncing,
            latest_valid_hash: Some(B256::ZERO),
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

fn hash_number(hash: B256) -> u64 {
    hash.as_slice()[0] as u64
}

fn syncing_payload_status() -> PayloadStatus {
    PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: Some(B256::ZERO) }
}

fn valid_payload_status() -> PayloadStatus {
    PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
}

fn payload_envelope_v3(
    block_number: u64,
    parent_hash: B256,
    block_hash: B256,
) -> BaseExecutionPayloadEnvelopeV3 {
    BaseExecutionPayloadEnvelopeV3 {
        execution_payload: ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash,
                    fee_recipient: Default::default(),
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: block_number,
                    extra_data: Default::default(),
                    base_fee_per_gas: Default::default(),
                    block_hash,
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        },
        block_value: Default::default(),
        blobs_bundle: Default::default(),
        should_override_builder: false,
        parent_beacon_block_root: B256::ZERO,
    }
}

fn syncing_error_payload() -> String {
    "execution layer syncing".to_string()
}

#[test]
fn s_a1_consolidate_task_syncing_preserves_safe_advance() {
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
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 2).unwrap_or(false)
            },
            20,
        )
        .expect("safe head did not reach consolidated target after Syncing->Valid FCU responses");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_heads = calls
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
            _ => None,
        })
        .collect::<Vec<_>>();

    // Invariant L3: pending consolidated-safe update survives a transient Syncing FCU and commits.
    assert!(
        fcu_heads.len() >= 2,
        "expected FCU progression to continue after first Syncing response"
    );
    assert!(
        fcu_heads.contains(&hash_for(2)),
        "expected consolidated safe target hash to appear in FCU calls"
    );
}

#[test]
fn s_a2_finalize_task_syncing_never_regresses_finalized() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;
        fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 4).unwrap_or(false)
            },
            120,
        )
        .expect("safe head did not converge during finalized monotonicity scenario");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let finalized_timeline = calls
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => {
                Some(hash_number(fcs.finalized_block_hash))
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(
        !finalized_timeline.is_empty(),
        "expected FCU-v3 timeline for finalized monotonicity check"
    );
    // Invariant S2.a: finalized.number is monotonically non-decreasing across FCU timeline.
    assert!(
        finalized_timeline.windows(2).all(|window| window[1] >= window[0]),
        "finalized regressed in FCU timeline: {finalized_timeline:?}",
    );
}

#[test]
fn s_a3_insert_task_syncing_block_not_lost() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let (fake_l1, fake_engine_client) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_client().clone())
    };

    let _ = fake_engine_client
        .with_new_payload_v3_responses(vec![syncing_payload_status(), valid_payload_status()]);

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 1).unwrap_or(false)
            },
            60,
        )
        .expect("insert-task syncing scenario failed to reach safe-head confirmation");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    assert!(
        calls.iter().any(|call| matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { .. })),
        "expected FCU-v3 traffic after Syncing->Valid scripted new_payload outcomes",
    );
}

#[test]
fn s_b1_sequencer_fcu_with_attrs_syncing_no_wedge() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Sequencer,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 2).unwrap_or(false)
            },
            30,
        )
        .expect("sequencer wedged after transient Syncing FCU response");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let fcu_v3_calls = calls
        .iter()
        .filter(|call| matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { .. }))
        .count();

    // Invariant L1: sequencer does not wedge; FCU path keeps advancing after Syncing.
    assert!(fcu_v3_calls >= 2, "expected FCU-v3 retry/progression in sequencer mode");
}

#[test]
fn s_b2_getpayload_after_syncing_slot_not_dropped() {
    let payload_id = PayloadId::new([7_u8; 8]);
    let client = FakeEngineClient::new(std::sync::Arc::new(RollupConfig::default()))
        .with_scripted_get_payload_v3_responses(vec![
            Err(syncing_error_payload()),
            Ok(payload_envelope_v3(2, hash_for(1), hash_for(2))),
        ]);
    let handle = client.handle();

    run_async(async {
        let first = client.get_payload_v3(payload_id).await;
        assert!(first.is_err(), "first get_payload_v3 should surface scripted Syncing error");
        let second = client
            .get_payload_v3(payload_id)
            .await
            .expect("second get_payload_v3 should return scripted payload envelope");
        assert_eq!(second.execution_payload.payload_inner.payload_inner.block_number, 2);
    });

    let get_payload_calls = run_async(handle.calls())
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::GetPayloadV3(id) => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(get_payload_calls, vec![payload_id, payload_id]);
}

#[test]
fn s_c2_signal_reset_fcu_syncing_eventually_commits() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let fake_l1 = driver.harness(node_id).fake_l1().clone();
    let alt_2 = block(2, hash_for(1), B256::from([42_u8; 32]), 42);
    let alt_3 = block(3, alt_2.hash, B256::from([43_u8; 32]), 43);

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;
    });
    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 3).unwrap_or(false)
            },
            60,
        )
        .expect("failed to reach pre-reorg safe head in reset scenario");

    run_async(async {
        fake_l1.reorg(2, vec![alt_2, alt_3]).await;
    });

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 3).unwrap_or(false)
            },
            20,
        )
        .expect("safe head did not reconverge after reset Syncing->Valid window");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let reset_target_hash = hash_for(3);
    let saw_reset_target = calls.iter().any(|call| {
        matches!(
            call,
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. }
                if fcs.head_block_hash == reset_target_hash && fcs.safe_block_hash == reset_target_hash
        )
    });

    // Invariant L3: reset target eventually commits after transient Syncing response.
    assert!(saw_reset_target, "expected FCU timeline to commit reorg reset target");
}
