//! Unified rollup node service.
//!
//! This module provides [`UnifiedRollupNode`] which orchestrates kona's actors
//! (NetworkActor, DerivationActor, L1WatcherActor) with our custom engine actor
//! using in-process channel-based communication.
//!
//! # Actor Wiring Architecture
//!
//! The rollup node uses kona's actor model with these key components:
//!
//! - **DirectEngineActor**: Our custom engine actor that uses [`DirectEngineApi`]
//!   for in-process communication with reth's engine via `ConsensusEngineHandle`.
//!
//! ```text
//! ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
//! │ DerivationActor │     │ NetworkActor    │     │ L1WatcherActor  │
//! │ (derives L2     │     │ (P2P gossip)    │     │ (L1 finality)   │
//! │  from L1 data)  │     │                 │     │                 │
//! └────────┬────────┘     └────────┬────────┘     └────────┬────────┘
//!          │                       │                       │
//!          │                       │                       │
//!          └───────────┬───────────┴───────────┬───────────┘
//!                      ▼                       ▼
//!              mpsc::Sender<EngineActorRequest>
//!                      │
//!                      ▼
//!          ┌─────────────────────────────────────────┐
//!          │ DirectEngineActor                       │
//!          │ └─ InProcessEngineClient                │
//!          │    └─ ConsensusEngineHandle (channel)   │
//!          └─────────────────────────────────────────┘
//! ```

use std::sync::Arc;

use base_engine_actor::{DirectEngineActor, DirectEngineApi, EngineActorRequest};
use kona_genesis::RollupConfig;
use kona_node_service::NodeActor;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{EngineClients, NodeHandle, UnifiedRollupNodeBuilder, UnifiedRollupNodeError};

/// A unified rollup node that orchestrates kona's actors with our custom engine.
///
/// This node combines:
/// - [`DirectEngineActor`] - custom engine actor using [`DirectEngineApi`] for
///   in-process EL communication via reth's `ConsensusEngineHandle`.
///
/// Additional actors from kona can be wired up using the engine clients returned
/// by [`Self::start_with_clients`]:
/// - [`NetworkActor`][kona_node_service::NetworkActor] - kona's P2P networking actor
/// - [`DerivationActor`][kona_node_service::DerivationActor] - kona's derivation pipeline actor
/// - [`L1WatcherActor`][kona_node_service::L1WatcherActor] - kona's L1 chain watcher actor
#[derive(Debug)]
pub struct UnifiedRollupNode<E: DirectEngineApi> {
    /// The rollup configuration.
    config: Arc<RollupConfig>,
    /// The engine driver for direct EL communication.
    engine_driver: Arc<E>,
    /// Channel buffer size for inter-actor communication.
    channel_buffer_size: usize,
}

impl<E: DirectEngineApi + std::fmt::Debug + 'static> UnifiedRollupNode<E> {
    /// Creates a new [`UnifiedRollupNodeBuilder`].
    pub const fn builder() -> UnifiedRollupNodeBuilder<E> {
        UnifiedRollupNodeBuilder::new()
    }

    /// Creates a new unified rollup node.
    pub const fn new(config: Arc<RollupConfig>, engine_driver: Arc<E>) -> Self {
        Self { config, engine_driver, channel_buffer_size: 256 }
    }

    /// Creates a new unified rollup node with a custom buffer size.
    pub const fn new_with_buffer_size(
        config: Arc<RollupConfig>,
        engine_driver: Arc<E>,
        channel_buffer_size: usize,
    ) -> Self {
        Self { config, engine_driver, channel_buffer_size }
    }

    /// Returns a reference to the rollup configuration.
    pub const fn config(&self) -> &Arc<RollupConfig> {
        &self.config
    }

    /// Returns a reference to the engine driver.
    pub const fn engine_driver(&self) -> &Arc<E> {
        &self.engine_driver
    }

    /// Starts the unified rollup node and blocks until shutdown.
    ///
    /// This method:
    /// 1. Creates the cancellation token for coordinated shutdown
    /// 2. Creates mpsc channels for inter-actor communication
    /// 3. Spawns the engine actor using [`DirectEngineActor`]
    /// 4. Sets up signal handling for graceful shutdown
    /// 5. Waits for the engine actor to complete or handles shutdown
    ///
    /// This is a convenience method that doesn't return engine clients.
    /// Use [`Self::start_with_clients`] if you need to wire up additional actors.
    pub async fn start(self) -> Result<(), UnifiedRollupNodeError> {
        let (_, handle) = self.start_with_clients().await?;
        handle.wait().await
    }

    /// Starts the unified rollup node and returns engine clients for wiring actors.
    ///
    /// This method starts the engine actor and returns [`EngineClients`] that can be
    /// used to wire up kona's other actors (DerivationActor, NetworkActor, etc.).
    ///
    /// # Returns
    ///
    /// A tuple of:
    /// - [`EngineClients`]: Clients for communicating with the engine actor
    /// - [`NodeHandle`]: A handle to wait for the node to complete
    ///
    /// # Example
    ///
    /// ```ignore
    /// let node = UnifiedRollupNode::builder()
    ///     .config(rollup_config)
    ///     .engine_driver(engine_driver)
    ///     .build();
    ///
    /// let (clients, handle) = node.start_with_clients().await?;
    ///
    /// // Use clients.engine_tx to send engine requests
    ///
    /// // Wait for shutdown
    /// handle.await?;
    /// ```
    pub async fn start_with_clients(
        self,
    ) -> Result<(EngineClients, NodeHandle), UnifiedRollupNodeError> {
        info!(
            chain_id = %self.config.l2_chain_id,
            "Starting unified rollup node"
        );

        // Create the cancellation token for coordinated shutdown
        let cancellation_token = CancellationToken::new();

        // Create channels for engine actor communication
        let (engine_tx, engine_rx) = mpsc::channel::<EngineActorRequest>(self.channel_buffer_size);

        // Create and spawn the engine actor
        let engine_actor = DirectEngineActor::with_components(
            cancellation_token.clone(),
            engine_rx,
            Arc::clone(&self.engine_driver),
            Arc::clone(&self.config),
        );

        let engine_cancellation = cancellation_token.clone();
        let config_for_actor = Arc::clone(&self.config);
        let engine_handle = tokio::spawn(async move {
            engine_actor
                .start(config_for_actor)
                .await
                .map_err(|e| UnifiedRollupNodeError::EngineActorFailed(e.to_string()))
        });

        // Set up signal handling for graceful shutdown (consume original token)
        let shutdown_token = cancellation_token;
        let shutdown_handle = tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};

                let mut sigint =
                    signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");

                tokio::select! {
                    _ = sigint.recv() => {
                        info!("Received SIGINT, initiating shutdown");
                    }
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM, initiating shutdown");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
                info!("Received Ctrl+C, initiating shutdown");
            }

            shutdown_token.cancel();
        });

        let clients = EngineClients { engine_tx };

        let handle = NodeHandle::new(engine_handle, shutdown_handle, engine_cancellation);

        info!("Unified rollup node started, engine clients available for actor wiring");
        Ok((clients, handle))
    }
}
