//! Enclave side of the Enclave link: answers one request per connection.

use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};
use tracing::info;

use crate::{
    EnclaveFailure, EnclaveRequest, EnclaveResponse, Frame, Server, TransportError, TransportResult,
};

/// Deadline for receiving a complete request frame from the host.
///
/// Must be generous enough to cover large [`EnclaveRequest::Prove`] payloads,
/// which include the full preimage bundle and can be many megabytes over vsock.
/// A single timeout applies to all request types because the request type is
/// unknown until the frame has been fully read.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Answers Enclave link requests with one enclave [`Server`].
#[derive(Debug, Clone)]
pub struct EnclaveHandler {
    server: Arc<Server>,
}

impl EnclaveHandler {
    /// Create a handler backed by the given server.
    pub const fn new(server: Arc<Server>) -> Self {
        Self { server }
    }

    /// Reads one request from `stream`, runs it, and writes the response.
    pub async fn serve(
        &self,
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
    ) -> TransportResult<()> {
        let request: EnclaveRequest = timeout(REQUEST_READ_TIMEOUT, Frame::read(&mut stream))
            .await
            .map_err(|_| TransportError::Timeout)??;
        let response = self.respond(request).await;
        Frame::write(&mut stream, &response).await
    }

    async fn respond(&self, request: EnclaveRequest) -> EnclaveResponse {
        if let Err(failure) = request.validate() {
            return EnclaveResponse::Failure(failure);
        }

        match request {
            EnclaveRequest::Prove(preimages) => {
                let preimage_count = preimages.len();
                let total_value_bytes: usize = preimages.iter().map(|(_, v)| v.len()).sum();
                info!(
                    preimage_count = preimage_count,
                    total_value_bytes = total_value_bytes,
                    "received prove request"
                );
                match Box::pin(self.server.prove(preimages)).await {
                    Ok(result) => EnclaveResponse::Prove(Box::new(result)),
                    Err(e) => EnclaveResponse::Failure(EnclaveFailure::Proof(e.to_string())),
                }
            }
            EnclaveRequest::SignerPublicKey => {
                info!("received signer public key request");
                EnclaveResponse::SignerPublicKey(self.server.signer_public_key())
            }
            EnclaveRequest::SignerAttestation { user_data, nonce } => {
                info!(
                    has_nonce = nonce.is_some(),
                    has_user_data = user_data.is_some(),
                    "received signer attestation request"
                );
                // NSM calls are blocking FFI. `spawn_blocking` keeps them off the async
                // executor on both multi-thread and current-thread runtimes.
                let server = Arc::clone(&self.server);
                match tokio::task::spawn_blocking(move || {
                    server.signer_attestation(user_data, nonce)
                })
                .await
                {
                    Ok(Ok(doc)) => EnclaveResponse::SignerAttestation(doc),
                    Ok(Err(e)) => {
                        EnclaveResponse::Failure(EnclaveFailure::Attestation(e.to_string()))
                    }
                    Err(e) => EnclaveResponse::Failure(EnclaveFailure::Attestation(format!(
                        "attestation task failed: {e}"
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    async fn round_trip(request: EnclaveRequest) -> EnclaveResponse {
        let handler = EnclaveHandler::new(Arc::new(Server::new_local().unwrap()));
        let (mut host, enclave) = duplex(64 * 1024);
        let serve = tokio::spawn(async move { handler.serve(enclave).await });
        Frame::write(&mut host, &request).await.unwrap();
        let response = Frame::read(&mut host).await.unwrap();
        serve.await.unwrap().unwrap();
        response
    }

    #[tokio::test]
    async fn oversized_nonce_is_rejected_as_invalid_request() {
        let response = round_trip(EnclaveRequest::SignerAttestation {
            user_data: None,
            nonce: Some(vec![0; EnclaveRequest::MAX_NONCE_BYTES + 1]),
        })
        .await;
        assert!(matches!(response, EnclaveResponse::Failure(EnclaveFailure::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn nonce_at_limit_reaches_nsm() {
        // Outside a Nitro enclave the NSM is unavailable, so a request that passes
        // the limit check fails at attestation instead.
        let response = round_trip(EnclaveRequest::SignerAttestation {
            user_data: Some(vec![0; EnclaveRequest::MAX_USER_DATA_BYTES]),
            nonce: Some(vec![0; EnclaveRequest::MAX_NONCE_BYTES]),
        })
        .await;
        assert!(matches!(response, EnclaveResponse::Failure(EnclaveFailure::Attestation(_))));
    }
}
