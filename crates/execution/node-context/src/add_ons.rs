//! Traits for configuring a node.

use std::future::Future;

use alloy_rpc_types_engine::JwtSecret;
use reth_engine_primitives::{ConsensusEngineEvent, ConsensusEngineHandle};
use reth_node_core::node_config::NodeConfig;
use reth_tokio_util::EventSender;

use crate::FullNodeComponents;

/// Context passed to [`NodeAddOns::launch_add_ons`],
#[derive(Debug, Clone)]
pub struct AddOnsContext<'a, N: FullNodeComponents> {
    /// Node with all configured components.
    pub node: N,
    /// Node configuration.
    pub config: &'a NodeConfig,
    /// Handle to the beacon consensus engine.
    pub beacon_engine_handle: ConsensusEngineHandle,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// JWT secret for the node.
    pub jwt_secret: JwtSecret,
}

/// Starts node services and returns their shared handles.
pub trait NodeAddOns<N: FullNodeComponents>: Send {
    /// Handle to add-ons.
    ///
    /// This type is returned by [`launch_add_ons`](Self::launch_add_ons) and represents a
    /// handle to the launched services. It must be `Clone` to allow multiple components to
    /// hold references and should provide methods to interact with the running services.
    ///
    /// For RPC add-ons, this typically includes:
    /// - Server handles to access local addresses and shutdown methods
    /// - RPC module registry for runtime inspection of available methods
    /// - Configured middleware and transport-specific settings
    /// - For Engine API implementations, this also includes handles for consensus layer
    ///   communication
    type Handle: Send + Sync + Clone;

    /// Configures and launches the add-ons.
    ///
    /// This method is called once during node startup after all core components are initialized.
    /// It receives an [`AddOnsContext`] that provides access to:
    ///
    /// - The fully configured node with all its components
    /// - Node configuration for reading settings
    /// - Engine API handles for consensus layer communication
    /// - JWT secrets for setting up authenticated endpoints (if any).
    ///
    /// The implementation should:
    /// 1. Use the context to configure the add-on services
    /// 2. Launch any background tasks using the node's task executor
    /// 3. Return a handle that allows interaction with the launched services
    ///
    /// # Errors
    ///
    /// This method may fail if the add-ons cannot be properly configured or launched,
    /// for example due to port binding issues or invalid configuration.
    fn launch_add_ons(
        self,
        ctx: AddOnsContext<'_, N>,
    ) -> impl Future<Output = eyre::Result<Self::Handle>> + Send;
}

impl<N: FullNodeComponents> NodeAddOns<N> for () {
    type Handle = ();

    async fn launch_add_ons(self, _components: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        Ok(())
    }
}
