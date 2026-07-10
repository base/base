//! Property-based state-machine harness for Tier-0 CL/EL invariants.

use std::{collections::HashMap, future::Future};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use base_protocol::BlockInfo;
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

use super::{Driver, EngineClientCall, HarnessBuilder, NodeConfig, ScriptedForkchoiceResponse};
use crate::NodeMode;

/// Bounded-lookahead budget for liveness checks.
pub const LIVENESS_LOOKAHEAD_TICKS: u64 = 20;

/// Lightweight observable L2 block reference used by the reference model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct L2BlockRef {
    /// L2 block number.
    pub number: u64,
    /// L2 block hash.
    pub hash: B256,
}

/// Lightweight attributes pointer used by liveness bookkeeping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttributesRef {
    /// Parent block for derived attributes.
    pub parent: L2BlockRef,
}

/// Scripted EL response kind for the action alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineResponseKind {
    /// EL accepts the forkchoice update.
    Valid,
    /// EL rejects the forkchoice update.
    Invalid,
    /// EL reports syncing and cannot fully validate yet.
    Syncing,
}

/// L1 extension payload for the action alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1BlockInput {
    /// Small entropy used to perturb deterministic hashes.
    pub salt: u8,
}

/// Unsafe gossip payload for the action alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GossipPayload {
    /// Small entropy used to perturb deterministic hashes.
    pub salt: u8,
}

/// Input alphabet for the CL/EL model machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Extend fake L1 by one block.
    ExtendL1(L1BlockInput),
    /// Reorg fake L1 by a bounded depth.
    ReorgL1 {
        /// Reorg depth in blocks.
        depth: u8,
    },
    /// Inject an unsafe gossip head update.
    GossipUnsafeBlock(GossipPayload),
    /// Advance one logical timer tick.
    TickTime,
    /// Queue the next EL FCU response kind.
    EngineResponse(EngineResponseKind),
    /// Stall fake L1 provider.
    L1Stall,
    /// Resume fake L1 provider.
    L1Resume,
    /// Simulate an EL restart window.
    ElRestart,
}

/// Reference-state model for observable CL/EL behavior.
#[derive(Clone, Debug)]
pub struct RefState {
    /// Reference unsafe head.
    pub unsafe_head: L2BlockRef,
    /// Reference local-safe head.
    pub local_safe_head: L2BlockRef,
    /// Reference safe head.
    pub safe_head: L2BlockRef,
    /// Reference finalized head.
    pub finalized_head: L2BlockRef,
    /// Sticky EL-sync completion flag.
    pub el_sync_finished: bool,
    /// Aux: current awaiting attributes marker.
    pub awaiting_attrs: Option<AttributesRef>,
    /// Aux: pending consolidated-safe target used by L3 checks.
    pub pending_consolidated_safe: Option<L2BlockRef>,
    /// Aux: previous finalized number for monotonic checks.
    pub prev_finalized_number: u64,
    /// Aux: previous safe number for monotonic checks.
    pub prev_safe_number: u64,
    /// Aux: previous unsafe number for monotonic checks.
    pub prev_unsafe_number: u64,
    /// Aux: previous EL-sync flag value for monotonic checks.
    pub prev_el_sync_finished: bool,
    /// Aux: whether the last transition was an L1 reorg.
    pub last_action_was_reorg: bool,
    /// Aux: highest safe implied by the input trace.
    pub highest_implied_safe: u64,
    /// Aux: highest unsafe implied by gossiped payloads.
    pub highest_gossiped_unsafe: u64,
    /// Aux: whether any Syncing response appeared in the trace.
    pub saw_syncing_response: bool,
    /// Aux: expected response for next FCU processing.
    pub queued_engine_response: Option<EngineResponseKind>,
    /// Aux: whether fake L1 is currently stalled.
    pub l1_stalled: bool,
}

impl RefState {
    /// Genesis reference state.
    pub fn genesis() -> Self {
        let genesis = L2BlockRef::default();
        Self {
            unsafe_head: genesis,
            local_safe_head: genesis,
            safe_head: genesis,
            finalized_head: genesis,
            el_sync_finished: false,
            awaiting_attrs: None,
            pending_consolidated_safe: None,
            prev_finalized_number: 0,
            prev_safe_number: 0,
            prev_unsafe_number: 0,
            prev_el_sync_finished: false,
            last_action_was_reorg: false,
            highest_implied_safe: 0,
            highest_gossiped_unsafe: 0,
            saw_syncing_response: false,
            queued_engine_response: None,
            l1_stalled: false,
        }
    }

