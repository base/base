//! Deterministic runtime driver for actor-integration harnesses.

use std::{future::Future, time::Duration};

use thiserror::Error;

use super::{Harness, HarnessBuilder};
use crate::NodeMode;

/// Node spawn configuration accepted by [`Driver::spawn_node`].
#[derive(Debug, Default)]
pub struct NodeConfig {
    /// Harness builder used to wire fake dependencies.
    pub builder: HarnessBuilder,
}

/// Snapshot of one spawned node.
#[derive(Clone, Debug, Default)]
pub struct NodeSnapshot {
    /// Latest safe-head number observed in fake safedb.
    pub safe_head_number: u64,
    /// Latest unsafe-head number observed in engine state.
    pub unsafe_head_number: u64,
}

/// Snapshot of all nodes managed by the driver.
#[derive(Clone, Debug, Default)]
pub struct DriverProgressSnapshot {
    /// Per-node snapshots in spawn order.
    pub nodes: Vec<NodeSnapshot>,
}

impl DriverProgressSnapshot {
    /// Returns the first validator snapshot if available.
    pub fn validator(&self) -> Option<&NodeSnapshot> {
        self.nodes.first()
    }
}

/// Timeout returned by [`Driver::await_progress`].
#[derive(Clone, Debug, Error)]
#[error("progress condition not met within {timeout_ticks} ticks")]
pub struct ProgressTimeout {
    /// Tick budget.
    pub timeout_ticks: u64,
    /// Last observed snapshot for debugging.
    pub snapshot: DriverProgressSnapshot,
}

/// Deterministic single-thread runtime driver with paused Tokio time.
#[derive(Debug)]
pub struct Driver {
    runtime: tokio::runtime::Runtime,
    harnesses: Vec<Harness>,
}

impl Driver {
    /// Creates a new driver with a current-thread runtime and paused time.
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("failed to build test runtime");
        Self { runtime, harnesses: Vec::new() }
    }

    /// Spawns one node harness in the provided role.
    pub fn spawn_node(&mut self, mode: NodeMode, config: NodeConfig) -> usize {
        let harness = self.runtime.block_on(async { config.builder.with_role(mode).build().await });
        self.harnesses.push(harness);
        self.harnesses.len() - 1
    }

    /// Returns a reference to a harness by node id.
    pub fn harness(&self, node_id: usize) -> &Harness {
        &self.harnesses[node_id]
    }

    /// Advances simulated time by `ticks` deterministic steps.
    ///
    /// Each tick advances paused time by 1ms and yields to the runtime exactly
    /// once. A single tick therefore lets each currently-ready task make one
    /// step of progress; it does not drain a chain of dependent actor messages.
    /// A message that must cross `N` actor hops (e.g. L1 watcher -> derivation
    /// -> engine) needs at least `N` ticks to propagate end-to-end. Size tick
    /// budgets accordingly: multi-hop choreography tests use large budgets such
    /// as `tick(200)` to guarantee the whole chain settles.
    pub fn tick(&mut self, ticks: u64) {
        self.runtime.block_on(async {
            for _ in 0..ticks {
                tokio::time::advance(Duration::from_millis(1)).await;
                tokio::task::yield_now().await;
            }
        });
    }

    /// Executes a future on the driver's internal runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Waits until `condition` is true or `timeout_ticks` expires.
    pub fn await_progress<F>(
        &mut self,
        condition: F,
        timeout_ticks: u64,
    ) -> Result<(), ProgressTimeout>
    where
        F: Fn(&DriverProgressSnapshot) -> bool,
    {
        for _ in 0..=timeout_ticks {
            let snapshot = self.snapshot();
            if condition(&snapshot) {
                return Ok(());
            }
            self.tick(1);
        }

        let snapshot = self.snapshot();
        Err(ProgressTimeout { timeout_ticks, snapshot })
    }

    /// Builds a snapshot for all managed harnesses.
    pub fn snapshot(&self) -> DriverProgressSnapshot {
        let nodes = self.runtime.block_on(async {
            let mut nodes = Vec::with_capacity(self.harnesses.len());
            for harness in &self.harnesses {
                let engine_state = harness.latest_engine_state();
                nodes.push(NodeSnapshot {
                    safe_head_number: harness.latest_safe_head_number().await,
                    unsafe_head_number: engine_state.sync_state.unsafe_head().block_info.number,
                });
            }
            nodes
        });
        DriverProgressSnapshot { nodes }
    }
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}
