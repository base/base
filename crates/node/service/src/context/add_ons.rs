//! Traits for configuring a node.

use base_common_runtime_tasks::EventSender;
use base_execution_engine_types::ConsensusEngineEvent;
use base_node_config::NodeConfig;

/// Components and configuration available while launching Base RPC services.
#[derive(Debug, Clone)]
pub struct AddOnsContext<'a> {
    /// Node with all configured components.
    pub node: crate::context::BaseNodeContext,
    /// Node configuration.
    pub config: &'a NodeConfig,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
}
