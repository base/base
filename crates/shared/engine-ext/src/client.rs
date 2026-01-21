//! In-process engine client implementation.

use reth_engine_primitives::ConsensusEngineHandle;
use reth_optimism_node::OpEngineTypes;
use reth_payload_builder::PayloadStore;

/// In-process engine client that bypasses RPC for direct channel communication.
///
/// This client uses reth's internal [`ConsensusEngineHandle`] and [`PayloadStore`]
/// to communicate with the engine, avoiding JSON-RPC serialization overhead.
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
/// │                   InProcessEngineClient                                 │
/// │    - new_payload(), fork_choice_updated() methods                       │
/// │    - get_payload() via PayloadStore                                     │
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
pub struct InProcessEngineClient {
    /// Handle for standard newPayload/FCU calls.
    pub(crate) consensus_handle: ConsensusEngineHandle<OpEngineTypes>,
    /// Store for retrieving built payloads.
    pub(crate) payload_store: PayloadStore<OpEngineTypes>,
}

impl InProcessEngineClient {
    /// Creates a new in-process engine client.
    ///
    /// # Arguments
    ///
    /// * `consensus_handle` - Handle to reth's consensus engine
    ///   (from `AddOnsContext::beacon_engine_handle`)
    /// * `payload_store` - Store for built payloads
    ///   (from `PayloadStore::new(ctx.node.payload_builder_handle())`)
    pub const fn new(
        consensus_handle: ConsensusEngineHandle<OpEngineTypes>,
        payload_store: PayloadStore<OpEngineTypes>,
    ) -> Self {
        Self { consensus_handle, payload_store }
    }

    /// Returns a reference to the consensus engine handle.
    pub const fn consensus_handle(&self) -> &ConsensusEngineHandle<OpEngineTypes> {
        &self.consensus_handle
    }

    /// Returns a reference to the payload store.
    pub const fn payload_store(&self) -> &PayloadStore<OpEngineTypes> {
        &self.payload_store
    }

    /// Consumes the client and returns the inner components.
    pub fn into_parts(self) -> (ConsensusEngineHandle<OpEngineTypes>, PayloadStore<OpEngineTypes>) {
        (self.consensus_handle, self.payload_store)
    }
}
