//! Traits for configuring a node.

use reth_engine_primitives::{ConsensusEngineEvent, ConsensusEngineHandle};
use reth_node_core::node_config::NodeConfig;
use base_common_runtime_tasks::EventSender;

/// Components and configuration available while launching Base RPC services.
#[derive(Debug, Clone)]
pub struct AddOnsContext<'a> {
    /// Node with all configured components.
    pub node: crate::BaseNodeContext,
    /// Node configuration.
    pub config: &'a NodeConfig,
    /// Handle to the beacon consensus engine.
    pub beacon_engine_handle: ConsensusEngineHandle,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
}
