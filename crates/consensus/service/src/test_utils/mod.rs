//! Shared deterministic test harness utilities for actor-integration tests.
#![cfg(test)]

mod fake_engine_client;
pub use fake_engine_client::{
    EngineClientCall, FakeEngineClient, FakeEngineClientHandle, ScriptedForkchoiceResponse,
};

mod fake_l1;
pub use fake_l1::{FakeL1, FakeL1BeaconError, FakeL1State};

mod fake_gossip;
pub use fake_gossip::{FakeGossipError, FakeGossipHandle, FakeGossipTransport};

mod fake_safedb;
pub use fake_safedb::{FakeSafeDB, FakeSafeDBHandle};

mod builder;
pub use builder::{Harness, HarnessBuilder};

mod driver;
pub use driver::{Driver, DriverProgressSnapshot, NodeConfig, NodeSnapshot, ProgressTimeout};

mod two_node;
pub use two_node::{
    FakeGossipTransportHandle, FakeL1Handle, NodeHandles, TimeoutError, TwoNodeHarness,
};

mod proptest_model;
pub use proptest_model::{
    Action, AttributesRef, ConsensusReferenceMachine, ConsensusStateMachineTest,
    EngineResponseKind, GossipPayload, L1BlockInput, L2BlockRef, LIVENESS_LOOKAHEAD_TICKS,
    ObservedState, RefState, SutState, apply_action, check_liveness, check_safety,
    count_duplicate_fcu_heads, deterministic_hash, observe_state, run_async, scripted_response,
};

mod edge_cases;
pub use edge_cases::EDGE_CASE_TEST_COUNT;

mod syncing_stalls;
pub use syncing_stalls::SYNCING_STALL_TEST_COUNT;

mod invariant_tests;
pub use invariant_tests::INVARIANT_TEST_COUNT;

mod sequencer_and_reorg;

mod syncing_stalls_wave2;
pub use syncing_stalls_wave2::SYNCING_STALL_WAVE2_TEST_COUNT;
