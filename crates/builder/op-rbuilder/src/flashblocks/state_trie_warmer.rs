use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use reth_provider::{ExecutionOutcome, HashedPostStateProvider, StateRootProvider};
use reth_revm::State;
use tracing::{debug, info, warn};

use crate::metrics::OpRBuilderMetrics;
use crate::flashblocks::context::OpPayloadBuilderCtx;

pub struct StateTrieWarmer {
    /// Whether state trie warming is enabled
    enabled: bool,
    /// Flag indicating if a warming task is currently running
    warming_in_progress: Arc<AtomicBool>,
    /// Metrics for observability
    metrics: Arc<OpRBuilderMetrics>,
}

impl StateTrieWarmer {
    pub fn new(enabled: bool, metrics: Arc<OpRBuilderMetrics>) -> Self {
        Self {
            enabled,
            warming_in_progress: Arc::new(AtomicBool::new(false)),
            metrics,
        }
    }

    /// Start a background state trie warming task via state root calculation.
    /// NOTE: State root calculation is synchronous and non-interruptible.
    /// Once started, it runs to completion on a blocking thread.
    /// If a warming task is already running, this is a no-op.
    pub fn start_warming<DB>(
        &self,
        state: &State<DB>,
        ctx: &OpPayloadBuilderCtx,
    ) where
        DB: StateRootProvider + HashedPostStateProvider + Clone + Send + 'static
    {
        if !self.enabled {
            return;
        }

        // Check if already warming (atomic check-and-set)
        if self.warming_in_progress.swap(true, Ordering::SeqCst) {
            // Already warming, don't start another
            self.metrics.state_trie_warming_skipped_count.increment(1);
            return;
        }

        self.metrics.state_trie_warming_started_count.increment(1);

        // Clone necessary data for background task
        let state_db = state.database.clone();
        let bundle_state = state.bundle_state.clone();
        let metrics = self.metrics.clone();
        let block_number = ctx.block_number();
        let warming_in_progress = self.warming_in_progress.clone();

        debug!(
            target: "state_trie_warming",
            block_number = block_number,
            "Starting background state trie warming"
        );

        // Spawn background task using spawn_blocking for CPU-intensive work
        tokio::spawn(async move {
            let start_time = Instant::now();

            // Create execution outcome from current bundle state
            let execution_outcome = ExecutionOutcome::new(
                bundle_state,
                vec![],  // No receipts needed for warming
                block_number,
                vec![],
            );

            // Calculate hashed state
            let hashed_state = state_db.hashed_post_state(execution_outcome.state());

            // Perform state root calculation in blocking thread pool
            // IMPORTANT: This is non-interruptible and runs to completion
            let result = tokio::task::spawn_blocking(move || {
                state_db.state_root_with_updates(hashed_state)
            })
            .await;

            match result {
                Ok(Ok(_)) => {
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
                Ok(Err(err)) => {
                    warn!(
                        target: "state_trie_warming",
                        block_number = block_number,
                        error = %err,
                        "State trie warming state root calculation failed"
                    );
                    metrics.state_trie_warming_error_count.increment(1);
                }
                Err(err) => {
                    warn!(
                        target: "state_trie_warming",
                        block_number = block_number,
                        error = %err,
                        "State trie warming task panicked"
                    );
                    metrics.state_trie_warming_error_count.increment(1);
                }
            }

            // Clear in-progress flag
            warming_in_progress.store(false, Ordering::SeqCst);
        });
    }

    /// Check if warming is currently in progress
    #[allow(dead_code)]
    pub fn is_warming(&self) -> bool {
        self.warming_in_progress.load(Ordering::SeqCst)
    }
}
