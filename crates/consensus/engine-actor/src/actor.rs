//! Direct engine actor implementing kona's `NodeActor` trait.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use base_engine_ext::DirectEngineApi;
use kona_genesis::RollupConfig;
use kona_node_service::NodeActor;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{DirectEngineProcessor, EngineActorError, EngineActorRequest, EngineSyncState};

/// Direct engine actor that communicates with reth via in-process channels.
///
/// This actor implements kona's `NodeActor` trait and processes engine requests
/// from the derivation pipeline and sync actors.
///
/// # Type Parameters
///
/// * `E` - The engine API implementation, must implement [`DirectEngineApi`]
pub struct DirectEngineActor<E: DirectEngineApi> {
    /// The engine client implementing DirectEngineApi.
    client: Arc<E>,
    /// The request receiver.
    rx: mpsc::Receiver<EngineActorRequest>,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
    /// Optional pre-initialized sync state.
    sync_state: Option<EngineSyncState>,
}

impl<E: DirectEngineApi> fmt::Debug for DirectEngineActor<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectEngineActor").field("sync_state", &self.sync_state).finish()
    }
}

impl<E: DirectEngineApi> DirectEngineActor<E> {
    /// Creates a new direct engine actor.
    pub const fn new(
        client: Arc<E>,
        rx: mpsc::Receiver<EngineActorRequest>,
        cancel: CancellationToken,
    ) -> Self {
        Self { client, rx, cancel, sync_state: None }
    }

    /// Creates a new direct engine actor with an existing sync state.
    pub const fn with_sync_state(
        client: Arc<E>,
        rx: mpsc::Receiver<EngineActorRequest>,
        cancel: CancellationToken,
        sync_state: EngineSyncState,
    ) -> Self {
        Self { client, rx, cancel, sync_state: Some(sync_state) }
    }

    /// Creates a new `DirectEngineActor` with all components.
    ///
    /// This is a convenience constructor that matches the API from `rf/unified-spike`.
    pub fn with_components(
        cancellation_token: CancellationToken,
        inbound_rx: mpsc::Receiver<EngineActorRequest>,
        client: Arc<E>,
        _rollup: Arc<RollupConfig>,
    ) -> Self {
        Self::new(client, inbound_rx, cancellation_token)
    }
}

#[async_trait]
impl<E> NodeActor for DirectEngineActor<E>
where
    E: DirectEngineApi + std::fmt::Debug + 'static,
{
    type Error = EngineActorError;
    type StartData = Arc<RollupConfig>;

    async fn start(self, config: Self::StartData) -> Result<(), Self::Error> {
        info!(
            chain_id = ?config.l2_chain_id,
            "Starting DirectEngineActor"
        );

        let processor = if let Some(sync_state) = self.sync_state {
            DirectEngineProcessor::with_sync_state(self.client, self.rx, self.cancel, sync_state)
        } else {
            DirectEngineProcessor::new(self.client, self.rx, self.cancel)
        };

        processor.run().await
    }
}
