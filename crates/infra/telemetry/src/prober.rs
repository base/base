use std::{fmt, net::SocketAddr, time::Duration};

use alloy_primitives::B512;
use async_trait::async_trait;
use reth_ecies::stream::ECIESStream;
use reth_eth_wire::{
    HelloMessage, UnauthedP2PStream,
    errors::{P2PHandshakeError, P2PStreamError},
};
use secp256k1::{PublicKey, SECP256K1, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};

/// Maximum time allowed for one complete reachability probe.
pub const RLPX_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stable outcome returned by an `RLPx` reachability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlpxProbeOutcome {
    /// TCP, ECIES, and devp2p Hello completed successfully.
    Reachable,
    /// The TCP connection could not be established.
    ConnectionFailed,
    /// The overall probe deadline elapsed.
    TimedOut,
    /// TCP connected, but ECIES or devp2p Hello failed.
    HandshakeFailed,
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
        Self {
            secret_key: SecretKey::new(&mut secp256k1::rand::thread_rng()),
            timeout: RLPX_PROBE_TIMEOUT,
        }
    }

    #[cfg(test)]
    /// Creates an ephemeral prober with a test-specific timeout.
    pub fn ephemeral_with_timeout(timeout: Duration) -> Self {
        Self { secret_key: SecretKey::new(&mut secp256k1::rand::thread_rng()), timeout }
    }
}

#[async_trait]
impl ReachabilityProber for RlpxProber {
    async fn probe(&self, target: RlpxProbeTarget) -> RlpxProbeResult {
        let started = Instant::now();
        let deadline = started + self.timeout;
        let finish = |outcome, stage, client_version| RlpxProbeResult {
            outcome,
            stage,
            elapsed: started.elapsed(),
            client_version,
        };

        let tcp = match timeout_at(deadline, TcpStream::connect(target.address)).await {
            Err(_) => {
                return finish(RlpxProbeOutcome::TimedOut, RlpxProbeStage::Tcp, None);
            }
            Ok(Err(_)) => {
                return finish(RlpxProbeOutcome::ConnectionFailed, RlpxProbeStage::Tcp, None);
            }
            Ok(Ok(tcp)) => tcp,
        };

        let ecies = match timeout_at(
            deadline,
            ECIESStream::connect_without_timeout(tcp, self.secret_key, target.node_id),
        )
        .await
        {
            Err(_) => {
                return finish(RlpxProbeOutcome::TimedOut, RlpxProbeStage::Ecies, None);
            }
            Ok(Err(_)) => {
                return finish(RlpxProbeOutcome::HandshakeFailed, RlpxProbeStage::Ecies, None);
            }
            Ok(Ok(ecies)) => ecies,
        };

        let public_key = PublicKey::from_secret_key(SECP256K1, &self.secret_key);
        let local_node_id = B512::from_slice(&public_key.serialize_uncompressed()[1..]);
        let hello = HelloMessage::builder(local_node_id)
            .client_version(format!("base-telemetry/{}", env!("CARGO_PKG_VERSION")))
            .port(0)
            .build();

        match timeout_at(deadline, UnauthedP2PStream::new(ecies).handshake(hello)).await {
            Err(_) | Ok(Err(P2PStreamError::HandshakeError(P2PHandshakeError::Timeout))) => {
                finish(RlpxProbeOutcome::TimedOut, RlpxProbeStage::Rlpx, None)
            }
            Ok(Err(_)) => finish(RlpxProbeOutcome::HandshakeFailed, RlpxProbeStage::Rlpx, None),
            Ok(Ok((_, remote_hello))) => finish(
                RlpxProbeOutcome::Reachable,
                RlpxProbeStage::Rlpx,
                Some(remote_hello.client_version),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future, time::Duration};

    use alloy_primitives::B512;
    use reth_ecies::stream::ECIESStream;
    use reth_eth_wire::{HelloMessage, UnauthedP2PStream};
    use secp256k1::{PublicKey, SECP256K1, SecretKey};
    use tokio::{net::TcpListener, sync::oneshot};

    use super::{
        ReachabilityProber, RlpxProbeOutcome, RlpxProbeStage, RlpxProbeTarget, RlpxProber,
    };

    const TEST_TIMEOUT: Duration = Duration::from_millis(100);

    fn node_identity() -> (SecretKey, B512) {
        let secret = SecretKey::new(&mut secp256k1::rand::thread_rng());
        let public = PublicKey::from_secret_key(SECP256K1, &secret);
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

        let result =
            RlpxProber::ephemeral().probe(RlpxProbeTarget { address, node_id: remote_id }).await;

        assert_eq!(result.outcome, RlpxProbeOutcome::HandshakeFailed);
        assert_eq!(result.stage, RlpxProbeStage::Rlpx);
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
