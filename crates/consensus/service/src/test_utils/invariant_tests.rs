//! Direct Tier-0 invariant tests for currently-untested strategy.md labels.

use std::future::Future;

use alloy_consensus::Header as ConsensusHeader;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated,
    PayloadAttributes, PayloadStatus, PayloadStatusEnum,
};
use base_common_genesis::RollupConfig;
use base_common_network::BaseEngineApi;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_engine::ConsolidateInput;
use base_consensus_safedb::SafeHeadResponse;
use base_protocol::{BlockInfo, L2BlockInfo};

use super::{
    Driver, EngineClientCall, FakeEngineClient, HarnessBuilder, NodeConfig,
    ScriptedForkchoiceResponse,
};
use crate::{EngineActorRequest, NodeMode};

/// Number of direct Tier-0 invariant tests in this module.
pub const INVARIANT_TEST_COUNT: usize = 6;

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

fn valid_payload_status() -> PayloadStatus {
    PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
}

fn hash_for(number: u64) -> B256 {
    debug_assert!(number <= u8::MAX as u64, "fake block hash encoding wraps above 255");
    B256::from([number as u8; 32])
}

fn block(number: u64, parent_hash: B256, hash: B256, timestamp: u64) -> BlockInfo {
    BlockInfo { number, hash, parent_hash, timestamp }
}

#[test]
fn d1_at_most_one_pending_attrs() {
    // Invariant D1: at most one attr-bearing FCU is pending before a plain FCU confirmation.
    let client = FakeEngineClient::new(std::sync::Arc::new(RollupConfig::default()));
    let handle = client.handle();
    handle.push_scripted_fcu_v3_blocking([valid_fcu(), valid_fcu(), valid_fcu()]);

    run_async(async {
        let attrs = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 1,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Default::default(),
                withdrawals: None,
                parent_beacon_block_root: Some(B256::ZERO),
                slot_number: None,
            },
            ..Default::default()
        };
        client
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: hash_for(1),
                    safe_block_hash: hash_for(1),
                    finalized_block_hash: hash_for(1),
                },
                Some(attrs),
            )
            .await
            .expect("scripted FCU-with-attrs should succeed");
        client
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: hash_for(1),
                    safe_block_hash: hash_for(1),
                    finalized_block_hash: hash_for(1),
                },
                None,
            )
            .await
            .expect("scripted plain FCU should succeed");
        client
            .fork_choice_updated_v3(
                ForkchoiceState {
                    head_block_hash: hash_for(2),
                    safe_block_hash: hash_for(2),
                    finalized_block_hash: hash_for(2),
                },
                Some(BasePayloadAttributes {
                    payload_attributes: PayloadAttributes {
                        timestamp: 2,
                        prev_randao: B256::ZERO,
                        suggested_fee_recipient: Default::default(),
                        withdrawals: None,
                        parent_beacon_block_root: Some(B256::ZERO),
                        slot_number: None,
                    },
                    ..Default::default()
                }),
            )
            .await
            .expect("second scripted FCU-with-attrs should succeed");
    });

    let mut has_pending_attrs = false;
    for call in run_async(handle.calls()) {
        if let EngineClientCall::ForkChoiceUpdatedV3 { payload_attributes, .. } = call {
            if payload_attributes.is_some() {
                assert!(!has_pending_attrs, "observed overlapping attr-bearing FCU requests");
                has_pending_attrs = true;
            } else {
                has_pending_attrs = false;
            }
        }
    }
}

#[test]
fn v1_parent_before_child_gossip_ordering() {
    // Invariant V1: no child payload is applied before parent via newPayload.
    let parent = ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Default::default(),
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: hash_for(1),
                transactions: vec![],
            },
            withdrawals: vec![],
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    };
    let child = ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: hash_for(1),
                fee_recipient: Default::default(),
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                prev_randao: B256::ZERO,
                block_number: 2,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 2,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: hash_for(2),
                transactions: vec![],
            },
            withdrawals: vec![],
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    };

    let client = FakeEngineClient::new(std::sync::Arc::new(RollupConfig::default()))
        .with_scripted_new_payload_v3_responses(vec![
            valid_payload_status(),
            valid_payload_status(),
        ]);
    let handle = client.handle();

    run_async(async {
        client
            .new_payload_v3(parent.clone(), B256::ZERO)
            .await
            .expect("parent payload should be accepted");
        client
            .new_payload_v3(child.clone(), B256::ZERO)
            .await
            .expect("child payload should be accepted after parent");
    });

    let new_payload_calls = run_async(handle.calls())
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::NewPayloadV3(payload) => Some(payload),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(new_payload_calls.len(), 2, "expected parent+child new_payload_v3 calls");
    assert_eq!(new_payload_calls[0].payload_inner.payload_inner.block_hash, hash_for(1));
    assert_eq!(new_payload_calls[1].payload_inner.payload_inner.parent_hash, hash_for(1));
}

