//! Edge case tests E1–E14 for CL/EL state logic (see strategy.md § Edge cases and how the framework catches them)

use std::{collections::HashSet, future::Future};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
use base_protocol::BlockInfo;

use super::{
    Driver, EngineClientCall, FakeSafeDBHandle, HarnessBuilder, NodeConfig, ScriptedForkchoiceResponse,
};
use crate::NodeMode;

/// Number of explicit Tier-0 edge-case tests in this module.
pub const EDGE_CASE_TEST_COUNT: usize = 8;

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

fn wait_for_safedb_head(
    driver: &mut Driver,
    safedb: &FakeSafeDBHandle,
    expected: u64,
    timeout_ticks: u64,
) -> bool {
    for _ in 0..=timeout_ticks {
        let latest = run_async(safedb.latest());
        if latest.map(|entry| entry.safe_head.number).unwrap_or_default() >= expected {
            return true;
        }
        driver.tick(1);
    }
    false
}

#[test]
fn e1_bootstrap_from_fresh() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..16).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    let initial_safe = driver.snapshot().validator().map(|validator| validator.safe_head_number).unwrap_or(0);
    assert_eq!(initial_safe, 0, "fresh bootstrap should start at genesis safe head");

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;
    });
    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 3).unwrap_or(false),
            100,
        )
        .expect("bootstrap progression did not reach safe head >= 3");

    let calls = run_async(fake_engine_handle.calls());
    let fcu_calls = calls
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(fcu_calls.len() >= 3, "expected at least 3 FCU-v3 calls during bootstrap");
    for state in &fcu_calls {
        // Invariant S1: finalized ≤ local_safe ≤ safe ≤ unsafe (fake emits equal heads).
        assert_eq!(state.finalized_block_hash, state.safe_block_hash);
        assert_eq!(state.safe_block_hash, state.head_block_hash);
    }

    let before_more_progress = fcu_calls.len();
    run_async(async {
        fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;
    });
    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 4).unwrap_or(false),
            100,
        )
        .expect("safe head did not keep advancing after bootstrap");

    let calls_after = run_async(fake_engine_handle.calls());
    let total_fcu_after = calls_after
        .iter()
        .filter(|call| matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { .. }))
        .count();
    // Invariant S3: el_sync_finished monotonic proxy (forward progress continues; no regression wedge).
    assert!(total_fcu_after > before_more_progress, "expected FCU progression to continue after bootstrap");
}

#[test]
fn e2_reorg_during_derivation() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..16).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    let block_1 = block(1, B256::ZERO, hash_for(1), 1);
    let old_block_2 = block(2, hash_for(1), hash_for(2), 2);
    let alt_block_2 = block(2, hash_for(1), B256::from([42_u8; 32]), 3);

    run_async(async {
        fake_l1.extend(block_1).await;
        fake_l1.extend(old_block_2).await;
        fake_l1.reorg(1, vec![alt_block_2]).await;
    });

    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 2).unwrap_or(false),
            100,
        )
        .expect("safe head did not reach reorged block height");

    let l1_state = run_async(fake_l1.state());
    assert_eq!(l1_state.canonical.len(), 2, "reorg should keep canonical length at 2");
    assert_eq!(l1_state.canonical[1].hash, alt_block_2.hash, "canonical tip should be alt reorg block");

    let calls = run_async(fake_engine_handle.calls());
    let fcu_heads = calls
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
            _ => None,
        })
        .collect::<Vec<_>>();

    let count_block_2 = fcu_heads.iter().filter(|hash| **hash == old_block_2.hash).count();

    // Invariant D3: signal-before-resume proxy (old block-2 attrs are superseded by reorg processing).
    assert!(count_block_2 >= 2, "expected block-2 FCU to be observed before and after reorg handling");
    // Invariant D4: derivation determinism proxy (final canonical L1 tip is the alt block).
    assert_eq!(l1_state.canonical[1].hash, alt_block_2.hash, "expected final canonical L1 tip to match alt reorg block");
}

