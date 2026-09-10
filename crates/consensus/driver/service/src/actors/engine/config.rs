use crate::LocalEngineClient;

use crate::NodeMode;

/// Runtime dependencies for the execution actor.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// In-process execution commands and local chain state.
    pub client: LocalEngineClient,
    /// Whether this node also builds sequencer blocks.
    pub mode: NodeMode,
}
