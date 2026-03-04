use std::sync::Arc;

use base_proof_preimage::{
    HintReaderServer, PreimageOracleServer, PreimageServerBackend, errors::PreimageOracleError,
};
use tokio::spawn;
use tracing::{error, info};

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
        let server =
            spawn(Self::start_oracle_server(self.oracle_server, Arc::clone(&self.backend)));
        let hint_router =
            spawn(Self::start_hint_router(self.hint_reader, Arc::clone(&self.backend)));

        tokio::select! {
            s = server => s.map_err(|e| HostError::Custom(e.to_string()))?,
            h = hint_router => h.map_err(|e| HostError::Custom(e.to_string()))?,
        }
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
