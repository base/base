//! Engine clients for inter-actor communication.

use kona_node_service::{
    EngineActorRequest, QueuedDerivationEngineClient, QueuedNetworkEngineClient,
};
use tokio::sync::mpsc;

/// Clients for communicating with the engine actor from other actors.
///
/// These are returned by [`UnifiedRollupNode::start_with_clients`] and can be used
/// to wire up kona's other actors (DerivationActor, NetworkActor, etc.).
#[derive(Debug)]
pub struct EngineClients {
    /// Client for derivation actor to send consolidation signals.
    pub derivation_client: QueuedDerivationEngineClient,
    /// Client for network actor to send unsafe blocks.
    pub network_client: QueuedNetworkEngineClient,
    /// The raw engine channel sender for custom use cases.
    pub engine_tx: mpsc::Sender<EngineActorRequest>,
}