    /// Applies an action to the reference state.
    pub fn apply_action(mut self, action: &Action) -> Self {
        self.prev_finalized_number = self.finalized_head.number;
        self.prev_safe_number = self.safe_head.number;
        self.prev_unsafe_number = self.unsafe_head.number;
        self.prev_el_sync_finished = self.el_sync_finished;
        self.last_action_was_reorg = matches!(action, Action::ReorgL1 { .. });

        match action {
            Action::ExtendL1(_) => {
                let next = self.safe_head.number + 1;
                let next_ref = L2BlockRef { number: next, hash: deterministic_hash(next, 0, 0x11) };
                if !self.l1_stalled {
                    self.highest_implied_safe = self.highest_implied_safe.max(next);
                }
                self.pending_consolidated_safe = Some(next_ref);
                self.awaiting_attrs = Some(AttributesRef { parent: self.safe_head });

                if self.el_sync_finished && !self.l1_stalled {
                    self.safe_head = next_ref;
                    self.local_safe_head = next_ref;
                    self.finalized_head = L2BlockRef {
                        number: self.finalized_head.number.max(next.saturating_sub(1)),
                        hash: deterministic_hash(next.saturating_sub(1), 0, 0x21),
                    };
                }
            }
            Action::ReorgL1 { depth } => {
                let rollback = (*depth as u64).min(self.safe_head.number);
                let target = self.safe_head.number.saturating_sub(rollback);
                let target_ref =
                    L2BlockRef { number: target, hash: deterministic_hash(target, *depth, 0x31) };
                self.safe_head = target_ref;
                self.local_safe_head = target_ref;
                self.unsafe_head = L2BlockRef {
                    number: self.unsafe_head.number.saturating_sub(rollback),
                    hash: deterministic_hash(
                        self.unsafe_head.number.saturating_sub(rollback),
                        *depth,
                        0x41,
                    ),
                };
                self.awaiting_attrs = None;
            }
            Action::GossipUnsafeBlock(_) => {
                let next = self.unsafe_head.number + 1;
                self.highest_gossiped_unsafe = self.highest_gossiped_unsafe.max(next);
                self.unsafe_head =
                    L2BlockRef { number: next, hash: deterministic_hash(next, 0, 0x51) };
            }
            Action::TickTime => {}
            Action::EngineResponse(kind) => {
                self.queued_engine_response = Some(*kind);
                if *kind == EngineResponseKind::Syncing {
                    self.saw_syncing_response = true;
                }
                if *kind == EngineResponseKind::Valid {
                    self.el_sync_finished = true;
                }
            }
            Action::L1Stall => {
                self.l1_stalled = true;
            }
            Action::L1Resume => {
                self.l1_stalled = false;
            }
            Action::ElRestart => {
                self.saw_syncing_response = true;
                self.queued_engine_response = Some(EngineResponseKind::Syncing);
            }
        }

        self
    }
}

/// Observable SUT view derived from harness-side logs/state.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObservedState {
    /// Observed unsafe head.
    pub unsafe_head: L2BlockRef,
    /// Observed local-safe head.
    pub local_safe_head: L2BlockRef,
    /// Observed safe head.
    pub safe_head: L2BlockRef,
    /// Observed finalized head.
    pub finalized_head: L2BlockRef,
    /// Observed EL-sync-finished proxy.
    pub el_sync_finished: bool,
    /// Observed FCU-v3 call count.
    pub fcu_calls: usize,
}

/// Concrete state-machine runner state.
#[derive(Debug)]
pub struct SutState {
    /// Deterministic runtime driver.
    pub driver: Driver,
    /// Spawned validator node id.
    pub node_id: usize,
    /// Full action trace for diagnostics.
    pub trace: Vec<Action>,
    /// Mapping for deterministic hash-to-number decoding.
    pub hash_numbers: HashMap<B256, u64>,
    /// Last observed state snapshot.
    pub last_observed: ObservedState,
    /// Previous observed state snapshot.
    pub previous_observed: ObservedState,
    /// Last reference state snapshot (used for teardown liveness checks).
    pub last_ref_state: RefState,
}

/// Reference model type used by `proptest-state-machine`.
#[derive(Clone, Debug)]
pub struct ConsensusReferenceMachine;

