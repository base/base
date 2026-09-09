//! Deterministic runtime driver for actor-integration harnesses.

use std::time::Duration;

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

/// Deterministic driver for actor-integration harnesses.
///
/// The driver is runtime-agnostic: it drives its harnesses on the ambient Tokio
/// runtime of the calling test. Tests MUST run on a current-thread runtime with
/// paused time (`#[tokio::test(start_paused = true)]`) because [`Driver::tick`]
/// advances the mock clock via [`tokio::time::advance`], which panics unless
/// time is paused.
#[derive(Debug)]
pub struct Driver {
    harnesses: Vec<Harness>,
}

impl Driver {
    /// Creates a new driver with no harnesses.
    pub fn new() -> Self {
        Self { harnesses: Vec::new() }
    }

    /// Spawns one node harness in the provided role.
    pub async fn spawn_node(&mut self, mode: NodeMode, config: NodeConfig) -> usize {
        let harness = config.builder.with_role(mode).build().await;
        self.harnesses.push(harness);
        self.harnesses.len() - 1
    }

    /// Returns a reference to a harness by node id.
    pub fn harness(&self, node_id: usize) -> &Harness {
        &self.harnesses[node_id]
    }

    /// Advances simulated time by `ticks` deterministic steps.
    ///
    /// Each tick advances paused time by 1ms and yields to the runtime exactly once, allowing one
    /// ready task to make one step of progress. Because multiple actors may each be ready, a
    /// message crossing `N` actor hops can take significantly more than `N` ticks depending on
    /// task interleaving. Size budgets conservatively — multi-hop tests use large values such as
    /// `tick(200)` to guarantee the whole chain settles regardless of scheduling order.
    pub async fn tick(&mut self, ticks: u64) {
        for _ in 0..ticks {
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }
    }

    /// Waits until `condition` is true or `timeout_ticks` expires.
    pub async fn await_progress<F>(
        &mut self,
        condition: F,
        timeout_ticks: u64,
    ) -> Result<(), ProgressTimeout>
    where
        F: Fn(&DriverProgressSnapshot) -> bool,
    {
        for _ in 0..=timeout_ticks {
            let snapshot = self.snapshot().await;
            if condition(&snapshot) {
                return Ok(());
            }
            self.tick(1).await;
        }

        let snapshot = self.snapshot().await;
        Err(ProgressTimeout { timeout_ticks, snapshot })
    }

    /// Builds a snapshot for all managed harnesses.
    pub async fn snapshot(&self) -> DriverProgressSnapshot {
        let mut nodes = Vec::with_capacity(self.harnesses.len());
        for harness in &self.harnesses {
            let engine_state = harness.latest_engine_state();
            nodes.push(NodeSnapshot {
                safe_head_number: harness.latest_safe_head_number().await,
                unsafe_head_number: engine_state.sync_state.unsafe_head().block_info.number,
            });
        }
        DriverProgressSnapshot { nodes }
    }
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}
