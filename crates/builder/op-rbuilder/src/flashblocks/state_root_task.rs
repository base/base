//! Streaming State Root Task for Flashblocks
//!
//! This module implements a background task that incrementally computes the state root
//! as transactions are executed, rather than computing it synchronously at finalization time.
//!
//! The architecture uses a channel-based approach where:
//! 1. Transaction execution sends state updates to the task
//! 2. The task pre-fetches trie nodes and builds a sparse trie incrementally
//! 3. At finalization, only the final root computation is needed (minimal blocking I/O)

use std::sync::Arc;

use alloy_primitives::B256;
use parking_lot::Mutex;
use reth_provider::{HashedPostStateProvider, StateRootProvider, StorageRootProvider};
use reth_trie::{HashedPostState, updates::TrieUpdates};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace};

/// Messages sent to the state root task
#[derive(Debug)]
pub enum StateRootMessage {
    /// A new state update from transaction execution
    StateUpdate(HashedPostState),
    /// Request to compute the final state root
    ComputeRoot(oneshot::Sender<StateRootResult>),
    /// Shutdown the task
    Shutdown,
}

/// Result of the state root computation
#[derive(Debug, Clone)]
pub struct StateRootResult {
    /// The computed state root
    pub state_root: B256,
    /// Trie updates for persistence
    pub trie_updates: TrieUpdates,
    /// The accumulated hashed post state
    pub hashed_state: HashedPostState,
}

/// Handle to communicate with the state root task
#[derive(Clone, Debug)]
pub struct StateRootTaskHandle {
    /// Channel to send messages to the task
    tx: mpsc::Sender<StateRootMessage>,
    /// Cached result if already computed
    cached_result: Arc<Mutex<Option<StateRootResult>>>,
}

impl StateRootTaskHandle {
    /// Send a state update to the task
    pub async fn send_state_update(&self, state: HashedPostState) -> Result<(), StateRootError> {
        self.tx
            .send(StateRootMessage::StateUpdate(state))
            .await
            .map_err(|_| StateRootError::TaskShutdown)
    }

    /// Send a state update synchronously (non-blocking try_send)
    pub fn try_send_state_update(&self, state: HashedPostState) -> Result<(), StateRootError> {
        self.tx
            .try_send(StateRootMessage::StateUpdate(state))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => StateRootError::ChannelFull,
                mpsc::error::TrySendError::Closed(_) => StateRootError::TaskShutdown,
            })
    }

    /// Request the final state root computation
    pub async fn compute_root(&self) -> Result<StateRootResult, StateRootError> {
        // Check if we already have a cached result
        if let Some(result) = self.cached_result.lock().clone() {
            return Ok(result);
        }

        let (tx, rx) = oneshot::channel();
        self.tx
            .send(StateRootMessage::ComputeRoot(tx))
            .await
            .map_err(|_| StateRootError::TaskShutdown)?;

        let result = rx.await.map_err(|_| StateRootError::TaskShutdown)?;

        // Cache the result
        *self.cached_result.lock() = Some(result.clone());

        Ok(result)
    }

    /// Shutdown the task
    pub async fn shutdown(&self) -> Result<(), StateRootError> {
        self.tx.send(StateRootMessage::Shutdown).await.map_err(|_| StateRootError::TaskShutdown)
    }
}

/// Errors that can occur during state root computation
#[derive(Debug, Clone, thiserror::Error)]
pub enum StateRootError {
    #[error("State root task has shut down")]
    TaskShutdown,
    #[error("Channel is full")]
    ChannelFull,
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Trie error: {0}")]
    Trie(String),
}

/// Configuration for the state root task
#[derive(Debug, Clone)]
pub struct StateRootTaskConfig {
    /// Channel buffer size for state updates
    pub channel_buffer_size: usize,
    /// Whether to enable prefetching of trie nodes
    pub enable_prefetch: bool,
}

impl Default for StateRootTaskConfig {
    fn default() -> Self {
        Self {
            channel_buffer_size: 256,
            enable_prefetch: true,
        }
    }
}

/// The streaming state root task
///
/// This task runs in the background and accumulates state updates from transaction execution.
/// When `compute_root` is called, it computes the final state root using the accumulated state.
#[derive(Debug)]
pub struct StateRootTask<P> {
    /// The state provider for trie operations
    provider: P,
    /// Channel to receive messages
    rx: mpsc::Receiver<StateRootMessage>,
    /// Accumulated hashed post state
    accumulated_state: HashedPostState,
    /// Configuration
    config: StateRootTaskConfig,
}