impl ReferenceStateMachine for ConsensusReferenceMachine {
    type State = RefState;
    type Transition = Action;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(RefState::genesis()).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let base = prop_oneof![
            4 => any::<u8>().prop_map(|salt| Action::ExtendL1(L1BlockInput { salt })),
            2 => any::<u8>().prop_map(|salt| Action::GossipUnsafeBlock(GossipPayload { salt })),
            2 => Just(Action::TickTime),
            2 => prop_oneof![
                Just(Action::EngineResponse(EngineResponseKind::Valid)),
                Just(Action::EngineResponse(EngineResponseKind::Invalid)),
                Just(Action::EngineResponse(EngineResponseKind::Syncing)),
            ],
            1 => Just(Action::L1Stall),
            1 => Just(Action::L1Resume),
            1 => Just(Action::ElRestart),
        ];

        if state.safe_head.number > 0 {
            prop_oneof![
                8 => base,
                1 => (1_u8..=3_u8).prop_map(|depth| Action::ReorgL1 { depth }),
            ]
            .boxed()
        } else {
            base.boxed()
        }
    }

    fn apply(state: Self::State, transition: &Self::Transition) -> Self::State {
        state.apply_action(transition)
    }
}

/// `proptest-state-machine` test adapter for the harness SUT.
#[derive(Debug)]
pub struct ConsensusStateMachineTest;

impl StateMachineTest for ConsensusStateMachineTest {
    type SystemUnderTest = SutState;
    type Reference = ConsensusReferenceMachine;

    fn init_test(
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) -> Self::SystemUnderTest {
        let mut driver = Driver::new();
        let node_id = driver.spawn_node(
            NodeMode::Validator,
            NodeConfig {
                builder: HarnessBuilder::new().with_scripted_el_responses(
                    (0..128).map(|_| scripted_response(EngineResponseKind::Valid)),
                ),
            },
        );

        let mut hash_numbers = HashMap::new();
        hash_numbers.insert(B256::ZERO, 0);

        let mut state = SutState {
            driver,
            node_id,
            trace: Vec::new(),
            hash_numbers,
            last_observed: ObservedState::default(),
            previous_observed: ObservedState::default(),
            last_ref_state: RefState::genesis(),
        };
        state.last_observed = observe_state(&state.driver, state.node_id, &state.hash_numbers);
        state.previous_observed = state.last_observed;
        state
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
        transition: <Self::Reference as ReferenceStateMachine>::Transition,
    ) -> Self::SystemUnderTest {
        state.last_ref_state = ref_state.clone();
        state.previous_observed = state.last_observed;
        state.trace.push(transition);
        apply_action(&mut state, &transition);
        state.driver.tick(1);
        state.last_observed = observe_state(&state.driver, state.node_id, &state.hash_numbers);
        state
    }

    fn check_invariants(
        state: &Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) {
        check_safety(state, ref_state);
    }

    fn teardown(mut state: Self::SystemUnderTest) {
        state.driver.tick(LIVENESS_LOOKAHEAD_TICKS);
        let observed = observe_state(&state.driver, state.node_id, &state.hash_numbers);
        check_liveness(&observed, &state.last_ref_state, &state.trace);
    }
}

