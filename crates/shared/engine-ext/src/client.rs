//! In-process engine client implementation.

use reth_engine_primitives::ConsensusEngineHandle;
use reth_optimism_node::OpEngineTypes;
use reth_payload_builder::PayloadStore;

use crate::ForkchoiceTracker;

/// In-process engine client that bypasses RPC for direct channel communication.
///
/// This client uses reth's internal [`ConsensusEngineHandle`] and [`PayloadStore`]
/// to communicate with the engine, avoiding JSON-RPC serialization overhead.
///
/// # Type Parameters
///
/// * `P` - The provider type used for querying L2 chain state.
///
/// # Architecture
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────────────┐
/// │                           RPC Layer (bypassed)                          │
/// └─────────────────────────────────────────────────────────────────────────┘
///                                      │
///                                      │ (skipped)
///                                      ▼
/// ┌─────────────────────────────────────────────────────────────────────────┐
/// │                   InProcessEngineClient<P>                              │
/// │    - new_payload(), fork_choice_updated() methods                       │
/// │    - get_payload() via PayloadStore                                     │
/// │    - l2_block_info_*() via Provider                                     │
/// └─────────────────────────────────────────────────────────────────────────┘
///                                      │
///                                      │ unbounded channel
///                                      ▼
/// ┌─────────────────────────────────────────────────────────────────────────┐
/// │                    EngineApiTreeHandler                                 │
/// │    - Runs in its own std::thread                                        │
/// │    - Processes EngineApiRequest variants                                │
/// └─────────────────────────────────────────────────────────────────────────┘
/// ```
#[derive(Debug)]
pub struct InProcessEngineClient<P> {
    /// Handle for standard newPayload/FCU calls.
    pub(crate) consensus_handle: ConsensusEngineHandle<OpEngineTypes>,
    /// Store for retrieving built payloads.
    pub(crate) payload_store: PayloadStore<OpEngineTypes>,
    /// Provider for querying L2 chain state.
    pub(crate) provider: P,
    /// Forkchoice state tracker.
    pub(crate) forkchoice: ForkchoiceTracker,
}

impl<P> InProcessEngineClient<P> {
    /// Creates a new in-process engine client.
    ///
    /// # Arguments
    ///
    /// * `consensus_handle` - Handle to reth's consensus engine
    ///   (from `AddOnsContext::beacon_engine_handle`)
    /// * `payload_store` - Store for built payloads
    ///   (from `PayloadStore::new(ctx.node.payload_builder_handle())`)
    /// * `provider` - Provider for querying L2 chain state
    pub fn new(
        consensus_handle: ConsensusEngineHandle<OpEngineTypes>,
        payload_store: PayloadStore<OpEngineTypes>,
        provider: P,
    ) -> Self {
        Self { consensus_handle, payload_store, provider, forkchoice: ForkchoiceTracker::new() }
    }

    /// Creates a new in-process engine client with an existing forkchoice tracker.
    ///
    /// This is useful when you want to share the forkchoice tracker with other components.
    pub const fn with_forkchoice(
        consensus_handle: ConsensusEngineHandle<OpEngineTypes>,
        payload_store: PayloadStore<OpEngineTypes>,
        provider: P,
        forkchoice: ForkchoiceTracker,
    ) -> Self {
        Self { consensus_handle, payload_store, provider, forkchoice }
    }

    /// Returns a reference to the consensus engine handle.
    pub const fn consensus_handle(&self) -> &ConsensusEngineHandle<OpEngineTypes> {
        &self.consensus_handle
    }

    /// Returns a reference to the payload store.
    pub const fn payload_store(&self) -> &PayloadStore<OpEngineTypes> {
        &self.payload_store
    }

    /// Returns a reference to the provider.
    pub const fn provider(&self) -> &P {
        &self.provider
    }

    /// Returns a reference to the forkchoice tracker.
    pub const fn forkchoice(&self) -> &ForkchoiceTracker {
        &self.forkchoice
    }

    /// Consumes the client and returns the inner components.
    pub fn into_parts(
        self,
    ) -> (ConsensusEngineHandle<OpEngineTypes>, PayloadStore<OpEngineTypes>, P, ForkchoiceTracker)
    {
        (self.consensus_handle, self.payload_store, self.provider, self.forkchoice)
    }
}
