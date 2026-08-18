//! Bounded tokio/rayon runtime configuration for in-process test nodes.

use reth_tasks::{RayonConfig, RuntimeConfig, TokioConfig};

/// Small, fixed thread-pool sizing for the reth runtime backing an in-process test node.
///
/// reth's [`RuntimeConfig::default`] sizes almost every pool from the host core count: the
/// tokio worker pool, the rayon cpu/rpc/prewarming/BAL pools, a 16-thread storage pool, and
/// account/storage proof pools at 2x the core count each. A single system-test stack runs a
/// builder and a client node (plus a node per shadow sequencer), and the suite runs several
/// stacks at once on a shared CI runner, so the defaults fan out to hundreds of threads per
/// node. That starves both RAM (OOM-killing the runner) and CPU (the chain then produces
/// blocks too slowly and transaction receipts time out).
///
/// Test nodes build tiny blocks, so a handful of threads per pool is ample. Capping every
/// pool keeps each node light enough that concurrent stacks fit on the runner.
#[derive(Debug, Clone, Copy)]
pub struct TestNodeRuntime;

impl TestNodeRuntime {
    /// tokio worker threads per node.
    const WORKER_THREADS: usize = 2;
    /// Threads for the rayon cpu, rpc, and storage pools.
    const POOL_THREADS: usize = 2;
    /// Threads for the proof, prewarming, BAL streaming, and state-trie overlay worker pools.
    const WORKER_POOL_THREADS: usize = 1;

    /// Returns a [`RuntimeConfig`] with all tokio and rayon pools bounded to small fixed sizes.
    pub fn config() -> RuntimeConfig {
        RuntimeConfig::default()
            .with_tokio(TokioConfig::with_worker_threads(Self::WORKER_THREADS))
            .with_rayon(RayonConfig {
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