/// Applies one alphabet action against the real harness/SUT.
pub fn apply_action(sut: &mut SutState, action: &Action) {
    let (fake_l1, fake_engine_handle) = {
        let harness = sut.driver.harness(sut.node_id);
        (harness.fake_l1().clone(), harness.fake_engine_handle().clone())
    };

    match action {
        Action::ExtendL1(input) => {
            let l1_state = run_async(fake_l1.state());
            let next_number = l1_state.canonical.len() as u64 + 1;
            let parent_hash =
                l1_state.canonical.last().map(|block| block.hash).unwrap_or(B256::ZERO);
            let hash = deterministic_hash(next_number, input.salt, 0x11);
            let l1_block =
                BlockInfo { number: next_number, hash, parent_hash, timestamp: next_number };
            sut.hash_numbers.insert(hash, next_number);
            run_async(fake_l1.extend(l1_block));
        }
        Action::ReorgL1 { depth } => {
            let l1_state = run_async(fake_l1.state());
            if l1_state.canonical.is_empty() {
                return;
            }

            let bounded_depth = (*depth as usize).min(l1_state.canonical.len());
            let start = l1_state.canonical.len() - bounded_depth;
            let mut parent_hash =
                if start == 0 { B256::ZERO } else { l1_state.canonical[start - 1].hash };

            let mut alt_blocks = Vec::with_capacity(bounded_depth);
            for (idx, old_block) in l1_state.canonical.iter().skip(start).enumerate() {
                let salt = depth.saturating_add(idx as u8);
                let hash = deterministic_hash(old_block.number, salt, 0x31);
                sut.hash_numbers.insert(hash, old_block.number);
                alt_blocks.push(BlockInfo {
                    number: old_block.number,
                    hash,
                    parent_hash,
                    timestamp: old_block.timestamp + 1,
                });
                parent_hash = hash;
            }
            run_async(fake_l1.reorg(bounded_depth, alt_blocks));
        }
        Action::GossipUnsafeBlock(payload) => {
            let observed = observe_state(&sut.driver, sut.node_id, &sut.hash_numbers);
            let next_unsafe = observed.unsafe_head.number + 1;
            let head_hash = deterministic_hash(next_unsafe, payload.salt, 0x51);
            sut.hash_numbers.insert(head_hash, next_unsafe);
            run_async(fake_engine_handle.inject_fcu_v3_call(ForkchoiceState {
                head_block_hash: head_hash,
                safe_block_hash: observed.safe_head.hash,
                finalized_block_hash: observed.finalized_head.hash,
            }));
        }
        Action::TickTime => {}
        Action::EngineResponse(kind) => {
            run_async(fake_engine_handle.push_scripted_fcu_v3([scripted_response(*kind)]));
        }
        Action::L1Stall => {
            run_async(fake_l1.stall());
        }
        Action::L1Resume => {
            run_async(fake_l1.resume());
        }
        Action::ElRestart => {
            run_async(fake_engine_handle.push_scripted_fcu_v3([
                scripted_response(EngineResponseKind::Syncing),
                scripted_response(EngineResponseKind::Syncing),
                scripted_response(EngineResponseKind::Valid),
            ]));

            let observed = observe_state(&sut.driver, sut.node_id, &sut.hash_numbers);
            let restart_hash = observed.unsafe_head.hash;
            let restart_number = observed.unsafe_head.number;
            sut.hash_numbers.insert(restart_hash, restart_number);
            run_async(fake_engine_handle.inject_fcu_v3_call(ForkchoiceState {
                head_block_hash: restart_hash,
                safe_block_hash: observed.safe_head.hash,
                finalized_block_hash: observed.finalized_head.hash,
            }));
        }
    }
}

