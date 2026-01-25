use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use reth_optimism_primitives::OpReceipt;
use reth_provider::{ExecutionOutcome, HashedPostStateProvider, StateRootProvider};
use reth_revm::db::states::bundle_state::BundleState;
use tracing::{debug, info, warn};

use crate::metrics::BuilderMetrics;

#[derive(Debug)]
pub struct StateTrieWarmer {
    /// Whether state trie warming is enabled
    enabled: bool,
    /// Flag indicating if a warming task is currently running
    warming_in_progress: Arc<AtomicBool>,
    /// Metrics for observability
    metrics: Arc<BuilderMetrics>,
}

impl StateTrieWarmer {
    pub fn new(enabled: bool, metrics: Arc<BuilderMetrics>) -> Self {
        Self { enabled, warming_in_progress: Arc::new(AtomicBool::new(false)), metrics }
    }

    /// Start a background state trie warming task via state root calculation.
    /// NOTE: State root calculation is synchronous and non-interruptible.
    /// Once started, it runs to completion on a blocking thread.
    /// If a warming task is already running, this is a no-op.
    pub fn start_warming<P>(&self, provider: P, bundle_state: BundleState, block_number: u64)
    where
        P: StateRootProvider + HashedPostStateProvider + Send + 'static,
    {
        if !self.enabled {
            return;
        }

        // Check if already warming (atomic check-and-set)
        if self.warming_in_progress.swap(true, Ordering::AcqRel) {
            // Already warming, don't start another
            self.metrics.state_trie_warming_skipped_count.increment(1);
            return;
        }

        self.metrics.state_trie_warming_started_count.increment(1);

        let metrics = self.metrics.clone();
        let warming_in_progress = self.warming_in_progress.clone();

        debug!(
            target: "state_trie_warming",
            block_number = block_number,
            "Starting background state trie warming"
        );

        // All work is CPU-intensive, so run entirely on the blocking thread pool
        tokio::task::spawn_blocking(move || {
            let start_time = Instant::now();

            // Create execution outcome from current bundle state
            let receipts: Vec<Vec<OpReceipt>> = vec![];
            let execution_outcome =
                ExecutionOutcome::new(bundle_state, receipts, block_number, vec![]);

            // Calculate hashed state and perform state root calculation to warm caches
            let hashed_state = provider.hashed_post_state(execution_outcome.state());

            match provider.state_root_with_updates(hashed_state) {
                Ok(_) => {
                    let duration = start_time.elapsed();
                    metrics.state_trie_warming_completed_count.increment(1);
                    metrics.state_trie_warming_duration.record(duration);
                    info!(
                        target: "state_trie_warming",
                        block_number = block_number,
                        duration_ms = duration.as_millis(),
                        "State trie warming completed successfully"
                    );
                }
                Err(err) => {
                    warn!(
                        target: "state_trie_warming",
                        block_number = block_number,
                        error = %err,
                        "State trie warming state root calculation failed"
                    );
                    metrics.state_trie_warming_error_count.increment(1);
                }
            }

            // Clear in-progress flag
            warming_in_progress.store(false, Ordering::Release);
        });
    }

    /// Check if warming is currently in progress
    #[allow(dead_code)]
    pub fn is_warming(&self) -> bool {
        self.warming_in_progress.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::B256;
    use reth_provider::ProviderError;
    use reth_trie::{HashedPostState, updates::TrieUpdates};

    use super::*;

    /// Mock provider that implements both traits with configurable delay.
    struct MockProvider {
        delay: Duration,
    }

    impl MockProvider {
        fn instant() -> Self {
            Self { delay: Duration::ZERO }
        }

        fn slow(delay: Duration) -> Self {
            Self { delay }
        }
    }

    impl StateRootProvider for MockProvider {
        fn state_root(&self, _state: HashedPostState) -> Result<B256, ProviderError> {
            Ok(B256::ZERO)
        }

        fn state_root_from_nodes(
            &self,
            _input: reth_trie::TrieInput,
        ) -> Result<B256, ProviderError> {
            Ok(B256::ZERO)
        }

        fn state_root_with_updates(
            &self,
            _state: HashedPostState,
        ) -> Result<(B256, TrieUpdates), ProviderError> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            Ok((B256::ZERO, TrieUpdates::default()))
        }

        fn state_root_from_nodes_with_updates(
            &self,
            _input: reth_trie::TrieInput,
        ) -> Result<(B256, TrieUpdates), ProviderError> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }
    }

    impl HashedPostStateProvider for MockProvider {
        fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
            HashedPostState::default()
        }
    }

    fn test_metrics() -> Arc<BuilderMetrics> {
        Arc::new(BuilderMetrics::default())
    }

    #[tokio::test]
    async fn warming_disabled_is_noop() {
        let warmer = StateTrieWarmer::new(false, test_metrics());
        warmer.start_warming(MockProvider::instant(), BundleState::default(), 1);
        assert!(!warmer.is_warming(), "warming should not start when disabled");
    }

    #[tokio::test]
    async fn warming_enabled_starts_and_completes() {
        let warmer = StateTrieWarmer::new(true, test_metrics());
        warmer.start_warming(MockProvider::instant(), BundleState::default(), 1);

        // Wait for the blocking task to finish
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!warmer.is_warming(), "warming should have completed");
    }

    #[tokio::test]
    async fn concurrent_warming_is_deduplicated() {
        let metrics = test_metrics();
        let warmer = StateTrieWarmer::new(true, metrics.clone());

        // Start a slow warming task
        warmer.start_warming(
            MockProvider::slow(Duration::from_millis(200)),
            BundleState::default(),
            1,
        );

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(warmer.is_warming(), "first warming should be in progress");

        // Second call should be a no-op (deduplicated)
        warmer.start_warming(MockProvider::instant(), BundleState::default(), 1);

        // First should still be running
        assert!(warmer.is_warming(), "warming should still be in progress");

        // Wait for completion
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!warmer.is_warming(), "warming should have completed");
    }
}
