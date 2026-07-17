//! Direct Tier-0 invariant tests for currently-untested strategy.md labels.

use alloy_consensus::Header as ConsensusHeader;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_consensus_engine::ConsolidateInput;
use base_consensus_safedb::SafeHeadResponse;
use base_protocol::{BlockInfo, L2BlockInfo};

use super::{Driver, EngineClientCall, HarnessBuilder, NodeConfig, ScriptedForkchoiceResponse};
use crate::{EngineActorRequest, NodeMode};

fn valid_fcu() -> ScriptedForkchoiceResponse {
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(B256::ZERO),
        },
        payload_id: None,
    })
}

fn invalid_fcu() -> ScriptedForkchoiceResponse {
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Invalid { validation_error: "test-invalid".into() },
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
    debug_assert!(number <= u8::MAX as u64, "fake block hash encoding wraps above 255");
    B256::from([number as u8; 32])
}

fn block(number: u64, parent_hash: B256, hash: B256, timestamp: u64) -> BlockInfo {
    BlockInfo { number, hash, parent_hash, timestamp }
}

#[tokio::test(start_paused = true)]
async fn v2_bootstrap_consistency() {
    // Invariant V2: bootstrap heads consistent; EL sync completion behavior only after derivation progress.
    let mut driver = Driver::new();
    let initial_safe = block(3, hash_for(2), hash_for(3), 3);
    let node_id = driver
        .spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new()
                    .with_initial_safedb([SafeHeadResponse {
                        l1_block: initial_safe.id(),
                        safe_head: initial_safe.id(),
                    }])
                    .with_scripted_el_responses([
                        syncing_fcu(),
                        syncing_fcu(),
                        valid_fcu(),
                        valid_fcu(),
                        valid_fcu(),
                        valid_fcu(),
                    ]),
            },
        )
        .await;

    let (fake_l1, safedb) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_safedb_handle().clone())
    };

    let boot_latest = safedb.latest().await.expect("expected pre-populated safedb entry");
    assert_eq!(boot_latest.safe_head.number, 3, "bootstrap must start from seeded safe head");

    for _ in 0..20 {
        driver.tick(1).await;
        let safe_now = driver
            .snapshot()
            .await
            .nodes
            .get(node_id)
            .map(|node| node.safe_head_number)
            .unwrap_or_default();
        assert_eq!(safe_now, 3, "without new L1 activity, bootstrap safe head must remain stable");
    }

    fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;

    let mut saw_transition_to_derived_progress = false;
    let mut transition_count = 0_u64;
    let mut last_was_progressed = false;
    for _ in 0..120 {
        let safe_now = driver
            .snapshot()
            .await
            .nodes
            .get(node_id)
            .map(|node| node.safe_head_number)
            .unwrap_or_default();
        let progressed = safe_now > 3;
        if progressed && !last_was_progressed {
            transition_count += 1;
        }
        saw_transition_to_derived_progress |= progressed;
        last_was_progressed = progressed;
        driver.tick(1).await;
    }

    assert!(
        saw_transition_to_derived_progress,
        "expected derivation-driven progress after L1 extension"
    );
    assert_eq!(
        transition_count, 1,
        "V2 violated: expected a single bootstrap->derived progression transition",
    );
}

#[tokio::test(start_paused = true)]
async fn l4_confirmations_observed_by_derivation() {
    // Invariant L4: applied attributes eventually surface as safe-head confirmations.
    let mut driver = Driver::new();
    let node_id = driver
        .spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new()
                    .with_scripted_el_responses((0..32).map(|_| valid_fcu())),
            },
        )
        .await;

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    // dispatch_safe_l2_for injects one synthetic FCU log entry per extend() call; real
    // engine-originated FCUs appear after that, so skip pre_extend_count+1 entries.
    let pre_extend_count = fake_engine_handle.call_count();
    fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;

    driver
        .await_progress(
            |snapshot| {
                snapshot.nodes.get(node_id).map(|node| node.safe_head_number >= 1).unwrap_or(false)
            },
            100,
        )
        .await
        .expect("safe head did not advance for derivation confirmation check");

    let calls = fake_engine_handle.calls().await;
    let saw_safe_confirmation = calls[pre_extend_count + 1..].iter().any(|call| {
        matches!(
            call,
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. }
                if fcs.safe_block_hash == hash_for(1)
        )
    });
    assert!(saw_safe_confirmation, "expected engine-originated FCU safe-head confirmation for derived block");
}

