//! Two-node choreography harness and Tier-0 C1/C2 liveness tests.

use std::{collections::VecDeque, future::Future, sync::Arc};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use base_protocol::BlockInfo;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::{
    Driver, EngineClientCall, FakeEngineClientHandle, FakeL1, FakeSafeDBHandle, HarnessBuilder,
    NodeConfig, ProgressTimeout, ScriptedForkchoiceResponse,
};
use crate::{DerivationActorRequest, DerivationState, NodeMode};

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

fn hash_for(number: u64) -> B256 {
    B256::from([number as u8; 32])
}

fn block(number: u64, parent_hash: B256, hash: B256, timestamp: u64) -> BlockInfo {
    BlockInfo { number, hash, parent_hash, timestamp }
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

fn invalid_fcu() -> ScriptedForkchoiceResponse {
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus {
            status: PayloadStatusEnum::Invalid { validation_error: "test-invalid".into() },
            latest_valid_hash: Some(B256::ZERO),
        },
        payload_id: None,
    })
}

/// Timeout returned by [`TwoNodeHarness::run_until_progress`].
#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct TimeoutError(#[from] pub ProgressTimeout);

#[derive(Clone, Debug, Default)]
struct SharedGossipState {
    drop_next: usize,
    reorder_pattern: Vec<usize>,
    outbound: VecDeque<BlockInfo>,
}

/// Shared in-memory fake gossip transport handle for two-node tests.
#[derive(Clone, Debug, Default)]
pub struct FakeGossipTransportHandle {
    state: Arc<Mutex<SharedGossipState>>,
}

impl FakeGossipTransportHandle {
    /// Drops the next `count` gossiped payloads.
    pub async fn drop_next(&self, count: usize) {
        self.state.lock().await.drop_next = count;
    }

    /// Reorders outbound deliveries according to index pattern.
    pub async fn reorder(&self, pattern: Vec<usize>) {
        self.state.lock().await.reorder_pattern = pattern;
    }

    async fn enqueue(&self, payload: BlockInfo) {
        let mut state = self.state.lock().await;
        if state.drop_next > 0 {
            state.drop_next -= 1;
            return;
        }
        state.outbound.push_back(payload);
        if !state.reorder_pattern.is_empty() {
            let source = state.outbound.iter().copied().collect::<Vec<_>>();
            let mut reordered = VecDeque::new();
            for idx in &state.reorder_pattern {
                if let Some(item) = source.get(*idx).copied() {
                    reordered.push_back(item);
                }
            }
            state.outbound = reordered;
            state.reorder_pattern.clear();
        }
    }

    async fn drain(&self) -> Vec<BlockInfo> {
        let mut state = self.state.lock().await;
        state.outbound.drain(..).collect()
    }
}

/// Shared fake L1 handle that mirrors updates into both nodes.
#[derive(Clone, Debug)]
pub struct FakeL1Handle {
    sequencer_l1: FakeL1,
    validator_l1: FakeL1,
}

impl FakeL1Handle {
    fn new(sequencer_l1: FakeL1, validator_l1: FakeL1) -> Self {
        Self { sequencer_l1, validator_l1 }
    }

    /// Extends both nodes' fake L1 chains with the same block.
    pub async fn extend(&self, block: BlockInfo) {
        self.sequencer_l1.extend(block).await;
        self.validator_l1.extend(block).await;
    }

    /// Reorgs both nodes' fake L1 chains in lockstep.
    pub async fn reorg(&self, depth: usize, alt_blocks: Vec<BlockInfo>) {
        self.sequencer_l1.reorg(depth, alt_blocks.clone()).await;
        self.validator_l1.reorg(depth, alt_blocks).await;
    }

    /// Returns the sequencer-side L1 snapshot.
    pub async fn state(&self) -> super::FakeL1State {
        self.sequencer_l1.state().await
    }
}

/// Per-node handles used by [`TwoNodeHarness`].
#[derive(Clone, Debug)]
pub struct NodeHandles {
    /// Driver node id.
    pub node_id: usize,
    /// Fake engine call-log/script handle.
    pub fake_engine_client: FakeEngineClientHandle,
    /// Fake safedb handle.
    pub fake_safedb: FakeSafeDBHandle,
    /// Shared fake gossip transport control handle.
    pub fake_gossip: FakeGossipTransportHandle,
    derivation_request_tx: mpsc::Sender<DerivationActorRequest>,
}

