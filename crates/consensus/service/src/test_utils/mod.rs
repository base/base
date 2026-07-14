//! Shared deterministic test harness utilities for actor-integration tests.

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

mod invariant_tests;
pub use invariant_tests::INVARIANT_TEST_COUNT;