#[tokio::test(start_paused = true)]
async fn l5_no_cross_actor_deadlock() {
    // Invariant L5: no wait-for cycle across engine/derivation/network actors.
    let mut driver = Driver::new();
    let node_id = driver
        .spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new().with_scripted_el_responses([
                    syncing_fcu(),
                    valid_fcu(),
                    valid_fcu(),
                    valid_fcu(),
                    valid_fcu(),
                    valid_fcu(),
                ]),
            },
        )
        .await;

    let fake_l1 = driver.harness(node_id).fake_l1().clone();

    fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
    fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;

    driver.tick(200).await;

    let safe_number = driver
        .snapshot()
        .await
        .nodes
        .get(node_id)
        .map(|node| node.safe_head_number)
        .unwrap_or_default();

    assert!(safe_number >= 3, "L5 violated: safe-head did not reach block 3 after 200 ticks");
}

#[tokio::test(start_paused = true)]
async fn e2e_invalid_fcu_reset_and_recovery() {
    // Regression: 2026-06-25 mainnet incident.
    //
    // When the EL returns INVALID for a forkchoice update the engine actor must escalate the
    // error to Reset (not Temporary). Before the fix the error was classified Temporary and the
    // actor retried the same invalid FCU forever, wedging the node until manual restart.
    //
    // Observable difference (on the head hash of the FCU that follows the INVALID one):
    //   pre-fix  (Temporary): the engine retries the SAME poisoned head in place — every FCU
    //                         carries hash_for(1), never resetting.
    //   post-fix (Reset):     INVALID escalates to Reset, the forkchoice is re-derived from
    //                         genesis, so a subsequent FCU carries the genesis head instead.
    let mut driver = Driver::new();
    let node_id = driver
        .spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new()
                    .with_reset_recovery_support()
                    .with_scripted_el_responses([invalid_fcu(), valid_fcu()]),
            },
        )
        .await;

    let (engine_tx, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.engine_request_sender(), harness.fake_engine_handle().clone())
    };

    // Send ProcessSafeL2SignalRequest directly instead of via fake_l1.extend().
    // fake_l1.extend() calls inject_fcu_v3_call() which pops the first scripted response before
    // the engine actor ever calls fork_choice_updated_v3(), defeating the test.
    engine_tx
        .send(EngineActorRequest::ProcessSafeL2SignalRequest(ConsolidateInput::BlockInfo(
            L2BlockInfo { block_info: block(1, B256::ZERO, hash_for(1), 1), ..Default::default() },
        )))
        .await
        .expect("engine actor must accept the signal");

    driver.tick(50).await;

    let fcu_heads: Vec<B256> = fake_engine_handle
        .calls()
        .await
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
            _ => None,
        })
        .collect();

    assert_eq!(
        fcu_heads.first().copied(),
        Some(hash_for(1)),
        "expected the first FCU to carry the poisoned head {:?}, got {:?}",
        hash_for(1),
        fcu_heads.first()
    );

    let genesis_head = ConsensusHeader::default().hash_slow();
    assert_eq!(
        fcu_heads.get(1).copied(),
        Some(genesis_head),
        "expected the FCU following the INVALID one to carry the genesis head {genesis_head:?} \
         (reset re-derived the forkchoice); got {:?}. pre-fix (Temporary) retries the poisoned \
         head {:?} in place instead of resetting. full heads: {fcu_heads:?}",
        fcu_heads.get(1),
        hash_for(1)
    );
}