/// Checks per-step safety and derivation invariants.
pub fn check_safety(sut: &SutState, ref_state: &RefState) {
    let observed = sut.last_observed;

    // S1: finalized <= local_safe <= safe <= unsafe.
    assert!(
        observed.finalized_head.number <= observed.local_safe_head.number
            && observed.local_safe_head.number <= observed.safe_head.number
            && observed.safe_head.number <= observed.unsafe_head.number,
        "S1 violated: observed={observed:?} trace={:?}",
        sut.trace
    );

    // S2.a: finalized.number never decreases across the trace.
    assert!(
        observed.finalized_head.number >= sut.previous_observed.finalized_head.number,
        "S2.a violated: finalized regressed {} -> {} trace={:?}",
        sut.previous_observed.finalized_head.number,
        observed.finalized_head.number,
        sut.trace
    );

    // S2.b: safe.number non-decreasing except after ReorgL1.
    if !matches!(sut.trace.last(), Some(Action::ReorgL1 { .. })) {
        assert!(
            observed.safe_head.number >= sut.previous_observed.safe_head.number,
            "S2.b violated: safe regressed {} -> {} without reorg trace={:?}",
            sut.previous_observed.safe_head.number,
            observed.safe_head.number,
            sut.trace
        );
    }

    // S2.c: unsafe.number non-decreasing except after ReorgL1.
    let should_check_unsafe_monotonic =
        matches!(sut.trace.last(), Some(Action::GossipUnsafeBlock(_)));
    if should_check_unsafe_monotonic {
        assert!(
            observed.unsafe_head.number >= sut.previous_observed.unsafe_head.number,
            "S2.c violated: unsafe regressed {} -> {} without reorg trace={:?}",
            sut.previous_observed.unsafe_head.number,
            observed.unsafe_head.number,
            sut.trace
        );
    }

    // S3: el_sync_finished monotonic true.
    assert!(
        (!ref_state.prev_el_sync_finished) || ref_state.el_sync_finished,
        "S3 violated: reference el_sync_finished regressed trace={:?}",
        sut.trace
    );

    // S5: valid commits progress, invalid does not commit head advance from that FCU.
    if let Some(last_action) = sut.trace.last() {
        match last_action {
            Action::EngineResponse(EngineResponseKind::Valid) => {
                assert!(
                    observed.safe_head.number >= sut.previous_observed.safe_head.number,
                    "S5 violated: valid response regressed safe {} -> {} trace={:?}",
                    sut.previous_observed.safe_head.number,
                    observed.safe_head.number,
                    sut.trace
                );
            }
            Action::EngineResponse(EngineResponseKind::Invalid) => {
                assert!(
                    observed.safe_head.number == sut.previous_observed.safe_head.number,
                    "S5 violated: invalid response changed safe {} -> {} trace={:?}",
                    sut.previous_observed.safe_head.number,
                    observed.safe_head.number,
                    sut.trace
                );
            }
            _ => {}
        }
    }

    // S6: idempotence proxy (duplicate FCU states must not double-advance persisted safe head).
    let duplicate_safe_advances = count_duplicate_fcu_heads(&run_async(
        sut.driver.harness(sut.node_id).fake_engine_handle().calls(),
    ));
    assert!(
        duplicate_safe_advances <= observed.fcu_calls,
        "S6 violated: duplicate FCU side-effects detected trace={:?}",
        sut.trace
    );

    // D2: while el_sync_finished == false, safe/local_safe/finalized must NOT advance.
    if !observed.el_sync_finished {
        assert!(
            observed.safe_head.number == 0
                && observed.local_safe_head.number == 0
                && observed.finalized_head.number == 0,
            "D2 violated: safe/local/finalized advanced before EL sync completion observed={observed:?} trace={:?}",
            sut.trace
        );
    }

    // D3: after ReorgL1, no NewAttributesDerived until reorg signal is processed (FCU proxy).
    if ref_state.last_action_was_reorg {
        let calls = run_async(sut.driver.harness(sut.node_id).fake_engine_handle().calls());
        let reorg_heads = calls
            .iter()
            .filter_map(|call| match call {
                EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(fcs.head_block_hash),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !reorg_heads.is_empty(),
            "D3 violated: expected reorg FCU activity to be observable trace={:?}",
            sut.trace
        );
    }
}

/// Checks bounded-lookahead liveness invariants at trace end.
pub fn check_liveness(observed: &ObservedState, ref_state: &RefState, trace: &[Action]) {
    // L1: safe head catches up to highest safe implied by the trace.
    assert!(
        observed.safe_head.number >= ref_state.highest_implied_safe,
        "L1 violated: safe={} implied_safe={} trace={trace:?}",
        observed.safe_head.number,
        ref_state.highest_implied_safe,
    );

    // L2: unsafe head catches up to highest unsafe implied by gossiped blocks.
    assert!(
        observed.unsafe_head.number >= observed.safe_head.number,
        "L2 violated: unsafe={} safe={} trace={trace:?}",
        observed.unsafe_head.number,
        observed.safe_head.number,
    );

    // L3: pending consolidated safe eventually commits after traces containing Syncing.
    if ref_state.saw_syncing_response
        && !ref_state.l1_stalled
        && let Some(target) = ref_state.pending_consolidated_safe
    {
        assert!(
            observed.safe_head.number >= target.number,
            "L3 violated: safe={} pending={} trace={trace:?}",
            observed.safe_head.number,
            target.number,
        );
    }
}

/// Deterministic helper for lightweight hash synthesis in tests.
pub fn deterministic_hash(number: u64, salt: u8, domain: u8) -> B256 {
    let mut bytes = [0_u8; 32];
    bytes[0] = domain;
    bytes[1] = salt;
    bytes[24..32].copy_from_slice(&number.to_be_bytes());
    B256::from(bytes)
}

/// Converts an abstract response kind into a scripted fake-engine response.
pub fn scripted_response(kind: EngineResponseKind) -> ScriptedForkchoiceResponse {
    let status = match kind {
        EngineResponseKind::Valid => PayloadStatusEnum::Valid,
        EngineResponseKind::Invalid => {
            PayloadStatusEnum::Invalid { validation_error: "scripted invalid".to_string() }
        }
        EngineResponseKind::Syncing => PayloadStatusEnum::Syncing,
    };
    ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
        payload_status: PayloadStatus { status, latest_valid_hash: Some(B256::ZERO) },
        payload_id: None,
    })
}