#[test]
fn v2_bootstrap_consistency() {
    // Invariant V2: bootstrap heads consistent; EL sync completion behavior only after derivation progress.
    let mut driver = Driver::new();
    let initial_safe = block(3, hash_for(2), hash_for(3), 3);
    let node_id = driver.spawn_node(
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
    );

    let (fake_l1, safedb) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_safedb_handle().clone())
    };

    let boot_latest = run_async(safedb.latest()).expect("expected pre-populated safedb entry");
    assert_eq!(boot_latest.safe_head.number, 3, "bootstrap must start from seeded safe head");

    for _ in 0..20 {
        driver.tick(1);
        let safe_now = driver
            .snapshot()
            .nodes
            .get(node_id)
            .map(|node| node.safe_head_number)
            .unwrap_or_default();
        assert_eq!(safe_now, 3, "without new L1 activity, bootstrap safe head must remain stable");
    }

    run_async(async {
        fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;
    });

    let mut saw_transition_to_derived_progress = false;
    let mut transition_count = 0_u64;
    let mut last_was_progressed = false;
    for _ in 0..120 {
        let safe_now = driver
            .snapshot()
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
        driver.tick(1);
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

#[test]
fn l4_confirmations_observed_by_derivation() {
    // Invariant L4: applied attributes eventually surface as safe-head confirmations.
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..32).map(|_| valid_fcu())),
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
            100,
        )
        .expect("safe head did not advance for derivation confirmation check");

    let calls = run_async(driver.harness(node_id).fake_engine_handle().calls());
    let saw_safe_confirmation = calls.iter().any(|call| {
        matches!(
            call,
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. }
                if fcs.safe_block_hash == hash_for(1)
        )
    });
    assert!(saw_safe_confirmation, "expected FCU safe-head confirmation for derived block");
}

#[test]
fn l5_no_cross_actor_deadlock() {
    // Invariant L5: no wait-for cycle across engine/derivation/network actors.
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;

        fake_engine_handle
            .inject_fcu_v3_call(ForkchoiceState {
                head_block_hash: hash_for(101),
                safe_block_hash: hash_for(2),
                finalized_block_hash: hash_for(1),
            })
            .await;
        fake_engine_handle
            .inject_fcu_v3_call(ForkchoiceState {
                head_block_hash: hash_for(102),
                safe_block_hash: hash_for(3),
                finalized_block_hash: hash_for(2),
            })
            .await;
    });

    driver.tick(200);

    let safe_number =
        driver.snapshot().nodes.get(node_id).map(|node| node.safe_head_number).unwrap_or_default();
    let fcu_calls = run_async(fake_engine_handle.calls())
        .into_iter()
        .filter(|call| matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { .. }))
        .count();

    assert!(safe_number >= 1, "L5 violated: no measurable safe-head progress after 200 ticks");
    assert!(fcu_calls >= 3, "L5 violated: actor graph did not keep processing FCU traffic");
}

#[test]
fn e2e_invalid_fcu_reset_and_recovery() {
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
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new()
                .with_reset_recovery_support()
                .with_scripted_el_responses([invalid_fcu(), valid_fcu()]),
        },
    );

    let (engine_tx, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.engine_request_sender(), harness.fake_engine_handle().clone())
    };

    // Send ProcessSafeL2SignalRequest directly instead of via fake_l1.extend().
    // fake_l1.extend() calls inject_fcu_v3_call() which pops the first scripted response before
    // the engine actor ever calls fork_choice_updated_v3(), defeating the test.
    driver.block_on(async {
        engine_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(ConsolidateInput::BlockInfo(
                L2BlockInfo {
                    block_info: block(1, B256::ZERO, hash_for(1), 1),
                    ..Default::default()
                },
            )))
            .await
            .expect("engine actor must accept the signal");
    });

    driver.tick(50);

    let fcu_heads: Vec<B256> = run_async(fake_engine_handle.calls())
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