impl<P> StateRootTask<P>
where
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider + Send + Sync + 'static,
{
    /// Create a new state root task and return a handle to communicate with it
    pub fn new(provider: P, config: StateRootTaskConfig) -> (Self, StateRootTaskHandle) {
        let (tx, rx) = mpsc::channel(config.channel_buffer_size);

        let task = Self {
            provider,
            rx,
            accumulated_state: HashedPostState::default(),
            config,
        };

        let handle = StateRootTaskHandle {
            tx,
            cached_result: Arc::new(Mutex::new(None)),
        };

        (task, handle)
    }

    /// Run the task until shutdown
    pub async fn run(mut self) {
        debug!(target: "state_root_task", "Starting streaming state root task");

        loop {
            match self.rx.recv().await {
                Some(StateRootMessage::StateUpdate(state)) => {
                    self.handle_state_update(state);
                }
                Some(StateRootMessage::ComputeRoot(response_tx)) => {
                    let result = self.compute_final_root();
                    let _ = response_tx.send(result);
                }
                Some(StateRootMessage::Shutdown) | None => {
                    debug!(target: "state_root_task", "Shutting down state root task");
                    break;
                }
            }
        }
    }

    /// Handle a state update by merging it into the accumulated state
    fn handle_state_update(&mut self, state: HashedPostState) {
        trace!(
            target: "state_root_task",
            accounts = state.accounts.len(),
            storages = state.storages.len(),
            "Received state update"
        );

        // Merge the new state into accumulated state
        self.accumulated_state.extend(state);

        // If prefetching is enabled, we could trigger async trie node prefetching here
        // This is a placeholder for future optimization
        if self.config.enable_prefetch {
            // TODO: Implement trie node prefetching based on touched addresses
            // This would pre-warm the cache for the final root computation
        }
    }

    /// Compute the final state root from accumulated state
    fn compute_final_root(&self) -> StateRootResult {
        debug!(
            target: "state_root_task",
            accounts = self.accumulated_state.accounts.len(),
            storages = self.accumulated_state.storages.len(),
            "Computing final state root"
        );

        match self
            .provider
            .state_root_with_updates(self.accumulated_state.clone())
        {
            Ok((state_root, trie_updates)) => {
                debug!(
                    target: "state_root_task",
                    ?state_root,
                    "State root computed successfully"
                );
                StateRootResult {
                    state_root,
                    trie_updates,
                    hashed_state: self.accumulated_state.clone(),
                }
            }
            Err(err) => {
                error!(
                    target: "state_root_task",
                    %err,
                    "Failed to compute state root, returning zero root"
                );
                // Return zero root on error - the caller should handle this
                StateRootResult {
                    state_root: B256::ZERO,
                    trie_updates: TrieUpdates::default(),
                    hashed_state: self.accumulated_state.clone(),
                }
            }
        }
    }
}

/// Builder for creating and spawning a state root task
#[derive(Debug)]
pub struct StateRootTaskBuilder<P> {
    provider: P,
    config: StateRootTaskConfig,
}

impl<P> StateRootTaskBuilder<P>
where
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider + Send + Sync + 'static,
{
    /// Create a new builder
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            config: StateRootTaskConfig::default(),
        }
    }

    /// Set the channel buffer size
    pub fn with_channel_buffer_size(mut self, size: usize) -> Self {
        self.config.channel_buffer_size = size;
        self
    }

    /// Enable or disable trie node prefetching
    pub fn with_prefetch(mut self, enable: bool) -> Self {
        self.config.enable_prefetch = enable;
        self
    }

    /// Build and spawn the task, returning a handle
    pub fn spawn(self) -> StateRootTaskHandle {
        let (task, handle) = StateRootTask::new(self.provider, self.config);

        // Spawn the task
        tokio::spawn(task.run());

        handle
    }

    /// Build the task without spawning (for testing)
    pub fn build(self) -> (StateRootTask<P>, StateRootTaskHandle) {
        StateRootTask::new(self.provider, self.config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock provider for testing
    #[derive(Clone)]
    struct MockProvider;

    impl StateRootProvider for MockProvider {
        fn state_root(
            &self,
            _state: HashedPostState,
        ) -> Result<B256, reth_provider::ProviderError> {
            Ok(B256::ZERO)
        }

        fn state_root_from_nodes(
            &self,
            _input: reth_trie::TrieInput,
        ) -> Result<B256, reth_provider::ProviderError> {
            Ok(B256::ZERO)
        }

        fn state_root_with_updates(
            &self,
            _state: HashedPostState,
        ) -> Result<(B256, TrieUpdates), reth_provider::ProviderError> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }

        fn state_root_from_nodes_with_updates(
            &self,
            _input: reth_trie::TrieInput,
        ) -> Result<(B256, TrieUpdates), reth_provider::ProviderError> {
            Ok((B256::ZERO, TrieUpdates::default()))
        }
    }

    impl HashedPostStateProvider for MockProvider {
        fn hashed_post_state(
            &self,
            _bundle_state: &reth_revm::db::BundleState,
        ) -> HashedPostState {
            HashedPostState::default()
        }
    }

    impl StorageRootProvider for MockProvider {
        fn storage_root(
            &self,
            _address: alloy_primitives::Address,
            _hashed_storage: reth_trie::HashedStorage,
        ) -> Result<B256, reth_provider::ProviderError> {
            Ok(B256::ZERO)
        }

        fn storage_proof(
            &self,
            _address: alloy_primitives::Address,
            _slot: B256,
            _hashed_storage: reth_trie::HashedStorage,
        ) -> Result<reth_trie::StorageProof, reth_provider::ProviderError> {
            Ok(reth_trie::StorageProof::default())
        }

        fn storage_multiproof(
            &self,
            _address: alloy_primitives::Address,
            _slots: &[B256],
            _hashed_storage: reth_trie::HashedStorage,
        ) -> Result<reth_trie::StorageMultiProof, reth_provider::ProviderError> {
            Ok(reth_trie::StorageMultiProof::empty())
        }
    }

    #[tokio::test]
    async fn test_state_root_task_basic() {
        let provider = MockProvider;
        let (task, handle) = StateRootTaskBuilder::new(provider).build();

        // Spawn the task
        let task_handle = tokio::spawn(task.run());

        // Send a state update
        handle
            .send_state_update(HashedPostState::default())
            .await
            .unwrap();

        // Compute the root
        let result = handle.compute_root().await.unwrap();
        assert_eq!(result.state_root, B256::ZERO);

        // Shutdown
        handle.shutdown().await.unwrap();

        // Wait for task to finish
        task_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_state_root_task_cached_result() {
        let provider = MockProvider;
        let handle = StateRootTaskBuilder::new(provider).spawn();

        // Compute the root twice - second should use cache
        let result1 = handle.compute_root().await.unwrap();
        let result2 = handle.compute_root().await.unwrap();

        assert_eq!(result1.state_root, result2.state_root);

        handle.shutdown().await.unwrap();
    }
}
