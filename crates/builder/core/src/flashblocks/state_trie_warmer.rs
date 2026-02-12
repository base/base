use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alloy_primitives::{B256, keccak256};
use reth_provider::StateRootProvider;
use reth_trie::{HashedPostState, HashedStorage};
use revm::state::EvmState;
use tracing::{debug, info, trace, warn};

use crate::metrics::BuilderMetrics;

/// Messages sent from execution to the state trie warming task.
/// Mirrors the subset of reth's `MultiProofMessage` relevant for warming.
pub(crate) enum StateTrieMessage {
    /// Per-transaction state diff from EVM execution.
    StateUpdate(EvmState),
    /// Signals that all transactions have been executed (block complete).
    FinishedStateUpdates,
}

/// Sender wrapper that sends state updates to the warming task.
/// Automatically sends `FinishedStateUpdates` on drop, mirroring
/// reth's `StateHookSender` pattern.
pub(crate) struct StateTrieHook {
    tx: Option<mpsc::Sender<StateTrieMessage>>,
}

impl StateTrieHook {
    /// Create a new hook that sends to the warming task.
    pub(crate) const fn new(tx: mpsc::Sender<StateTrieMessage>) -> Self {
        Self { tx: Some(tx) }
    }

    /// Create a no-op hook for when warming is disabled.
    pub(crate) const fn noop() -> Self {
        Self { tx: None }
    }

    /// Send a state update to the warming task.
    /// Clones the evm state and sends it over the channel.
    pub(crate) fn send_state_update(&self, state: &EvmState) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(StateTrieMessage::StateUpdate(state.clone()));
        }
    }
}

impl Drop for StateTrieHook {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(StateTrieMessage::FinishedStateUpdates);
        }
    }
}

/// Background task that receives per-transaction state updates and
/// continuously warms the state trie cache by computing state roots.
///
/// Algorithm:
/// 1. Block on `recv()` for first `StateUpdate`, accumulate into `HashedPostState`
/// 2. Debounce: `recv_timeout(10ms)` to drain more updates
/// 3. Compute `state_root_with_updates(accumulated.clone())` to warm caches
/// 4. `try_recv()` non-blocking drain of messages queued during computation
/// 5. If new messages arrived -> go to 3 (skip debounce)
/// 6. If no new messages -> go to 1 (back to blocking wait)
/// 7. `FinishedStateUpdates` at any step -> final warming, then exit
pub(crate) struct StateTrieWarmerTask<P> {
    rx: mpsc::Receiver<StateTrieMessage>,
    provider: P,
    metrics: Arc<BuilderMetrics>,
    block_number: u64,
}

impl<P> StateTrieWarmerTask<P>
where
    P: StateRootProvider,
{
    /// Create a new warming task.
    pub(crate) const fn new(
        rx: mpsc::Receiver<StateTrieMessage>,
        provider: P,
        metrics: Arc<BuilderMetrics>,
        block_number: u64,
    ) -> Self {
        Self { rx, provider, metrics, block_number }
    }

    /// Run the warming task to completion.
    /// This method blocks and should be called on a blocking thread.
    pub(crate) fn run(self) {
        let Self { rx, provider, metrics, block_number } = self;
        let mut accumulated = HashedPostState::default();
        let mut has_unwarmed_state = false;

        debug!(
            target: "state_trie_warming",
            block_number,
            "State trie warming task started"
        );

        loop {
            // Step 1: Wait for first message (blocking)
            let msg = match rx.recv() {
                Ok(msg) => msg,
                Err(_) => {
                    // Channel closed without FinishedStateUpdates
                    debug!(
                        target: "state_trie_warming",
                        block_number,
                        "Channel closed, warming task exiting"
                    );
                    if has_unwarmed_state {
                        run_warming(&provider, &accumulated, &metrics, block_number);
                    }
                    return;
                }
            };

            match msg {
                StateTrieMessage::FinishedStateUpdates => {
                    debug!(
                        target: "state_trie_warming",
                        block_number,
                        "Received FinishedStateUpdates"
                    );
                    if has_unwarmed_state {
                        run_warming(&provider, &accumulated, &metrics, block_number);
                    }
                    return;
                }
                StateTrieMessage::StateUpdate(state) => {
                    accumulated.extend(evm_state_to_hashed_post_state(state));
                    has_unwarmed_state = true;
                }
            }

            // Step 2: Debounce - drain for 10ms
            if drain_messages(
                &rx,
                &mut accumulated,
                &mut has_unwarmed_state,
                Some(Duration::from_millis(10)),
            ) {
                // Received FinishedStateUpdates during debounce
                if has_unwarmed_state {
                    run_warming(&provider, &accumulated, &metrics, block_number);
                }
                return;
            }

            // Step 3-6: Warming loop
            loop {
                if !has_unwarmed_state {
                    break; // Go back to blocking recv
                }

                // Step 3: Compute state root to warm caches
                run_warming(&provider, &accumulated, &metrics, block_number);
                has_unwarmed_state = false;

                // Step 4: Non-blocking drain of messages queued during computation
                if drain_messages(&rx, &mut accumulated, &mut has_unwarmed_state, None) {
                    // Received FinishedStateUpdates
                    if has_unwarmed_state {
                        run_warming(&provider, &accumulated, &metrics, block_number);
                    }
                    return;
                }

                // Step 5: If new messages arrived, loop back to step 3
                // Step 6: If no new messages, break to step 1
                if !has_unwarmed_state {
                    break;
                }
            }
        }
    }
}

