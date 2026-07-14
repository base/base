//! Execution-layer `RLPx` reachability probing: dials a target over TCP,
//! authenticates the ECIES transport, and exchanges the devp2p Hello.

use std::{fmt, io, net::SocketAddr, time::Duration};

use alloy_primitives::B512;
use async_trait::async_trait;
use reth_ecies::{ECIESError, stream::ECIESStream};
use reth_eth_wire::{
    HelloMessage, UnauthedP2PStream,
    errors::{P2PHandshakeError, P2PStreamError},
};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tracing::debug;

/// Maximum time allowed for one complete reachability probe.
pub const RLPX_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stable outcome returned by an `RLPx` reachability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlpxProbeOutcome {
    /// TCP and ECIES completed, and the peer answered the devp2p Hello
    /// exchange, either with its own Hello or with an authenticated
    /// Disconnect (e.g. at peer capacity).
    Reachable,
    /// The TCP connection could not be established.
    ConnectionFailed,
    /// The overall probe deadline elapsed.
    TimedOut,
    /// TCP connected, but ECIES or devp2p Hello failed.
    HandshakeFailed,
}

impl fmt::Display for RlpxProbeOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Reachable => "reachable",
            Self::ConnectionFailed => "connection_failed",
            Self::TimedOut => "timed_out",
            Self::HandshakeFailed => "handshake_failed",
        })
    }
}

/// Protocol stage reached by an `RLPx` reachability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlpxProbeStage {
    /// Establishing the TCP connection.
    Tcp,
    /// Authenticating the encrypted ECIES transport.
    Ecies,
    /// Exchanging the devp2p Hello message.
    Rlpx,
}

impl fmt::Display for RlpxProbeStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tcp => "tcp",
            Self::Ecies => "ecies",
            Self::Rlpx => "rlpx",
        })
    }
}

/// Failure of one execution-layer `RLPx` reachability probe.
#[derive(Debug, thiserror::Error)]
pub enum RlpxProbeError {
    /// The TCP connection could not be established.
    #[error("tcp connect failed: {0}")]
    Tcp(#[from] io::Error),
    /// The encrypted ECIES transport could not be authenticated.
    #[error("ecies handshake failed: {0}")]
    Ecies(#[from] ECIESError),
    /// The devp2p Hello exchange failed.
    #[error("rlpx hello exchange failed: {0}")]
    Rlpx(#[source] P2PStreamError),
    /// The probe deadline elapsed at the given stage.
    #[error("probe timed out at {0} stage")]
    TimedOut(RlpxProbeStage),
}

impl RlpxProbeError {
    /// Returns the stable outcome for this failure.
    pub const fn outcome(&self) -> RlpxProbeOutcome {
        match self {
            Self::Tcp(_) => RlpxProbeOutcome::ConnectionFailed,
            Self::Ecies(_) | Self::Rlpx(_) => RlpxProbeOutcome::HandshakeFailed,
            Self::TimedOut(_) => RlpxProbeOutcome::TimedOut,
        }
    }

    /// Returns the protocol stage at which the failure occurred.
    pub const fn stage(&self) -> RlpxProbeStage {
        match self {
            Self::Tcp(_) => RlpxProbeStage::Tcp,
            Self::Ecies(_) => RlpxProbeStage::Ecies,
            Self::Rlpx(_) => RlpxProbeStage::Rlpx,
            Self::TimedOut(stage) => *stage,
        }
    }
}

/// Network target for an execution-layer `RLPx` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RlpxProbeTarget {
    /// Public socket address to dial.
    pub address: SocketAddr,
    /// Expected 64-byte execution-layer node identity.
    pub node_id: B512,
}

/// Result produced by an execution-layer `RLPx` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RlpxProbeResult {
    /// Stable probe outcome.
    pub outcome: RlpxProbeOutcome,
    /// Protocol stage reached by the probe.
    pub stage: RlpxProbeStage,
    /// Total probe duration.
    pub elapsed: Duration,
    /// Client version returned by the remote devp2p Hello.
    pub client_version: Option<String>,
}

/// Interface used by the HTTP route to execute reachability probes.
#[async_trait]
pub trait ReachabilityProber: fmt::Debug + Send + Sync {
    /// Probes one execution-layer target.
    async fn probe(&self, target: RlpxProbeTarget) -> RlpxProbeResult;
}

/// Process-local execution-layer `RLPx` prober.
#[derive(Clone)]
pub struct RlpxProber {
    secret_key: SecretKey,
    local_node_id: B512,
    timeout: Duration,
}

impl fmt::Debug for RlpxProber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RlpxProber").finish_non_exhaustive()
    }
}