impl NodeHandles {
    /// Returns the current derivation state.
    pub async fn derivation_state(&self) -> DerivationState {
        let (result_tx, result_rx) = oneshot::channel();
        self.derivation_request_tx
            .send(DerivationActorRequest::CurrentStateRequest(result_tx))
            .await
            .expect("failed to send CurrentStateRequest to derivation actor");
        result_rx.await.expect("failed to receive derivation state")
    }

    /// Returns the current derivation state if the actor is still reachable.
    pub async fn current_derivation_state(&self) -> Option<DerivationState> {
        let (result_tx, result_rx) = oneshot::channel();
        if self
            .derivation_request_tx
            .send(DerivationActorRequest::CurrentStateRequest(result_tx))
            .await
            .is_err()
        {
            return None;
        }
        result_rx.await.ok()
    }
}

/// Two-node deterministic harness with sequencer + validator.
#[derive(Debug)]
pub struct TwoNodeHarness<'a> {
    driver: &'a mut Driver,
    /// Sequencer node handles.
    pub sequencer: NodeHandles,
    /// Validator node handles.
    pub validator: NodeHandles,
    /// Shared fake L1 mirrored into both nodes.
    pub fake_l1: FakeL1Handle,
    /// Shared fake gossip transport for fault injection.
    pub fake_gossip_transport: FakeGossipTransportHandle,
}

