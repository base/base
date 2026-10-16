//! Shared deterministic test harness utilities for actor-integration tests.

mod fake_engine_client;
pub use fake_engine_client::{
    EngineClientCall, FakeEngineClient, FakeEngineClientHandle, ScriptedForkchoiceResponse,
};

mod fake_l1;
pub use fake_l1::FakeL1;

mod l1_watcher;
pub use l1_watcher::{BoxedBlockStream, RecordingDerivationClient, block_at};

mod fake_safedb;
pub use fake_safedb::{FakeSafeDB, FakeSafeDBHandle};

mod builder;
pub use builder::{Harness, HarnessBuilder};

mod driver;
pub use driver::{Driver, DriverProgressSnapshot, NodeSnapshot, ProgressTimeout};

#[cfg(test)]
mod invariant_tests;