impl RlpxProber {
    /// Creates a prober with a fresh ephemeral node identity.
    pub fn ephemeral() -> Self {
        let secret_key = SecretKey::new(&mut secp256k1::rand::thread_rng());
        let secp = Secp256k1::signing_only();
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let local_node_id = B512::from_slice(&public_key.serialize_uncompressed()[1..]);

        Self { secret_key, local_node_id, timeout: RLPX_PROBE_TIMEOUT }
    }

    /// Creates an ephemeral prober with a test-specific timeout.
    #[cfg(test)]
    pub fn ephemeral_with_timeout(timeout: Duration) -> Self {
        Self { timeout, ..Self::ephemeral() }
    }

    /// Runs one probe attempt against the deadline and returns the remote
    /// client version advertised in its Hello, if any.
    pub async fn try_probe(
        &self,
        target: RlpxProbeTarget,
        deadline: Instant,
    ) -> Result<Option<String>, RlpxProbeError> {
        let tcp = timeout_at(deadline, TcpStream::connect(target.address))
            .await
            .map_err(|_| RlpxProbeError::TimedOut(RlpxProbeStage::Tcp))??;

        let ecies = timeout_at(
            deadline,
            ECIESStream::connect_without_timeout(tcp, self.secret_key, target.node_id),
        )
        .await
        .map_err(|_| RlpxProbeError::TimedOut(RlpxProbeStage::Ecies))??;

        let hello = HelloMessage::builder(self.local_node_id)
            .client_version(format!("base-telemetry/{}", env!("CARGO_PKG_VERSION")))
            .port(0)
            .build();

        match timeout_at(deadline, UnauthedP2PStream::new(ecies).handshake(hello))
            .await
            .map_err(|_| RlpxProbeError::TimedOut(RlpxProbeStage::Rlpx))?
        {
            Ok((_, remote_hello)) => Ok(Some(remote_hello.client_version)),
            Err(P2PStreamError::HandshakeError(P2PHandshakeError::Timeout)) => {
                Err(RlpxProbeError::TimedOut(RlpxProbeStage::Rlpx))
            }
            Err(P2PStreamError::HandshakeError(P2PHandshakeError::Disconnected(reason))) => {
                // A Disconnect received here arrived over the authenticated ECIES
                // stream, so the node is reachable even though it refused the
                // session (e.g. it is at peer capacity).
                debug!(
                    reason = %reason,
                    target = %target.address,
                    "reachability probe disconnected by reachable peer"
                );
                Ok(None)
            }
            Err(error) => Err(RlpxProbeError::Rlpx(error)),
        }
    }
}