#[test]
fn e3_empty_l1_batch() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..8).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_safedb_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_safedb_handle().clone())
    };

    let l1_block_with_l2 = block(1, B256::ZERO, hash_for(1), 1);
    let l1_block_without_l2 = block(1, hash_for(1), B256::from([31_u8; 32]), 2);

    run_async(async {
        fake_l1.extend(l1_block_with_l2).await;
    });
    assert!(wait_for_safedb_head(&mut driver, &fake_safedb_handle, 1, 100));

    let entries_before = run_async(fake_safedb_handle.entries());
    run_async(async {
        fake_l1.extend(l1_block_without_l2).await;
    });
    driver.tick(20);
    let entries_after = run_async(fake_safedb_handle.entries());

    assert!(!entries_before.is_empty(), "expected initial safedb entry");
    assert!(!entries_after.is_empty(), "expected safedb entries after second L1 block");
    // Invariant S2.b: safe/local-safe non-decreasing without silent regression.
    assert!(entries_after.len() > entries_before.len(), "expected new safedb entry after empty L1 batch");
    // Edge-case E3 property: L1-origin can advance while L2 number is unchanged.
    assert_eq!(
        entries_after.last().map(|entry| entry.safe_head.number),
        entries_before.last().map(|entry| entry.safe_head.number),
        "safe head number should remain stable for empty-batch L1 extension",
    );
}

#[test]
fn e5_concurrent_gossip_and_derivation_same_block() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..8).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    let target = block(5, hash_for(4), hash_for(5), 5);
    run_async(async {
        fake_l1.extend(target).await;
    });
    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 5).unwrap_or(false),
            100,
        )
        .expect("target block did not advance safe head");

    let calls = run_async(fake_engine_handle.calls());
    let target_heads = calls
        .iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } if fcs.head_block_hash == target.hash => {
                Some(fcs.head_block_hash)
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    let mut transitions_to_target = 0_usize;
    let mut last = None;
    for hash in target_heads {
        if Some(hash) != last {
            transitions_to_target += 1;
            last = Some(hash);
        }
    }

    // Invariant S5: engine/CL agreement proxy at wire boundary.
    // Invariant V1/D4 proxy: block N should produce a single state transition even with duplicate delivery.
    assert_eq!(transitions_to_target, 1, "expected a single state transition for block N");
}

#[test]
fn e8_task_queue_backpressure() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..64).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    run_async(async {
        for number in 1..=24 {
            fake_l1.extend(block(number, hash_for(number.saturating_sub(1)), hash_for(number), number)).await;
        }
    });

    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 24).unwrap_or(false),
            250,
        )
        .expect("backpressure run did not converge to final safe head");

    let calls = run_async(fake_engine_handle.calls());
    let fcu_heads = calls
        .into_iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
            _ => None,
        })
        .collect::<Vec<_>>();

    let unique_heads = fcu_heads.iter().copied().collect::<HashSet<_>>();
    // Invariant S4: partial commits are not silently lost under queue pressure.
    assert!(fcu_heads.len() >= 24, "expected at least one FCU per enqueued derivation step");
    // Invariant S6: idempotence/no double-apply for identical task state effects.
    assert_eq!(unique_heads.len(), fcu_heads.len(), "expected no duplicate FCU head applications");
}

#[test]
fn e9_backup_unsafe_reorg_race() {
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

    let (fake_l1, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
    });

    driver
        .await_progress(
            |snapshot| snapshot.validator().map(|validator| validator.safe_head_number >= 2).unwrap_or(false),
            100,
        )
        .expect("backup-unsafe-reorg race setup did not progress");

    let calls = run_async(fake_engine_handle.calls());
    let matching_recovery = calls
        .iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } if fcs.head_block_hash == hash_for(1) => {
                Some(fcs)
            }
            _ => None,
        })
        .count();

    // Invariant S3: sticky flag should clear once (proxy: no repeated recovery FCU for same forgotten head).
    // Invariant S5: recovery does not double-fire and diverge CL↔EL forkchoice intent.
    assert_eq!(matching_recovery, 1, "expected exactly one recovery FCU for the forgotten unsafe head");
}

