//! Bounded Rayon runtime configuration for in-process test nodes.

use clap::Args;
use eyre::{Result, ensure};
use reth_node_core::node_config::NodeConfig;
use reth_tasks::{RayonConfig, RuntimeConfig};
use serde::{Deserialize, Serialize};

/// Runtime sizing policy for an in-process execution node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProcessNodeRuntime {
    /// Use Reth's host-derived worker pool sizing for benchmarks and developer networks.
    Host,
    /// Use bounded worker pools to keep concurrent system tests from oversubscribing CI hosts.
    SystemTest,
}

impl InProcessNodeRuntime {
    /// Returns the Reth runtime configuration for this policy.
    pub fn config(self) -> RuntimeConfig {
        match self {
            Self::Host => RuntimeConfig::default(),
            Self::SystemTest => TestNodeRuntime::config(),
        }
    }

    /// Applies this policy's memory bounds to a node's engine configuration.
    ///
    /// Reth sizes its cross-block state cache at 4 `GiB` per node by default. A system-test stack
    /// runs a builder and client node in one process and several stacks run at once, so the
    /// default reserves far more than a devnet's state needs.
    pub fn bound_engine_memory<C>(self, node_config: &mut NodeConfig<C>) {
        if self == Self::SystemTest {
            node_config.engine.cross_block_cache_size = TestNodeRuntime::CROSS_BLOCK_CACHE_SIZE_MB;
        }
    }
}

/// Opt-in engine state shared with the native payload builder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Args)]
pub struct BuilderEngineSharingConfig {
    /// Share the engine's sparse trie with the native builder (requires at least five CPUs).
    #[arg(long = "engine.share-sparse-trie-with-payload-builder")]
    pub share_sparse_trie_with_payload_builder: bool,
    /// Share the engine's execution cache with the native builder.
    #[arg(long = "engine.share-execution-cache-with-payload-builder")]
    pub share_execution_cache_with_payload_builder: bool,
    /// Suppress threshold-triggered persistence while the native builder holds a payload job.
    #[arg(long = "engine.suppress-persistence-during-build")]
    #[serde(default)]
    pub suppress_persistence_during_build: bool,
}

impl BuilderEngineSharingConfig {
    /// Apply explicit sharing settings without changing the engine's pool sizing.
    pub fn apply<C>(self, node_config: &mut NodeConfig<C>) -> Result<()> {
        ensure!(
            !self.share_sparse_trie_with_payload_builder
                || node_config.engine.tree_config().has_enough_parallelism(),
            "sparse-trie sharing requires at least five available CPUs"
        );
        node_config.engine.suppress_persistence_during_build =
            self.suppress_persistence_during_build;
        node_config.engine.share_sparse_trie_with_payload_builder =
            self.share_sparse_trie_with_payload_builder;
        node_config.engine.share_execution_cache_with_payload_builder =
            self.share_execution_cache_with_payload_builder;
        Ok(())
    }
}

/// Persistence policy shared by snapshot builders and followers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Args)]
pub struct SnapshotPersistenceConfig {
    /// Leave persistence settings to Reth rather than persisting every block.
    /// Applies equally to builder and follower; disabled to preserve benchmark baselines.
    #[arg(long)]
    pub use_reth_persistence_defaults: bool,
}

impl SnapshotPersistenceConfig {
    /// Returns the legacy threshold override, or leaves Reth's default untouched.
    pub const fn threshold(self) -> Option<u64> {
        if self.use_reth_persistence_defaults { None } else { Some(0) }
    }

    /// Returns the legacy cadence-dependent backpressure override, or Reth's default.
    pub fn backpressure(self, interval: crate::DevnetBlockInterval) -> Option<u64> {
        if self.use_reth_persistence_defaults {
            None
        } else {
            Some(interval.snapshot_persistence_backpressure_threshold())
        }
    }
}

/// Small, fixed Rayon thread-pool sizing for the reth runtime backing an in-process test node.
///
/// The in-process nodes share the test Tokio runtime, but Reth's default Rayon pools are sized
/// from the host core count. A system-test stack runs a builder and client node, and several
/// stacks share one CI runner. Capping those pools prevents the combined nodes from
/// oversubscribing CPU and memory, which otherwise stalls block production and causes receipt
/// timeouts.
#[derive(Debug, Clone, Copy)]
pub struct TestNodeRuntime;

