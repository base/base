//! Bounded Rayon runtime configuration for in-process test nodes.

use reth_tasks::{RayonConfig, RuntimeConfig};

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
    /// Threads for the Rayon CPU, RPC, and storage pools.
    const POOL_THREADS: usize = 2;
    /// Threads for the proof, prewarming, BAL streaming, and state-trie overlay pools.
    ///
    /// Reth halves proof-worker counts for small blocks. This must remain at least two so integer
    /// division does not turn `1 / 2` into an empty account or storage worker pool.
    const WORKER_POOL_THREADS: usize = 2;

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
    use super::TestNodeRuntime;

    #[test]
    fn proof_worker_pools_survive_small_block_halving() {
        let config = TestNodeRuntime::config();

        assert!(config.rayon.proof_storage_worker_threads.unwrap() / 2 > 0);
        assert!(config.rayon.proof_account_worker_threads.unwrap() / 2 > 0);
    }
}
