//! Unified node components for CL+EL integration.
//!
//! This module provides types for in-process communication between the consensus layer (kona)
//! and execution layer (reth) in the unified binary.
//!
//! # Architecture
//!
//! The unified binary runs both layers in a single process, using direct channel communication
//! instead of HTTP/IPC Engine API calls:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Unified Binary                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  1. Launch reth EL via BaseNodeRunner                                   │
//! │  2. Extract ConsensusEngineHandle + PayloadStore from FullNode          │
//! │  3. Create InProcessEngineDriver with extracted components              │
//! │  4. Start UnifiedRollupNode with the driver                             │
//! └─────────────────────────────────────────────────────────────────────────┘
//!                                    │
//!         ┌───────────────────────────┴───────────────────────────┐
//!         ▼                                                       ▼
//! ┌────────────────────────┐                         ┌────────────────────────┐
//! │  reth Execution Layer  │◄────────────────────────│  kona Consensus Layer  │
//! │  ───────────────────── │   InProcessEngineDriver │  ────────────────────  │
//! │  • ConsensusEngineHandle│   (channel-based)      │  • DirectEngineActor   │
//! │  • PayloadStore        │                         │  • DerivationActor     │
//! │  • Provider            │                         │  • NetworkActor        │
//! └────────────────────────┘                         └────────────────────────┘
//! ```
//!
//! # Implementation
//!
//! The `UnifiedExtension` uses the `add_node_started_hook` to extract engine components
//! from the fully launched node. This provides access to concrete types rather than generics,
//! avoiding type mismatch issues.

use std::sync::Arc;

use base_engine_bridge::{ConsensusEngineHandle, OpEngineTypes, PayloadStore};
use tokio::sync::oneshot;
use tracing::info;

use crate::{BaseBuilder, BaseNodeExtension, FromExtensionConfig, OpProvider};

/// Engine components needed for in-process CL+EL communication.
///
/// These components are extracted from the launched reth node and used to create
/// an [`InProcessEngineDriver`][base_engine_bridge::InProcessEngineDriver].
///
/// # Fields
///
/// - `engine_handle`: Channel-based handle to reth's consensus engine, allowing direct
///   forkchoice updates and payload operations without HTTP serialization.
/// - `payload_store`: Store for retrieving payloads built by reth's payload builder.
/// - `provider`: Blockchain provider for querying headers, blocks, and chain state.
///
/// # Example
///
/// ```ignore
/// use base_engine_bridge::InProcessEngineDriver;
///
/// let driver = InProcessEngineDriver::new(
///     components.engine_handle,
///     components.payload_store,
///     components.provider,
/// );
/// ```
#[derive(Debug)]
pub struct EngineComponents {
    /// Handle to communicate with reth's consensus engine.
    pub engine_handle: ConsensusEngineHandle<OpEngineTypes>,
    /// Store for retrieving built payloads.
    pub payload_store: PayloadStore<OpEngineTypes>,
    /// The blockchain provider for querying chain state.
    pub provider: Arc<OpProvider>,
}

impl EngineComponents {
    /// Creates new engine components.
    pub const fn new(
        engine_handle: ConsensusEngineHandle<OpEngineTypes>,
        payload_store: PayloadStore<OpEngineTypes>,
        provider: Arc<OpProvider>,
    ) -> Self {
        Self { engine_handle, payload_store, provider }
    }
}

/// Configuration for the unified binary extension.
///
/// This configuration provides a oneshot sender that will receive the engine components
/// extracted from the launched reth node.
#[derive(Debug)]
pub struct UnifiedConfig {
    /// Sender for engine components. The components will be sent when the node starts.
    pub engine_components_tx: oneshot::Sender<EngineComponents>,
}

/// Extension for the unified binary that enables in-process CL+EL communication.
///
/// When installed, this extension uses the `add_node_started_hook` to extract engine
/// components from the fully launched node. This approach provides access to concrete
/// types from `FullNode`, avoiding generic type mismatch issues.
///
/// # Example
///
/// ```ignore
/// use base_client_node::{BaseNodeRunner, UnifiedConfig, UnifiedExtension};
/// use tokio::sync::oneshot;
///
/// let (engine_tx, engine_rx) = oneshot::channel();
///
/// let mut runner = BaseNodeRunner::new(rollup_args);
/// runner.install_ext::<UnifiedExtension>(UnifiedConfig {
///     engine_components_tx: engine_tx,
/// });
///
/// // In a spawned task:
/// let components = engine_rx.await?;
/// let driver = InProcessEngineDriver::new(
///     components.engine_handle,
///     components.payload_store,
///     components.provider,
/// );
/// ```
#[derive(Debug)]
pub struct UnifiedExtension {
    config: UnifiedConfig,
}

impl BaseNodeExtension for UnifiedExtension {
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        let tx = self.config.engine_components_tx;

        // Use node_started_hook to extract engine components from the fully launched node.
        // This gives us access to concrete types (OpFullNode) rather than generics.
        builder.add_node_started_hook(move |full_node| {
            info!(target: "base::unified", "Extracting engine components from launched node");

            // Extract components from the full node:
            // - beacon_engine_handle from add_ons_handle (RpcHandle)
            // - payload_builder_handle from full_node
            // - provider from full_node
            let engine_handle = full_node.add_ons_handle.beacon_engine_handle.clone();
            let payload_store = PayloadStore::new(full_node.payload_builder_handle.clone());
            let provider = Arc::new(full_node.provider);

            let components = EngineComponents::new(engine_handle, payload_store, provider);

            // Send to receiver (ignore error if receiver dropped)
            let _ = tx.send(components);

            info!(target: "base::unified", "Engine components sent to unified binary");
            Ok(())
        })
    }
}

impl FromExtensionConfig for UnifiedExtension {
    type Config = UnifiedConfig;

    fn from_config(config: Self::Config) -> Self {
        Self { config }
    }
}