/// Drain messages from the channel, accumulating state updates.
/// If `timeout` is Some, uses `recv_timeout` for the first message then `try_recv` for the rest.
/// If `timeout` is None, only uses `try_recv` (non-blocking).
/// Returns true if `FinishedStateUpdates` was received.
fn drain_messages(
    rx: &mpsc::Receiver<StateTrieMessage>,
    accumulated: &mut HashedPostState,
    has_unwarmed_state: &mut bool,
    timeout: Option<Duration>,
) -> bool {
    // First message: optionally with timeout
    if let Some(timeout) = timeout {
        loop {
            match rx.recv_timeout(timeout) {
                Ok(StateTrieMessage::FinishedStateUpdates) => return true,
                Ok(StateTrieMessage::StateUpdate(state)) => {
                    accumulated.extend(evm_state_to_hashed_post_state(state));
                    *has_unwarmed_state = true;
                }
                Err(_) => break, // Timeout or disconnected
            }
        }
    }

    // Remaining messages: non-blocking
    loop {
        match rx.try_recv() {
            Ok(StateTrieMessage::FinishedStateUpdates) => return true,
            Ok(StateTrieMessage::StateUpdate(state)) => {
                accumulated.extend(evm_state_to_hashed_post_state(state));
                *has_unwarmed_state = true;
            }
            Err(_) => break, // Empty or disconnected
        }
    }

    false
}

/// Run a single state root computation to warm the trie cache.
fn run_warming<P: StateRootProvider>(
    provider: &P,
    accumulated: &HashedPostState,
    metrics: &BuilderMetrics,
    block_number: u64,
) {
    let start_time = Instant::now();
    metrics.state_trie_warming_started_count.increment(1);

    match provider.state_root_with_updates(accumulated.clone()) {
        Ok(_) => {
            let duration = start_time.elapsed();
            metrics.state_trie_warming_completed_count.increment(1);
            metrics.state_trie_warming_duration.record(duration);
            info!(
                target: "state_trie_warming",
                block_number,
                duration_ms = duration.as_millis(),
                "State trie warming completed successfully"
            );
        }
        Err(err) => {
            warn!(
                target: "state_trie_warming",
                block_number,
                error = %err,
                "State trie warming state root calculation failed"
            );
            metrics.state_trie_warming_error_count.increment(1);
        }
    }
}

