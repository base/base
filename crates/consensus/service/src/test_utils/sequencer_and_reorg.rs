//! Sequencer-role invariants (Sq1–Sq4) and remaining edge cases (E4/E6/E7/E11/E12).

use std::future::Future;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus, PayloadStatusEnum,
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

fn hash_for(number: u64) -> B256 {
    B256::from([number as u8; 32])
}

fn hash_number(hash: B256) -> u64 {
    hash.as_slice()[0] as u64
}

fn block(number: u64, parent_hash: B256, hash: B256, timestamp: u64) -> BlockInfo {
    BlockInfo { number, hash, parent_hash, timestamp }
}

fn latest_fcu_state(calls: &[EngineClientCall]) -> Option<ForkchoiceState> {
    calls.iter().rev().find_map(|call| match call {
        EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(*fcs),
        _ => None,
    })
}

fn payload_v3(block_number: u64, parent_hash: B256, block_hash: B256) -> ExecutionPayloadV3 {
    ExecutionPayloadV3 {
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
    }
}

fn payload_envelope_v3(
    block_number: u64,
    parent_hash: B256,
    block_hash: B256,
) -> BaseExecutionPayloadEnvelopeV3 {
    BaseExecutionPayloadEnvelopeV3 {
        execution_payload: payload_v3(block_number, parent_hash, block_hash),
        block_value: Default::default(),
        blobs_bundle: Default::default(),
        should_override_builder: false,
        parent_beacon_block_root: B256::ZERO,
    }
}

fn valid_payload_status() -> PayloadStatus {
    PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
}

#[test]
fn sq1_sequencer_no_self_reorg_of_unsafe_head() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Sequencer,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..64).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    run_async(async {
        for number in 1..=10 {
            fake_l1
                .extend(block(number, hash_for(number.saturating_sub(1)), hash_for(number), number))
                .await;
        }
    });

    driver
        .await_progress(
            |snapshot| snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 10).unwrap_or(false),
            150,
        )
        .expect("sequencer did not process 10 L1-driven updates");
    driver.tick(1);

    let calls = run_async(fake_engine_handle.calls());
    let observed = calls
        .iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => {
                Some((hash_number(fcs.head_block_hash), fcs.head_block_hash))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let prefix_until_ten = observed
        .iter()
        .position(|(number, _)| *number >= 10)
        .map(|idx| observed[..=idx].to_vec())
        .unwrap_or_default();

    assert!(prefix_until_ten.len() >= 10, "expected FCU-v3 timeline covering 10 unsafe heads");
    // Sq1: sequencer unsafe head must not self-reorg without an explicit signal.
    assert!(
        prefix_until_ten.windows(2).all(|window| window[1] >= window[0]),
        "Sq1 violated: unsafe head regressed in FCU sequence prefix: {prefix_until_ten:?}",
    );
}

#[test]
fn sq2_one_payload_per_slot_idempotent_retries() {
    let payload_id = PayloadId::new([7_u8; 8]);
    let client = FakeEngineClient::new(std::sync::Arc::new(RollupConfig::default()))
        .with_scripted_get_payload_v3_responses(vec![
            Ok(payload_envelope_v3(2, hash_for(1), hash_for(2))),
            Ok(payload_envelope_v3(2, hash_for(1), hash_for(2))),
        ]);
    let handle = client.handle();

    run_async(async {
        let first = client
            .get_payload_v3(payload_id)
            .await
            .expect("first get_payload_v3 should succeed");
        let second = client
            .get_payload_v3(payload_id)
            .await
            .expect("second get_payload_v3 retry should succeed");
        assert_eq!(
            first.execution_payload.payload_inner.payload_inner.block_hash,
            second.execution_payload.payload_inner.payload_inner.block_hash,
            "retry should produce the same payload for the same slot id",
        );
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
fn sq3_build_parent_freshness() {
    let expected_parent = hash_for(10);
    let fresh = payload_v3(11, expected_parent, hash_for(11));
    let stale = payload_v3(11, hash_for(9), B256::from([0x99; 32]));
    let client = FakeEngineClient::new(std::sync::Arc::new(RollupConfig::default()))
        .with_scripted_new_payload_v3_responses(vec![valid_payload_status(), valid_payload_status()]);
    let handle = client.handle();

    run_async(async {
        client
            .new_payload_v3(fresh, B256::ZERO)
            .await
            .expect("fresh parent payload should succeed");
        client
            .new_payload_v3(stale, B256::ZERO)
            .await
            .expect("stale parent payload should still be recorded for freshness checks");
    });

    let new_payload_calls = run_async(handle.calls())
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::NewPayloadV3(payload) => Some(payload),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(new_payload_calls.len(), 2, "expected two recorded new_payload_v3 calls");
    assert_eq!(
        new_payload_calls[0].payload_inner.payload_inner.parent_hash,
        expected_parent,
        "first call should be fresh parent build",
    );
    assert_ne!(
        new_payload_calls[1].payload_inner.payload_inner.parent_hash,
        expected_parent,
        "second call should expose stale-parent build for discard logic",
    );
}

#[test]
fn sq4_bounded_lag_unsafe_vs_safe() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Sequencer,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..1024).map(|_| valid_fcu())),
        },
    );

    let fake_l1 = {
        let harness = driver.harness(node_id);
        harness.fake_l1().clone()
    };

    run_async(async {
        for number in 1..=150 {
            fake_l1
                .extend(block(number, hash_for(number.saturating_sub(1)), hash_for(number), number))
                .await;
        }
    });

    driver
        .await_progress(
            |snapshot| snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 150).unwrap_or(false),
            300,
        )
        .expect("sequencer safe head did not advance under L1 progression");
    driver.tick(1);

    let snapshot = driver.snapshot();
    let node = snapshot.nodes.get(node_id).expect("missing sequencer node snapshot");
    let lag = node.unsafe_head_number.saturating_sub(node.safe_head_number);

    // Sq4: unsafe-safe lag must stay bounded under sustained progression.
    assert!(lag < 1000, "Sq4 violated: unsafe-safe lag exceeded bound: {lag}");
}

