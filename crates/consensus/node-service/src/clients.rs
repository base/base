//! Engine clients for inter-actor communication.

use base_engine_actor::EngineActorRequest;
use tokio::sync::mpsc;

/// Clients for communicating with the engine actor from other actors.
///
/// These are returned by [`UnifiedRollupNode::start_with_clients`][crate::UnifiedRollupNode::start_with_clients]
/// and can be used to wire up custom actors.
#[derive(Debug)]
pub struct EngineClients {
    /// The raw engine channel sender for sending engine requests.
    pub engine_tx: mpsc::Sender<EngineActorRequest>,
}