#[async_trait]
impl ReachabilityProber for RlpxProber {
    async fn probe(&self, target: RlpxProbeTarget) -> RlpxProbeResult {
        let started = Instant::now();
        match self.try_probe(target, started + self.timeout).await {
            Ok(client_version) => RlpxProbeResult {
                outcome: RlpxProbeOutcome::Reachable,
                stage: RlpxProbeStage::Rlpx,
                elapsed: started.elapsed(),
                client_version,
            },
            Err(error) => {
                debug!(
                    error = %error,
                    target = %target.address,
                    stage = %error.stage(),
                    "reachability probe failed"
                );
                RlpxProbeResult {
                    outcome: error.outcome(),
                    stage: error.stage(),
                    elapsed: started.elapsed(),
                    client_version: None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, future, time::Duration};

    use alloy_primitives::B512;
    use reth_ecies::stream::ECIESStream;
    use reth_eth_wire::{DisconnectReason, HelloMessage, UnauthedP2PStream};
    use secp256k1::{PublicKey, Secp256k1, SecretKey};
    use tokio::{net::TcpListener, sync::oneshot, time::Instant};

    use super::{
        RLPX_PROBE_TIMEOUT, ReachabilityProber, RlpxProbeOutcome, RlpxProbeStage, RlpxProbeTarget,
        RlpxProber,
    };

    const TEST_TIMEOUT: Duration = Duration::from_millis(100);

    fn node_identity() -> (SecretKey, B512) {
        let secret = SecretKey::new(&mut secp256k1::rand::thread_rng());
        let secp = Secp256k1::signing_only();
        let public = PublicKey::from_secret_key(&secp, &secret);
        let id = B512::from_slice(&public.serialize_uncompressed()[1..]);
        (secret, id)
    }

    #[tokio::test]
    async fn completes_rlpx_handshake_with_local_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (remote_secret, remote_id) = node_identity();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ecies = ECIESStream::incoming(tcp, remote_secret).await.unwrap();
            let hello =
                HelloMessage::builder(remote_id).client_version("test-peer/1.0").port(0).build();
            UnauthedP2PStream::new(ecies).handshake(hello).await.unwrap();
        });

        let result =
            RlpxProber::ephemeral().probe(RlpxProbeTarget { address, node_id: remote_id }).await;

        assert_eq!(result.outcome, RlpxProbeOutcome::Reachable);
        assert_eq!(result.stage, RlpxProbeStage::Rlpx);
        assert_eq!(result.client_version.as_deref(), Some("test-peer/1.0"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reports_disconnect_during_hello_as_reachable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (remote_secret, remote_id) = node_identity();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ecies = ECIESStream::incoming(tcp, remote_secret).await.unwrap();
            UnauthedP2PStream::new(ecies)
                .send_disconnect(DisconnectReason::TooManyPeers)
                .await
                .unwrap();
        });

        let result =
            RlpxProber::ephemeral().probe(RlpxProbeTarget { address, node_id: remote_id }).await;

        assert_eq!(result.outcome, RlpxProbeOutcome::Reachable);
        assert_eq!(result.stage, RlpxProbeStage::Rlpx);
        assert_eq!(result.client_version, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reports_ecies_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_, remote_id) = node_identity();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            drop(tcp);
        });

        let result =
            RlpxProber::ephemeral().probe(RlpxProbeTarget { address, node_id: remote_id }).await;

        assert_eq!(result.outcome, RlpxProbeOutcome::HandshakeFailed);
        assert_eq!(result.stage, RlpxProbeStage::Ecies);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reports_rlpx_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (remote_secret, remote_id) = node_identity();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ecies = ECIESStream::incoming(tcp, remote_secret).await.unwrap();
            drop(ecies);
        });

        let error = RlpxProber::ephemeral()
            .try_probe(
                RlpxProbeTarget { address, node_id: remote_id },
                Instant::now() + RLPX_PROBE_TIMEOUT,
            )
            .await
            .unwrap_err();

        assert_eq!(error.outcome(), RlpxProbeOutcome::HandshakeFailed);
        assert_eq!(error.stage(), RlpxProbeStage::Rlpx);
        assert!(error.source().is_some());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reports_ecies_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_, remote_id) = node_identity();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut byte = [0_u8; 1];
            tcp.peek(&mut byte).await.unwrap();
            accepted_tx.send(()).unwrap();
            future::pending::<()>().await;
            drop(tcp);
        });
        let probe = tokio::spawn(async move {
            RlpxProber::ephemeral_with_timeout(TEST_TIMEOUT)
                .probe(RlpxProbeTarget { address, node_id: remote_id })
                .await
        });

        accepted_rx.await.unwrap();
        let result = probe.await.unwrap();

        assert_eq!(result.outcome, RlpxProbeOutcome::TimedOut);
        assert_eq!(result.stage, RlpxProbeStage::Ecies);
        server.abort();
    }

    #[tokio::test]
    async fn reports_rlpx_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (remote_secret, remote_id) = node_identity();
        let (authenticated_tx, authenticated_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ecies = ECIESStream::incoming(tcp, remote_secret).await.unwrap();
            authenticated_tx.send(()).unwrap();
            future::pending::<()>().await;
            drop(ecies);
        });
        let probe = tokio::spawn(async move {
            RlpxProber::ephemeral_with_timeout(TEST_TIMEOUT)
                .probe(RlpxProbeTarget { address, node_id: remote_id })
                .await
        });

        authenticated_rx.await.unwrap();
        let result = probe.await.unwrap();

        assert_eq!(result.outcome, RlpxProbeOutcome::TimedOut);
        assert_eq!(result.stage, RlpxProbeStage::Rlpx);
        server.abort();
    }

    #[tokio::test]
    async fn reports_closed_port_as_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let result =
            RlpxProber::ephemeral().probe(RlpxProbeTarget { address, node_id: B512::ZERO }).await;

        assert_eq!(result.outcome, RlpxProbeOutcome::ConnectionFailed);
        assert_eq!(result.stage, RlpxProbeStage::Tcp);
        assert_eq!(result.client_version, None);
    }
}