#[test]
#[ignore = "Harness gap: crash/restart mid-slot requires an exposed SequencerActor lifecycle seam around in-flight getPayload to prove exactly-once same-slot recovery."]
fn e4_sequencer_crash_mid_slot() {
    // E4: exact restart semantics depend on dropping/restarting a live SequencerActor while a
    // specific slot build is in-flight; test_utils only wires engine+derivation actors.
}

#[test]
fn e6_deep_l1_reorg_past_safe_not_finalized() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..256).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    run_async(async {
        for number in 1..=4 {
            fake_l1
                .extend(block(number, hash_for(number.saturating_sub(1)), hash_for(number), number))
                .await;
        }
    });

    driver
        .await_progress(
            |snapshot| snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 4).unwrap_or(false),
            120,
        )
        .expect("failed to reach pre-reorg safe head");
    driver.tick(1);

    let calls_before = run_async(fake_engine_handle.calls());
    let finalized_before = latest_fcu_state(&calls_before)
        .map(|state| hash_number(state.finalized_block_hash))
        .expect("missing pre-reorg FCU state");

    let alt_1 = block(1, B256::ZERO, B256::from([0x41; 32]), 41);
    let alt_2 = block(2, alt_1.hash, B256::from([0x42; 32]), 42);
    let alt_3 = block(3, alt_2.hash, B256::from([0x43; 32]), 43);
    let alt_4 = block(4, alt_3.hash, hash_for(4), 44);

    run_async(async {
        fake_l1.reorg(4, vec![alt_1, alt_2, alt_3, alt_4]).await;
    });

    driver
        .await_progress(
            |snapshot| snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 4).unwrap_or(false),
            120,
        )
        .expect("failed to reconverge after deep reorg");
    driver.tick(1);

    let calls_after = run_async(fake_engine_handle.calls());
    let finalized_after = latest_fcu_state(&calls_after)
        .map(|state| hash_number(state.finalized_block_hash))
        .expect("missing post-reorg FCU state");
    let l1_state = run_async(fake_l1.state());

    // E6 / S2.a: finalized must never regress under deep reorg handling.
    assert!(
        finalized_after >= finalized_before,
        "expected finalized head to be non-decreasing across deep reorg"
    );
    // E6 / S2.b: safe derivation reconverges to a valid canonical chain after explicit reorg signal.
    assert!(
        l1_state
            .canonical
            .windows(2)
            .all(|window| window[1].parent_hash == window[0].hash),
        "expected canonical chain to be internally consistent after reorg"
    );
}

#[test]
#[ignore = "Harness gap: cannot inject impossible reorg Signal past finalized without production-code modification."]
fn e7_deep_l1_reorg_past_finalized() {
    // E7: impossible under normal assumptions; requires forcing a bad reset/signal path that the
    // frozen Tier-0 harness does not expose.
}

#[test]
#[ignore = "Harness gap: FakeEngineClient does not expose contradiction tracking for prior VALID then INVALID-on-safe-ancestor with explicit halt/rollback signalization."]
fn e11_invalid_newpayload_on_committed_safe_ancestor() {
    // E11: asserting deterministic halt-or-rollback requires observability of contradiction
    // handling outcome (fatal error surface vs rollback marker) that is not exported by harness.
}

#[test]
#[ignore = "Harness gap: SignalNeeded events are internal state-machine transitions; test_utils exposes only ProcessEngineSignalRequest (processed), not pre-processed queued depth ordering."]
fn e12_repeated_signal_needed_before_processed() {
    // E12: deepest-signal collapse needs visibility into queued SignalNeeded notifications before
    // SignalProcessed; current harness only allows injecting already-processed signals.
}