impl<'a> TwoNodeHarness<'a> {
    /// Builds sequencer + validator nodes in the same runtime and wires shared fake handles.
    pub fn build(driver: &'a mut Driver) -> Self {
        let sequencer_id = driver.spawn_node(NodeMode::Sequencer, NodeConfig::default());
        let validator_id = driver.spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new()
                    .with_scripted_el_responses((0..64).map(|_| valid_fcu())),
            },
        );

        let (sequencer_l1, sequencer_engine, sequencer_safedb) = {
            let harness = driver.harness(sequencer_id);
            (
                harness.fake_l1().clone(),
                harness.fake_engine_handle().clone(),
                harness.fake_safedb_handle().clone(),
            )
        };
        let sequencer_derivation_tx = {
            let harness = driver.harness(sequencer_id);
            harness.derivation_request_sender()
        };
        let (validator_l1, validator_engine, validator_safedb) = {
            let harness = driver.harness(validator_id);
            (
                harness.fake_l1().clone(),
                harness.fake_engine_handle().clone(),
                harness.fake_safedb_handle().clone(),
            )
        };
        let validator_derivation_tx = {
            let harness = driver.harness(validator_id);
            harness.derivation_request_sender()
        };

        let fake_gossip_transport = FakeGossipTransportHandle::default();
        let fake_l1 = FakeL1Handle::new(sequencer_l1, validator_l1);

        let sequencer = NodeHandles {
            node_id: sequencer_id,
            fake_engine_client: sequencer_engine,
            fake_safedb: sequencer_safedb,
            fake_gossip: fake_gossip_transport.clone(),
            derivation_request_tx: sequencer_derivation_tx,
        };
        let validator = NodeHandles {
            node_id: validator_id,
            fake_engine_client: validator_engine,
            fake_safedb: validator_safedb,
            fake_gossip: fake_gossip_transport.clone(),
            derivation_request_tx: validator_derivation_tx,
        };

        Self { driver, sequencer, validator, fake_l1, fake_gossip_transport }
    }

    /// Scripts identical FCU responses on both nodes.
    pub fn script_both_fcu(&self, scripted: impl IntoIterator<Item = ScriptedForkchoiceResponse>) {
        let responses = scripted.into_iter().collect::<Vec<_>>();
        self.sequencer.fake_engine_client.push_scripted_fcu_v3_blocking(responses.clone());
        self.validator.fake_engine_client.push_scripted_fcu_v3_blocking(responses);
    }

    /// Scripts FCU responses only on validator.
    pub fn script_validator_fcu(
        &self,
        scripted: impl IntoIterator<Item = ScriptedForkchoiceResponse>,
    ) {
        self.validator.fake_engine_client.push_scripted_fcu_v3_blocking(scripted);
    }

    /// Scripts `new_payload_v3` responses only on validator.
    pub fn script_validator_new_payload_v3(
        &self,
        scripted: impl IntoIterator<Item = PayloadStatus>,
    ) {
        self.validator.fake_engine_client.push_scripted_new_payload_v3_blocking(scripted);
    }

    /// Extends shared L1 and relays corresponding synthetic gossip deliveries.
    pub fn extend_l1_with_gossip(&mut self, block: BlockInfo) {
        let fake_l1 = self.fake_l1.clone();
        let fake_gossip = self.fake_gossip_transport.clone();
        let validator_engine = self.validator.fake_engine_client.clone();

        run_async(async move {
            fake_l1.extend(block).await;
            fake_gossip.enqueue(block).await;
            for delivered in fake_gossip.drain().await {
                validator_engine
                    .inject_fcu_v3_call(ForkchoiceState {
                        head_block_hash: delivered.hash,
                        safe_block_hash: delivered.hash,
                        finalized_block_hash: delivered.hash,
                    })
                    .await;
            }
        });

        self.driver.tick(1);
    }

    /// Performs a shared L1 reorg and advances one tick.
    pub fn reorg_l1(&mut self, depth: usize, alt_blocks: Vec<BlockInfo>) {
        let fake_l1 = self.fake_l1.clone();
        run_async(async move {
            fake_l1.reorg(depth, alt_blocks).await;
        });
        self.driver.tick(1);
    }

    /// Runs deterministic ticks until validator safe head reaches `target_safe_number`.
    pub fn run_until_progress(
        &mut self,
        target_safe_number: u64,
        timeout_ticks: u64,
    ) -> Result<(), TimeoutError> {
        self.driver
            .await_progress(
                |snapshot| {
                    snapshot
                        .nodes
                        .get(self.validator.node_id)
                        .map(|validator| validator.safe_head_number >= target_safe_number)
                        .unwrap_or(false)
                },
                timeout_ticks,
            )
            .map_err(TimeoutError::from)
    }

    fn safe_head_number(&self, node_id: usize) -> u64 {
        self.driver
            .snapshot()
            .nodes
            .get(node_id)
            .map(|node| node.safe_head_number)
            .unwrap_or_default()
    }

    fn latest_fcu_state(handle: &FakeEngineClientHandle) -> Option<ForkchoiceState> {
        run_async(handle.calls()).into_iter().rev().find_map(|call| match call {
            EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c1_sequencer_output_reaches_validator() {
        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);
        harness.script_both_fcu((0..64).map(|_| valid_fcu()));

        for number in 1..=3 {
            harness.extend_l1_with_gossip(block(
                number,
                hash_for(number - 1),
                hash_for(number),
                number,
            ));
        }

        harness
            .run_until_progress(3, 50)
            .expect("validator safe head did not reach sequencer target within liveness gate");

        let sequencer_safe = harness.safe_head_number(harness.sequencer.node_id);
        let validator_safe = harness.safe_head_number(harness.validator.node_id);
        // Invariant C1: sequencer output reaches validator and final safe heads agree.
        assert_eq!(
            validator_safe, sequencer_safe,
            "validator safe head must match sequencer safe head"
        );

        let sequencer_fcu = TwoNodeHarness::latest_fcu_state(&harness.sequencer.fake_engine_client)
            .expect("sequencer should have at least one FCU call");
        let validator_fcu = TwoNodeHarness::latest_fcu_state(&harness.validator.fake_engine_client)
            .expect("validator should have at least one FCU call");
        // Invariant C1: validator unsafe head (FCU head hash) matches sequencer unsafe head.
        assert_eq!(validator_fcu.head_block_hash, sequencer_fcu.head_block_hash);
        // Invariant C1: validator safe head hash matches sequencer safe head hash.
        assert_eq!(validator_fcu.safe_block_hash, sequencer_fcu.safe_block_hash);
    }

    #[test]
    fn c1_dropped_gossip_validator_catches_up_via_l1_derivation() {
        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);
        harness.script_both_fcu((0..128).map(|_| valid_fcu()));
        run_async(harness.fake_gossip_transport.drop_next(3));

        for number in 1..=3 {
            harness.extend_l1_with_gossip(block(
                number,
                hash_for(number - 1),
                hash_for(number),
                number,
            ));
        }

        harness
            .run_until_progress(3, 100)
            .expect("validator did not catch up after dropped gossip within liveness gate");

        let sequencer_safe = harness.safe_head_number(harness.sequencer.node_id);
        let validator_safe = harness.safe_head_number(harness.validator.node_id);
        // Invariant C1: final sequencer/validator agreement still holds after gossip faults.
        assert_eq!(validator_safe, sequencer_safe);
        // Invariant L2: validator eventually progresses to available safe tip.
        assert!(validator_safe >= 3, "validator safe head must make forward progress");
    }

    #[test]
    fn c2_l1_reorg_both_roles_converge() {
        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);
        harness.script_both_fcu((0..192).map(|_| valid_fcu()));

        for number in 1..=5 {
            harness.extend_l1_with_gossip(block(
                number,
                hash_for(number - 1),
                hash_for(number),
                number,
            ));
        }

        harness
            .run_until_progress(5, 80)
            .expect("initial convergence to safe=5 failed before reorg");

        let alt_4 = block(4, hash_for(3), B256::from([44_u8; 32]), 44);
        let alt_5 = block(5, alt_4.hash, B256::from([55_u8; 32]), 55);
        harness.reorg_l1(2, vec![alt_4, alt_5]);

        harness
            .run_until_progress(5, 80)
            .expect("post-reorg convergence to new safe tip failed within liveness gate");

        let sequencer_safe = harness.safe_head_number(harness.sequencer.node_id);
        let validator_safe = harness.safe_head_number(harness.validator.node_id);
        // Invariant C2: sequencer and validator safe heads converge after reorg.
        assert_eq!(validator_safe, sequencer_safe);

        let l1_state = run_async(harness.fake_l1.state());
        // Invariant C2: both roles follow the new canonical L1 tip (alt_5).
        assert_eq!(l1_state.canonical.last().map(|tip| tip.hash), Some(alt_5.hash));

        let sequencer_calls = run_async(harness.sequencer.fake_engine_client.calls());
        let validator_calls = run_async(harness.validator.fake_engine_client.calls());
        let seq_replayed_height = sequencer_calls.iter().filter(|call| {
        matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } if fcs.head_block_hash == hash_for(4))
    });
        let val_replayed_height = validator_calls.iter().filter(|call| {
        matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } if fcs.head_block_hash == hash_for(4))
    });
        // Invariant S2.b: safe can regress on explicit signal and then re-advance during reorg handling.
        assert!(
            seq_replayed_height.count() >= 2,
            "sequencer should process reorged safe height at least twice"
        );
        // Invariant S2.b: validator mirrors the same regress-then-advance behavior under reorg signal.
        assert!(
            val_replayed_height.count() >= 2,
            "validator should process reorged safe height at least twice"
        );
    }

    #[test]
    fn c1_syncing_response_wedges_validator_liveness_gate_catches() {
        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);
        harness.script_both_fcu((0..128).map(|_| valid_fcu()));
        harness.script_validator_fcu([syncing_fcu(), valid_fcu(), valid_fcu(), valid_fcu()]);

        for number in 1..=3 {
            harness.extend_l1_with_gossip(block(
                number,
                hash_for(number - 1),
                hash_for(number),
                number,
            ));
        }

        // This is the integration-level symptom of #3809/#3803.
        // On pre-fix commit, this test would time out because validator's safe_head would wedge.
        let result = harness.run_until_progress(3, 50);
        // Invariant L2: validator must still make progress despite transient Syncing response.
        assert!(result.is_ok(), "validator should not wedge on first Syncing FCU response");
        // Invariant L3: accepted monotonic updates eventually become visible in safe_head.
        assert!(harness.safe_head_number(harness.validator.node_id) >= 3);
    }

    #[test]
    fn liveness_gate_catches_wedged_validator() {
        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);
        harness.script_validator_fcu((0..64).map(|_| syncing_fcu()));

        let result = harness.run_until_progress(1, 10);
        // Invariant L2 (negative proof): liveness gate must detect and fail a wedged validator.
        assert!(result.is_err(), "liveness gate should return TimeoutError for wedged validator");
    }

    #[test]
    fn c1_sequencer_invalid_block_reorg_validator_recovers() {
        // Property tested: L2 — validator never wedges. C1 — sequencer output ⇒ validator agreement.
        //
        // User-reported production bug: when the sequencer produces block B that is INVALID
        // (originally caused by a receipt-root mismatch between the sealed header and the EL's
        // execution result), the sequencer reorgs back and produces a new block B'. Validators
        // that gossip-received B and got INVALID from their own newPayload/FCU do NOT
        // automatically recover to B' — they remain wedged on the invalid tip.
        //
        // We reproduce the failure shape by scripting an INVALID FCU response on the first
        // engine call for both roles (any-INVALID-mechanism-equivalent per user directive), then
        // Valid for the retry/next block. Reception of INVALID on the first FCU is the
        // equivalent-shape signal for "this block cannot be committed"; the fix under test is
        // that the CL retries/reorgs to a valid replacement block and the validator converges.
        //
        // Outcome interpretation:
        // - PASS ⇒ validator recovers, confirming the CL handles sequencer-side INVALID + retry.
        // - FAIL (TimeoutError) ⇒ documents the reported bug: validator does not automatically
        //   recover from an invalid sequencer block reorg.

        let mut driver = Driver::new();
        let mut harness = TwoNodeHarness::build(&mut driver);

        // Sequencer's own engine view is fine (it produced the block from its perspective).
        // Validator's engine rejects the first gossiped block as INVALID on newPayload (models
        // a receipt-root or state-root mismatch surfacing only when the EL executes the payload),
        // and returns INVALID on the first FCU as well (the CL sees the block as unusable through
        // both entry points). Subsequent responses are Valid — modeling the sequencer's reorged
        // replacement block being executable. The question this test answers: after rejecting the
        // first gossiped block on both newPayload and FCU, does the validator recover to the
        // sequencer's tip via subsequent gossip / L1 derivation?
        harness.script_both_fcu(std::iter::repeat_with(valid_fcu).take(128));
        harness.script_validator_fcu(
            std::iter::once(invalid_fcu()).chain(std::iter::repeat_with(valid_fcu).take(127)),
        );
        let invalid_payload = PayloadStatus {
            status: PayloadStatusEnum::Invalid {
                validation_error: "test-invalid-newpayload".into(),
            },
            latest_valid_hash: Some(B256::ZERO),
        };
        let valid_payload = || PayloadStatus {
            status: PayloadStatusEnum::Valid,
            latest_valid_hash: Some(B256::ZERO),
        };
        harness.script_validator_new_payload_v3(
            std::iter::once(invalid_payload).chain(std::iter::repeat_with(valid_payload).take(127)),
        );

        for number in 1..=3 {
            harness.extend_l1_with_gossip(block(
                number,
                hash_for(number - 1),
                hash_for(number),
                number,
            ));
        }

        let result = harness.run_until_progress(3, 100);

        // Invariant L2: validator must not wedge after sequencer-driven invalid-block reorg.
        // Invariant C1: after the sequencer's reorged replacement, the validator's safe head
        // must eventually equal the sequencer's.
        assert!(
            result.is_ok(),
            "validator should recover after sequencer's invalid block is reorged (bug: it currently does not)",
        );
        let validator_safe = harness.safe_head_number(harness.validator.node_id);
        let sequencer_safe = harness.safe_head_number(harness.sequencer.node_id);
        assert_eq!(
            validator_safe, sequencer_safe,
            "validator safe head should match sequencer after reorg recovery",
        );

        // Diagnostic: confirm which EngineClient boundaries the validator's harness route hits.
        // If NewPayloadV3 count is 0, the scripted-INVALID newPayload seam was never reached and
        // this test does not actually exercise the receipt-root-mismatch failure mode.
        let validator_calls = run_async(harness.validator.fake_engine_client.calls());
        let fcu_v3_count = validator_calls
            .iter()
            .filter(|c| matches!(c, EngineClientCall::ForkChoiceUpdatedV3 { .. }))
            .count();
        let new_payload_v3_count = validator_calls
            .iter()
            .filter(|c| matches!(c, EngineClientCall::NewPayloadV3(_)))
            .count();
        eprintln!(
            "diagnostic: validator EngineClient calls: total={}, fcu_v3={}, new_payload_v3={}",
            validator_calls.len(),
            fcu_v3_count,
            new_payload_v3_count,
        );
    }
}