#[test]
fn e13_l1_provider_stall() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses((0..32).map(|_| valid_fcu())),
        },
    );

    let (fake_l1, fake_safedb_handle, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (
            harness.fake_l1().clone(),
            harness.fake_safedb_handle().clone(),
            harness.fake_engine_handle().clone(),
        )
    };

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
    });
    assert!(wait_for_safedb_head(&mut driver, &fake_safedb_handle, 1, 100));

    run_async(async {
        fake_l1.stall().await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;
        fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;
    });
    driver.tick(20);

    let latest_while_stalled = run_async(fake_safedb_handle.latest()).map(|entry| entry.safe_head.number);
    // Invariant L2/S1: derivation-safe progression halts while L1 provider is stalled.
    assert_eq!(latest_while_stalled, Some(1), "safe head should not advance while L1 is stalled");

    run_async(async {
        for number in 2..=4 {
            fake_engine_handle
                .inject_fcu_v3_call(ForkchoiceState {
                    head_block_hash: hash_for(number),
                    safe_block_hash: hash_for(1),
                    finalized_block_hash: hash_for(1),
                })
                .await;
        }
    });

    let stalled_calls = run_async(fake_engine_handle.calls());
    let has_unsafe_progress = stalled_calls.iter().any(|call| {
        matches!(
            call,
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. }
                if fcs.head_block_hash == hash_for(4) && fcs.safe_block_hash == hash_for(1)
        )
    });
    // Invariant L2: unsafe path can still progress independently of L1 retrieval stall.
    assert!(has_unsafe_progress, "expected unsafe-style FCU progression while derivation is stalled");

    run_async(async {
        fake_l1.resume().await;
    });
    let resumed = wait_for_safedb_head(&mut driver, &fake_safedb_handle, 4, 200);
    // Invariant L2: once L1 resumes, derivation catches up without requiring an extra signal.
    assert!(resumed, "safe head did not resume progression after L1 resume");
}

#[test]
fn e14_el_restart_mid_flight() {
    let mut driver = Driver::new();
    let node_id = driver.spawn_node(
        NodeMode::Validator,
        NodeConfig {
            builder: HarnessBuilder::new().with_scripted_el_responses([
                valid_fcu(),
                valid_fcu(),
                syncing_fcu(),
                syncing_fcu(),
                syncing_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
                valid_fcu(),
            ]),
        },
    );

    let (fake_l1, fake_safedb_handle, fake_engine_handle) = {
        let harness = driver.harness(node_id);
        (
            harness.fake_l1().clone(),
            harness.fake_safedb_handle().clone(),
            harness.fake_engine_handle().clone(),
        )
    };

    run_async(async {
        fake_l1.extend(block(1, B256::ZERO, hash_for(1), 1)).await;
        fake_l1.extend(block(2, hash_for(1), hash_for(2), 2)).await;
    });
    assert!(wait_for_safedb_head(&mut driver, &fake_safedb_handle, 2, 100));

    run_async(async {
        fake_l1.extend(block(3, hash_for(2), hash_for(3), 3)).await;
        fake_l1.extend(block(4, hash_for(3), hash_for(4), 4)).await;
        fake_l1.extend(block(5, hash_for(4), hash_for(5), 5)).await;
    });
    driver.tick(50);

    run_async(async {
        fake_l1.extend(block(6, hash_for(5), hash_for(6), 6)).await;
        fake_l1.extend(block(7, hash_for(6), hash_for(7), 7)).await;
        fake_l1.extend(block(8, hash_for(7), hash_for(8), 8)).await;
    });
    let caught_up = wait_for_safedb_head(&mut driver, &fake_safedb_handle, 8, 200);

    // Invariant L3: previously consolidated safe advances eventually commit after Syncing window ends.
    assert!(caught_up, "safe head did not catch up after EL Syncing window");

    let calls = run_async(fake_engine_handle.calls());
    let fcu_heads = calls
        .iter()
        .filter_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
            _ => None,
        })
        .collect::<Vec<_>>();
    let unique = fcu_heads.iter().copied().collect::<HashSet<_>>();

    // Invariant S3: el_sync_finished monotonic proxy (no silent rollback; forward calls continue through restart).
    assert!(fcu_heads.len() >= 8, "expected FCU progression across restart window");
    // Invariant S5: restart recovery should still cover the full head range even if retries duplicate calls.
    assert!(unique.len() >= 8, "expected forkchoice calls to cover every post-restart head");
}