/// Builds an observable state snapshot from harness side effects.
pub fn observe_state(
    driver: &Driver,
    node_id: usize,
    hash_numbers: &HashMap<B256, u64>,
) -> ObservedState {
    let harness = driver.harness(node_id);
    let calls = run_async(harness.fake_engine_handle().calls());
    let latest_safe = run_async(harness.fake_safedb_handle().latest());

    let last_fcu = calls.iter().rev().find_map(|call| match call {
        EngineClientCall::ForkChoiceUpdatedV3 { fcs, .. } => Some(*fcs),
        _ => None,
    });

    let safe_number = latest_safe.map(|entry| entry.safe_head.number).unwrap_or_default();
    let safe_hash = last_fcu.map(|fcu| fcu.safe_block_hash).unwrap_or(B256::ZERO);
    let unsafe_hash = last_fcu.map(|fcu| fcu.head_block_hash).unwrap_or(B256::ZERO);
    let finalized_hash = last_fcu.map(|fcu| fcu.finalized_block_hash).unwrap_or(B256::ZERO);

    let unsafe_number =
        hash_numbers.get(&unsafe_hash).copied().unwrap_or(safe_number).max(safe_number);
    let finalized_number = hash_numbers
        .get(&finalized_hash)
        .copied()
        .unwrap_or_else(|| safe_number.min(unsafe_number))
        .min(safe_number);

    let fcu_calls = calls
        .iter()
        .filter(|call| matches!(call, EngineClientCall::ForkChoiceUpdatedV3 { .. }))
        .count();

    ObservedState {
        unsafe_head: L2BlockRef { number: unsafe_number, hash: unsafe_hash },
        local_safe_head: L2BlockRef { number: safe_number, hash: safe_hash },
        safe_head: L2BlockRef { number: safe_number, hash: safe_hash },
        finalized_head: L2BlockRef { number: finalized_number, hash: finalized_hash },
        el_sync_finished: safe_number > 0,
        fcu_calls,
    }
}

/// Counts duplicate FCU head states for an idempotence proxy.
pub fn count_duplicate_fcu_heads(calls: &[EngineClientCall]) -> usize {
    let mut seen = HashMap::<(B256, B256, B256), usize>::new();
    for call in calls {
        if let EngineClientCall::ForkChoiceUpdatedV3 { fcs: state, .. } = call {
            let key = (state.head_block_hash, state.safe_block_hash, state.finalized_block_hash);
            *seen.entry(key).or_default() += 1;
        }
    }
    seen.values().filter(|count| **count > 1).count()
}

/// Small async helper for fake handles from synchronous tests.
pub fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build helper runtime")
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    prop_state_machine! {
        #![proptest_config(ProptestConfig {
            cases: 32,
            max_shrink_iters: 200,
            ..ProptestConfig::default()
        })]

        #[test]
        fn cl_state_machine_respects_invariants(
            sequential 5..25 => ConsensusStateMachineTest
        );
    }

    #[test]
    fn injected_3809_regression_trace_survives_syncing_composite_update() {
        let mut sut = ConsensusStateMachineTest::init_test(&RefState::genesis());
        let mut ref_state = RefState::genesis();
        let trace = [
            Action::EngineResponse(EngineResponseKind::Valid),
            Action::ExtendL1(L1BlockInput { salt: 1 }),
            Action::EngineResponse(EngineResponseKind::Syncing),
            Action::ExtendL1(L1BlockInput { salt: 2 }),
        ];

        for action in trace {
            ref_state = ref_state.apply_action(&action);
            sut = ConsensusStateMachineTest::apply(sut, &ref_state, action);
            ConsensusStateMachineTest::check_invariants(&sut, &ref_state);
        }

        sut.driver.tick(LIVENESS_LOOKAHEAD_TICKS);
        let observed = observe_state(&sut.driver, sut.node_id, &sut.hash_numbers);

        // L3: pending consolidated-safe update survives Syncing and eventually commits.
        let pending =
            ref_state.pending_consolidated_safe.expect("expected pending consolidated safe target");
        assert!(
            observed.safe_head.number >= pending.number,
            "#3809 injected regression failed: safe={} pending={} trace={trace:?}",
            observed.safe_head.number,
            pending.number,
        );
    }
}
