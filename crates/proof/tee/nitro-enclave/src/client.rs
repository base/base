//! Host side of the Enclave link.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use k256::ecdsa::VerifyingKey;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, duplex},
    time::timeout,
};
#[cfg(target_os = "linux")]
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{info, warn};

use crate::{
    EnclaveFailure, EnclaveHandler, EnclaveRequest, EnclaveResponse, Frame, Server, TransportError,
};

/// Errors returned by [`EnclaveClient`].
#[derive(Debug, Error)]
pub enum EnclaveLinkError {
    /// Opening the vsock connection timed out.
    #[error("connect timed out")]
    ConnectTimeout,
    /// Opening the vsock connection failed.
    #[error("connect failed: {0}")]
    ConnectFailed(std::io::Error),
    /// The enclave did not answer within the operation's deadline.
    #[error("{operation} timed out")]
    Timeout {
        /// The operation that timed out.
        operation: &'static str,
    },
    /// Writing the request or reading the response frame failed.
    #[error("frame error: {0}")]
    Frame(#[from] TransportError),
    /// The enclave answered with a response for a different operation.
    #[error("unexpected response for {expected}")]
    UnexpectedResponse {
        /// The operation that was sent.
        expected: &'static str,
    },
    /// The enclave returned a malformed signer public key.
    #[error("invalid signer public key: expected 65-byte uncompressed SEC1 key")]
    InvalidSignerKey,
    /// The enclave refused or failed the request.
    #[error("enclave rejected request: {0}")]
    Enclave(#[from] EnclaveFailure),
}

/// Where an [`EnclaveClient`] reaches its enclave.
#[derive(Debug, Clone)]
pub enum EnclaveConnector {
    /// A Nitro Enclave reached over vsock.
    #[cfg(target_os = "linux")]
    Vsock {
        /// Enclave context id.
        cid: u32,
        /// Enclave vsock port.
        port: u32,
    },
    /// An enclave [`Server`] in this process, reached over an in-memory stream.
    InProcess(Arc<Server>),
}

/// Client for one enclave over the Enclave link.
///
/// Every request opens a fresh connection, so in-process mode exercises the same
/// framing, checks, and error mapping as vsock.
#[derive(Debug, Clone)]
pub struct EnclaveClient {
    connector: EnclaveConnector,
}

impl EnclaveClient {
    /// Deadline for opening a vsock connection.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Deadline for a signer key or attestation response. Proving has no response
    /// deadline; callers bound it through cancellation.
    pub const SIGNER_TIMEOUT: Duration = Duration::from_secs(10);

    /// In-memory stream buffer; larger than one throttled frame write.
    const IN_PROCESS_BUFFER_BYTES: usize = 64 * 1024;

    /// Create a client for a Nitro Enclave at the given vsock endpoint.
    #[cfg(target_os = "linux")]
    pub const fn vsock(cid: u32, port: u32) -> Self {
        Self { connector: EnclaveConnector::Vsock { cid, port } }
    }

    /// Create a client for an enclave server running in this process.
    pub const fn in_process(server: Arc<Server>) -> Self {
        Self { connector: EnclaveConnector::InProcess(server) }
    }

    /// Send witness preimages to the enclave and return the signed proof.
    ///
    /// Dropping the future stops the in-process handler at its next await point.
    pub async fn prove(
        &self,
        preimages: Vec<(PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult, EnclaveLinkError> {
        let (cid, port) = match &self.connector {
            #[cfg(target_os = "linux")]
            EnclaveConnector::Vsock { cid, port } => (Some(*cid), Some(*port)),
            EnclaveConnector::InProcess(_) => (None::<u32>, None::<u32>),
        };
        info!(
            preimage_count = preimages.len(),
            cid,
            port,
            in_process = matches!(self.connector, EnclaveConnector::InProcess(_)),
            total_value_bytes = preimages.iter().map(|(_, v)| v.len()).sum::<usize>(),
            "sending prove request to enclave"
        );
        match self.call(EnclaveRequest::Prove(preimages), None).await? {
            EnclaveResponse::Prove(result) => Ok(*result),
            _ => Err(EnclaveLinkError::UnexpectedResponse { expected: "prove" }),
        }
    }

    /// Return the 65-byte uncompressed ECDSA public key of the enclave signer.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>, EnclaveLinkError> {
        match self.call(EnclaveRequest::SignerPublicKey, Some(Self::SIGNER_TIMEOUT)).await? {
            EnclaveResponse::SignerPublicKey(key) if key.len() == 65 && key[0] == 0x04 => Ok(key),
            EnclaveResponse::SignerPublicKey(_) => Err(EnclaveLinkError::InvalidSignerKey),
            _ => Err(EnclaveLinkError::UnexpectedResponse { expected: "signer_public_key" }),
        }
    }

    /// Return the Ethereum address of the enclave signer.
    pub async fn signer_address(&self) -> Result<Address, EnclaveLinkError> {
        let key = self.signer_public_key().await?;
        let key =
            VerifyingKey::from_sec1_bytes(&key).map_err(|_| EnclaveLinkError::InvalidSignerKey)?;
        Ok(public_key_to_address(&key))
    }

    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes) for the enclave signer.
    pub async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, EnclaveLinkError> {
        let request = EnclaveRequest::SignerAttestation { user_data, nonce };
        match self.call(request, Some(Self::SIGNER_TIMEOUT)).await? {
            EnclaveResponse::SignerAttestation(doc) => Ok(doc),
            _ => Err(EnclaveLinkError::UnexpectedResponse { expected: "signer_attestation" }),
        }
    }

    /// Sends one request over `stream` and returns the enclave's answer.
    ///
    /// Rejects requests that break enclave limits before sending, and maps both
    /// typed and legacy untyped enclave failures to [`EnclaveLinkError::Enclave`].
    /// Drops the request after writing, before waiting for the response.
    pub async fn exchange(
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
        request: EnclaveRequest,
        response_timeout: Option<Duration>,
    ) -> Result<EnclaveResponse, EnclaveLinkError> {
        request.validate()?;
        let operation = request.operation();
        let is_prove = matches!(request, EnclaveRequest::Prove(_));
        Frame::write(&mut stream, &request).await?;
        drop(request);

        let read = Frame::read(&mut stream);
        let response = match response_timeout {
            Some(limit) => timeout(limit, read)
                .await
                .map_err(|_| EnclaveLinkError::Timeout { operation })??,
            None => read.await?,
        };

        match response {
            EnclaveResponse::Failure(failure) => Err(failure.into()),
            EnclaveResponse::Error(message) => Err(if is_prove {
                EnclaveFailure::Proof(message)
            } else {
                EnclaveFailure::Attestation(message)
            }
            .into()),
            response => Ok(response),
        }
    }

    async fn call(
        &self,
        request: EnclaveRequest,
        response_timeout: Option<Duration>,
    ) -> Result<EnclaveResponse, EnclaveLinkError> {
        request.validate()?;
        match &self.connector {
            #[cfg(target_os = "linux")]
            EnclaveConnector::Vsock { cid, port } => {
                let stream = timeout(
                    Self::CONNECT_TIMEOUT,
                    VsockStream::connect(VsockAddr::new(*cid, *port)),
                )
                .await
                .map_err(|_| EnclaveLinkError::ConnectTimeout)?
                .map_err(EnclaveLinkError::ConnectFailed)?;
                Self::exchange(stream, request, response_timeout).await
            }
            EnclaveConnector::InProcess(server) => {
                let (stream, enclave_end) = duplex(Self::IN_PROCESS_BUFFER_BYTES);
                let handler = EnclaveHandler::new(Arc::clone(server));
                let exchange = Self::exchange(stream, request, response_timeout);
                tokio::pin!(exchange);
                tokio::select! {
                    result = &mut exchange => result,
                    result = handler.serve(enclave_end) => {
                        if let Err(error) = result {
                            warn!(error = %error, "in-process enclave connection failed");
                        }
                        exchange.await
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future, poll_fn},
        io::{self, Write},
        sync::Mutex,
        task::Poll,
    };

    use base_proof_preimage::PreimageKeyType;
    use tokio::io::{AsyncReadExt, duplex};

    use super::*;

    #[derive(Debug, Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn prove_log_identifies_enclave() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        #[cfg(target_os = "linux")]
        let client = EnclaveClient::vsock(16, 9000);
        #[cfg(not(target_os = "linux"))]
        let client = client();

        tracing::subscriber::with_default(subscriber, || {
            let mut prove = Box::pin(client.prove(Vec::new()));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = prove.as_mut().poll(&mut context);
        });

        let log = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let log =
            log.lines().find(|line| line.contains("sending prove request to enclave")).unwrap();
        #[cfg(target_os = "linux")]
        {
            assert!(log.contains("cid=16"), "{log}");
            assert!(log.contains("port=9000"), "{log}");
        }
        #[cfg(not(target_os = "linux"))]
        assert!(log.contains("in_process=true"), "{log}");
        assert!(log.contains("preimage_count=0"), "{log}");
        assert!(log.contains("total_value_bytes=0"), "{log}");
    }

    fn client() -> EnclaveClient {
        EnclaveClient::in_process(Arc::new(Server::new_local().unwrap()))
    }

    #[tokio::test]
    async fn in_process_signer_address_matches_server() {
        let server = Arc::new(Server::new_local().unwrap());
        let client = EnclaveClient::in_process(Arc::clone(&server));
        assert_eq!(client.signer_address().await.unwrap(), server.signer_address());
    }

    #[tokio::test]
    async fn empty_witness_is_a_proof_rejection() {
        let err = client().prove(Vec::new()).await.unwrap_err();
        assert!(matches!(err, EnclaveLinkError::Enclave(EnclaveFailure::Proof(_))), "{err:?}");
    }

    #[tokio::test]
    async fn dropping_prove_stops_in_process_handler() {
        let server = Arc::new(Server::new_local().unwrap());
        let weak_server = Arc::downgrade(&server);
        let client = EnclaveClient::in_process(Arc::clone(&server));
        let key = PreimageKey::new([1; 32], PreimageKeyType::Local);
        drop(server);
        let mut prove = Box::pin(async move {
            client.prove(vec![(key, vec![0; 2 * EnclaveClient::IN_PROCESS_BUFFER_BYTES])]).await
        });

        let state = poll_fn(|cx| Poll::Ready(prove.as_mut().poll(cx))).await;
        assert!(state.is_pending(), "request must still be in flight");
        assert!(weak_server.upgrade().is_some(), "in-flight calls must keep the server alive");
        drop(prove);
        assert!(weak_server.upgrade().is_none(), "cancelled calls must release the server");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn invalid_attestation_is_rejected_before_connecting_to_unreachable_enclave() {
        let client = EnclaveClient::vsock(u32::MAX, u32::MAX);
        for (user_data, nonce) in [
            (Some(vec![0; EnclaveRequest::MAX_USER_DATA_BYTES + 1]), None),
            (None, Some(vec![0; EnclaveRequest::MAX_NONCE_BYTES + 1])),
        ] {
            let err = client.signer_attestation(user_data, nonce).await.unwrap_err();
            assert!(
                matches!(err, EnclaveLinkError::Enclave(EnclaveFailure::InvalidRequest(_))),
                "{err:?}"
            );
        }
    }

    #[tokio::test]
    async fn oversized_nonce_is_rejected_before_sending() {
        let (host, mut peer) = duplex(64 * 1024);
        let request = EnclaveRequest::SignerAttestation {
            user_data: None,
            nonce: Some(vec![0; EnclaveRequest::MAX_NONCE_BYTES + 1]),
        };
        let err = EnclaveClient::exchange(host, request, Some(Duration::from_millis(10)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, EnclaveLinkError::Enclave(EnclaveFailure::InvalidRequest(_))),
            "{err:?}"
        );
        assert_eq!(
            peer.read(&mut [0; 1]).await.unwrap(),
            0,
            "invalid requests must not write a frame"
        );
    }

    #[tokio::test]
    async fn peer_closing_mid_exchange_is_a_frame_error() {
        let (host, mut enclave) = duplex(64 * 1024);
        let peer = tokio::spawn(async move {
            let _: EnclaveRequest = Frame::read(&mut enclave).await.unwrap();
        });
        let err =
            EnclaveClient::exchange(host, EnclaveRequest::SignerPublicKey, None).await.unwrap_err();
        peer.await.unwrap();
        assert!(matches!(err, EnclaveLinkError::Frame(_)), "{err:?}");
    }

    #[tokio::test]
    async fn legacy_error_reply_is_a_proof_rejection() {
        let (host, mut enclave) = duplex(64 * 1024);
        let peer = tokio::spawn(async move {
            let _: EnclaveRequest = Frame::read(&mut enclave).await.unwrap();
            Frame::write(&mut enclave, &EnclaveResponse::Error("boom".into())).await.unwrap();
        });
        let err = EnclaveClient::exchange(host, EnclaveRequest::Prove(Vec::new()), None)
            .await
            .unwrap_err();
        peer.await.unwrap();
        assert!(
            matches!(err, EnclaveLinkError::Enclave(EnclaveFailure::Proof(ref m)) if m == "boom")
        );
    }
}
