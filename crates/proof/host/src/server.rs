use std::sync::Arc;

use base_proof_preimage::{
    HintReaderServer, PreimageOracleServer, PreimageServerBackend, errors::PreimageOracleError,
};
use tokio::task::JoinSet;
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
        // Dropping the set, including when `start` itself is cancelled, aborts both loops.
        let mut tasks = JoinSet::new();
        tasks.spawn(
            Self::start_oracle_server(self.oracle_server, Arc::clone(&self.backend))
                .instrument(info_span!("oracle_server_loop")),
        );
        tasks.spawn(
            Self::start_hint_router(self.hint_reader, Arc::clone(&self.backend))
                .instrument(info_span!("hint_router_loop")),
        );

        let finished = tasks.join_next().await.expect("two server loops were spawned");
        // Stop the other loop so it no longer holds the channels or the backend on return.
        tasks.shutdown().await;

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

    /// Oracle server whose client never sends a request.
    struct IdleOracleServer;

    #[async_trait]
    impl PreimageOracleServer for IdleOracleServer {
        async fn next_preimage_request<F>(&self, _: &F) -> PreimageOracleResult<()>
        where
            F: PreimageFetcher + Send + Sync,
        {
            std::future::pending().await
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

    #[tokio::test]
    async fn cancelling_start_stops_both_server_loops() {
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let server = PreimageServer::new(
            IdleOracleServer,
            IdleHintReader { _dropped: dropped_tx },
            Arc::new(UnusedBackend),
        );
        let start = tokio::spawn(server.start());
        tokio::task::yield_now().await;

        start.abort();
        let _ = start.await;

        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx).await;
        assert!(matches!(closed, Ok(Err(_))), "the hint loop must stop when start is cancelled");
    }
}