impl TestNodeRuntime {
    /// Cross-block state cache size, in MB, for each in-process test node.
    const CROSS_BLOCK_CACHE_SIZE_MB: usize = 64;
    /// Threads for the Rayon CPU, RPC, and storage pools.
    const POOL_THREADS: usize = 2;
    /// Threads for the proof, prewarming, BAL streaming, and state-trie overlay pools.
    const WORKER_POOL_THREADS: usize = 1;

    /// Returns a [`RuntimeConfig`] with bounded Rayon pools.
    pub fn config() -> RuntimeConfig {
        RuntimeConfig::default().with_rayon(RayonConfig {
            cpu_threads: Some(Self::POOL_THREADS),
            rpc_threads: Some(Self::POOL_THREADS),
            storage_threads: Some(Self::POOL_THREADS),
            proof_storage_worker_threads: Some(Self::WORKER_POOL_THREADS),
            proof_account_worker_threads: Some(Self::WORKER_POOL_THREADS),
            prewarming_threads: Some(Self::WORKER_POOL_THREADS),
            bal_streaming_threads: Some(Self::WORKER_POOL_THREADS),
            state_trie_overlay_worker_threads: Some(Self::WORKER_POOL_THREADS),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production persistence keeps upstream values and suppression is builder-only.
    #[test]
    fn persistence_defaults_and_builder_suppression() {
        let mut builder = NodeConfig::default();
        let follower = NodeConfig::default();
        BuilderEngineSharingConfig { suppress_persistence_during_build: true, ..Default::default() }
            .apply(&mut builder).unwrap();
        assert_eq!(builder.engine.suppress_persistence_during_build, true, "builder receives suppression");
        assert_eq!(follower.engine.suppress_persistence_during_build, false, "follower retains suppression default");
        assert_eq!(builder.engine.persistence_threshold, 50, "reth persistence threshold is retained");
        assert_eq!(builder.engine.memory_block_buffer_target(), 5, "reth memory buffer is retained");
        assert_eq!(builder.engine.persistence_backpressure_threshold(), 100, "reth backpressure is retained");
        assert_eq!(builder.engine.num_state_masking_blocks(), 30, "reth masking is retained");
        let legacy = SnapshotPersistenceConfig::default();
        assert_eq!(legacy.threshold(), Some(0), "legacy persists every block");
        assert_eq!(legacy.backpressure(crate::DevnetBlockInterval::TwoHundredMilliseconds), Some(160), "legacy cadence backpressure is retained");
        let production = SnapshotPersistenceConfig { use_reth_persistence_defaults: true };
        assert_eq!(production.threshold(), None, "production threshold must be left to reth");
        assert_eq!(production.backpressure(crate::DevnetBlockInterval::TwoHundredMilliseconds), None, "production backpressure must be left to reth");
    }

    /// Defaults preserve the disabled engine settings; cache-only sharing is independently opt-in.
    #[test]
    fn sharing_defaults_and_cache_opt_in() {
        let mut node = NodeConfig::default();
        BuilderEngineSharingConfig::default().apply(&mut node).unwrap();
        assert!(!node.engine.share_sparse_trie_with_payload_builder, "sparse sharing stays off");
        assert!(!node.engine.share_execution_cache_with_payload_builder, "cache sharing stays off");
        BuilderEngineSharingConfig {
            share_execution_cache_with_payload_builder: true,
            ..Default::default()
        }
        .apply(&mut node)
        .unwrap();
        assert!(
            node.engine.share_execution_cache_with_payload_builder,
            "cache flag reaches EngineArgs"
        );
        assert!(
            !node.engine.share_sparse_trie_with_payload_builder,
            "cache sharing does not imply sparse sharing"
        );
    }

    /// Sparse sharing either reaches `EngineArgs` or fails clearly on a small host.
    #[test]
    fn sparse_sharing_requires_parallelism() {
        let mut node = NodeConfig::default();
        let supported = node.engine.tree_config().has_enough_parallelism();
        let result = BuilderEngineSharingConfig {
            share_sparse_trie_with_payload_builder: true,
            share_execution_cache_with_payload_builder: true,
        }
        .apply(&mut node);
        if supported {
            result.expect("supported hosts must accept sparse sharing");
            assert!(
                node.engine.share_sparse_trie_with_payload_builder,
                "sparse flag reaches EngineArgs"
            );
            assert!(
                node.engine.share_execution_cache_with_payload_builder,
                "cache flag reaches EngineArgs"
            );
        } else {
            assert!(
                result.unwrap_err().to_string().contains("at least five"),
                "small hosts must fail clearly"
            );
            assert!(
                !node.engine.share_sparse_trie_with_payload_builder,
                "failed validation leaves flags unchanged"
            );
        }
    }
}