/// Convert per-transaction EVM state diff to hashed post state.
/// Copied from reth's `multiproof.rs` since it's `pub(crate)` there.
fn evm_state_to_hashed_post_state(update: EvmState) -> HashedPostState {
    let mut hashed_state = HashedPostState::with_capacity(update.len());

    for (address, account) in update {
        if account.is_touched() {
            let hashed_address = keccak256(address);
            trace!(target: "state_trie_warming", ?address, ?hashed_address, "Adding account to state update");

            let destroyed = account.is_selfdestructed();
            let info = if destroyed { None } else { Some(account.info.into()) };
            hashed_state.accounts.insert(hashed_address, info);

            let mut changed_storage_iter = account
                .storage
                .into_iter()
                .filter(|(_slot, value)| value.is_changed())
                .map(|(slot, value)| (keccak256(B256::from(slot)), value.present_value))
                .peekable();

            if destroyed {
                hashed_state.storages.insert(hashed_address, HashedStorage::new(true));
            } else if changed_storage_iter.peek().is_some() {
                hashed_state
                    .storages
                    .insert(hashed_address, HashedStorage::from_iter(false, changed_storage_iter));
            }
        }
    }

    hashed_state
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use reth_provider::ProviderError;
    use reth_trie::{HashedPostState, updates::TrieUpdates};

    use super::*;

    fn test_metrics() -> Arc<BuilderMetrics> {
        Arc::new(BuilderMetrics::default())
    }

    #[test]
    fn noop_hook_does_not_panic() {
        let hook = StateTrieHook::noop();
        hook.send_state_update(&EvmState::default());
        drop(hook);
    }

    #[test]
    fn hook_sends_finished_on_drop() {
        let (tx, rx) = mpsc::channel();
        {
            let hook = StateTrieHook::new(tx);
            hook.send_state_update(&EvmState::default());
        } // hook dropped here

        // Should have received StateUpdate then FinishedStateUpdates
        let msg1 = rx.recv().unwrap();
        assert!(matches!(msg1, StateTrieMessage::StateUpdate(_)));
        let msg2 = rx.recv().unwrap();
        assert!(matches!(msg2, StateTrieMessage::FinishedStateUpdates));
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Mock provider that tracks how many times `state_root_with_updates` is called.
    struct CountingProvider {
        call_count: Arc<AtomicU64>,
    }

    impl CountingProvider {
        fn new() -> (Self, Arc<AtomicU64>) {
            let count = Arc::new(AtomicU64::new(0));
            (Self { call_count: Arc::clone(&count) }, count)
        }
    }

    impl StateRootProvider for CountingProvider {
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
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok((B256::ZERO, TrieUpdates::default()))
        }

        fn state_root_from_nodes_with_updates(
            &self,
            _input: reth_trie::TrieInput,
        ) -> Result<(B256, TrieUpdates), ProviderError> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }
    }

    #[test]
    fn task_runs_to_completion() {
        let (tx, rx) = mpsc::channel();
        let (provider, call_count) = CountingProvider::new();

        let task = StateTrieWarmerTask::new(rx, provider, test_metrics(), 1);

        // Send some updates then finish
        tx.send(StateTrieMessage::StateUpdate(EvmState::default())).unwrap();
        tx.send(StateTrieMessage::FinishedStateUpdates).unwrap();

        // Run on current thread (it's synchronous)
        task.run();

        // Verify warming was attempted
        assert!(call_count.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn task_handles_channel_close() {
        let (tx, rx) = mpsc::channel();
        let (provider, call_count) = CountingProvider::new();

        let task = StateTrieWarmerTask::new(rx, provider, test_metrics(), 1);

        // Send an update then drop sender (no FinishedStateUpdates)
        tx.send(StateTrieMessage::StateUpdate(EvmState::default())).unwrap();
        drop(tx);

        task.run();

        // Should still have warmed
        assert!(call_count.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn task_with_no_updates_exits_cleanly() {
        let (tx, rx) = mpsc::channel();
        let (provider, call_count) = CountingProvider::new();

        let task = StateTrieWarmerTask::new(rx, provider, test_metrics(), 1);

        // Immediately finish
        tx.send(StateTrieMessage::FinishedStateUpdates).unwrap();

        task.run();

        // No warming should have been started (no state updates)
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn task_accumulates_multiple_updates() {
        let (tx, rx) = mpsc::channel();
        let (provider, call_count) = CountingProvider::new();

        let task = StateTrieWarmerTask::new(rx, provider, test_metrics(), 1);

        // Send multiple updates
        for _ in 0..5 {
            tx.send(StateTrieMessage::StateUpdate(EvmState::default())).unwrap();
        }
        tx.send(StateTrieMessage::FinishedStateUpdates).unwrap();

        task.run();

        // Warming should have been performed at least once
        assert!(call_count.load(Ordering::SeqCst) >= 1);
    }
}
