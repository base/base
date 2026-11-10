use std::sync::Arc;

use base_proof_preimage::{
    HintReaderServer, PreimageOracleServer, PreimageServerBackend, errors::PreimageOracleError,
};
use tokio::spawn;
use tracing::{Instrument, error, info, info_span};

use crate::HostError;

/// The [`PreimageServer`] is responsible for waiting for incoming preimage requests and
/// serving them to the client.
#[derive(Debug)]
pub struct PreimageServer<P, H, B> {
    oracle_server: P,
    hint_reader: H,
    backend: Arc<B>,
}

impl<P, H, B> PreimageServer<P, H, B>
where
    P: PreimageOracleServer + Send + Sync + 'static,
    H: HintReaderServer + Send + Sync + 'static,
    B: PreimageServerBackend + Send + Sync + 'static,
{
    /// Creates a new [`PreimageServer`].
    pub const fn new(oracle_server: P, hint_reader: H, backend: Arc<B>) -> Self {
        Self { oracle_server, hint_reader, backend }
    }

    /// Starts the [`PreimageServer`] and waits for incoming requests.
    pub async fn start(self) -> Result<(), HostError> {
        let mut server = spawn(
            Self::start_oracle_server(self.oracle_server, Arc::clone(&self.backend))
                .instrument(info_span!("oracle_server_loop")),
        );
        let mut hint_router = spawn(
            Self::start_hint_router(self.hint_reader, Arc::clone(&self.backend))
                .instrument(info_span!("hint_router_loop")),
        );

        let (finished, sibling) = tokio::select! {
            s = &mut server => (s, hint_router),
            h = &mut hint_router => (h, server),
        };
        sibling.abort();
        // The sibling's outcome is irrelevant once the first task has finished; awaiting it only
        // ensures it no longer holds the channels or the backend when `start` returns.
        let _ = sibling.await;

        finished.map_err(|e| HostError::Custom(e.to_string()))?
    }

    async fn start_oracle_server(oracle_server: P, backend: Arc<B>) -> Result<(), HostError> {
        info!(target: "host_server", "starting oracle server");
        loop {
            match oracle_server.next_preimage_request(backend.as_ref()).await {
                Ok(_) => {}
                Err(PreimageOracleError::IOError(_)) => return Ok(()),
                Err(e) => {
                    error!(target: "host_server", error = %e, "failed to serve preimage request");
                    return Err(HostError::PreimageRequestFailed(e));
                }
            }
        }
    }

    async fn start_hint_router(hint_reader: H, backend: Arc<B>) -> Result<(), HostError> {
        info!(target: "host_server", "starting hint router");
        loop {
            match hint_reader.next_hint(backend.as_ref()).await {
                Ok(_) => {}
                Err(PreimageOracleError::IOError(_)) => return Ok(()),
                Err(e) => {
                    error!(target: "host_server", error = %e, "failed to route hint");
                    return Err(HostError::RouteHintFailed(e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use base_proof_preimage::{
        HintRouter, PreimageFetcher, PreimageKey,
        errors::{ChannelError, PreimageOracleResult},
    };
    use tokio::sync::oneshot;

    use super::*;

    /// Oracle server whose client has already disconnected.
    struct DisconnectedOracleServer;

    #[async_trait]
    impl PreimageOracleServer for DisconnectedOracleServer {
        async fn next_preimage_request<F>(&self, _: &F) -> PreimageOracleResult<()>
        where
            F: PreimageFetcher + Send + Sync,
        {
            Err(PreimageOracleError::IOError(ChannelError::Closed))
        }
    }

    /// Hint reader that never receives a hint and reports when it is dropped.
    struct IdleHintReader {
        _dropped: oneshot::Sender<()>,
    }

    #[async_trait]
    impl HintReaderServer for IdleHintReader {
        async fn next_hint<R>(&self, _: &R) -> PreimageOracleResult<()>
        where
            R: HintRouter + Send + Sync,
        {
            std::future::pending().await
        }
    }

    struct UnusedBackend;

    #[async_trait]
    impl PreimageFetcher for UnusedBackend {
        async fn get_preimage(&self, _: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            Err(PreimageOracleError::KeyNotFound)
        }
    }

    #[async_trait]
    impl HintRouter for UnusedBackend {
        async fn route_hint(&self, _: String) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_stops_hint_router_when_oracle_server_finishes() {
        let (dropped_tx, mut dropped_rx) = oneshot::channel();
        let server = PreimageServer::new(
            DisconnectedOracleServer,
            IdleHintReader { _dropped: dropped_tx },
            Arc::new(UnusedBackend),
        );

        server.start().await.expect("oracle server disconnect is a clean shutdown");

        assert_eq!(dropped_rx.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    }
}
