//! Consensus-layer libp2p reachability probing: dials a target over TCP,
//! authenticates the Noise transport against the expected peer identity, and
//! collects the remote identify agent version.

use std::{fmt, io, net::SocketAddr, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, identify,
    identity::Keypair,
    multiaddr::Protocol,
    noise::{Config as NoiseConfig, Error as NoiseError},
    swarm::{DialError, SwarmEvent},
    tcp::Config as TcpConfig,
    yamux::Config as YamuxConfig,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tracing::debug;

/// Maximum time allowed for one complete consensus-layer reachability probe.
pub const LIBP2P_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stable outcome returned by a consensus-layer libp2p reachability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Libp2pProbeOutcome {
    /// TCP, the Noise handshake, and stream multiplexer negotiation all
    /// completed against the expected peer identity. A node that closes the
    /// connection right after it is established (e.g. at peer capacity) is
    /// still reachable; its probe omits the client version.
    Reachable,
    /// The TCP connection could not be established.
    ConnectionFailed,
    /// The overall probe deadline elapsed.
    TimedOut,
    /// TCP connected, but the Noise handshake or peer identity check failed.
    HandshakeFailed,
}

impl fmt::Display for Libp2pProbeOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Reachable => "reachable",
            Self::ConnectionFailed => "connection_failed",
            Self::TimedOut => "timed_out",
            Self::HandshakeFailed => "handshake_failed",
        })
    }
}

/// Protocol stage reached by a consensus-layer libp2p reachability probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Libp2pProbeStage {
    /// Establishing the TCP connection.
    TcpConnect,
    /// Authenticating the Noise transport and negotiating the multiplexer.
    SecurityHandshake,
    /// Exchanging libp2p identify information.
    Identify,
}

impl fmt::Display for Libp2pProbeStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TcpConnect => "tcp_connect",
            Self::SecurityHandshake => "security_handshake",
            Self::Identify => "identify",
        })
    }
}

/// Failure of one consensus-layer libp2p reachability probe.
#[derive(Debug, thiserror::Error)]
pub enum Libp2pProbeError {
    /// The TCP connection could not be established.
    #[error("tcp connect failed: {0}")]
    Tcp(#[from] io::Error),
    /// The local Noise configuration could not be constructed.
    #[error("noise configuration failed: {0}")]
    Noise(#[from] NoiseError),
    /// The libp2p dial failed after TCP reachability was verified, so the
    /// Noise handshake, peer identity check, or multiplexer negotiation
    /// failed.
    #[error("libp2p dial failed: {0}")]
    Dial(#[from] DialError),
    /// The probe deadline elapsed at the given stage.
    #[error("probe timed out at {0} stage")]
    TimedOut(Libp2pProbeStage),
}

impl Libp2pProbeError {
    /// Returns the stable outcome for this failure.
    pub const fn outcome(&self) -> Libp2pProbeOutcome {
        match self {
            Self::Tcp(_) => Libp2pProbeOutcome::ConnectionFailed,
            Self::Noise(_) | Self::Dial(_) => Libp2pProbeOutcome::HandshakeFailed,
            Self::TimedOut(_) => Libp2pProbeOutcome::TimedOut,
        }
    }

    /// Returns the protocol stage at which the failure occurred.
    pub const fn stage(&self) -> Libp2pProbeStage {
        match self {
            Self::Tcp(_) => Libp2pProbeStage::TcpConnect,
            Self::Noise(_) | Self::Dial(_) => Libp2pProbeStage::SecurityHandshake,
            Self::TimedOut(stage) => *stage,
        }
    }
}

/// Network target for a consensus-layer libp2p probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Libp2pProbeTarget {
    /// Public socket address to dial.
    pub address: SocketAddr,
    /// Expected libp2p peer identity.
    pub peer_id: PeerId,
}

/// Result produced by a consensus-layer libp2p probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Libp2pProbeResult {
    /// Stable probe outcome.
    pub outcome: Libp2pProbeOutcome,
    /// Protocol stage reached by the probe.
    pub stage: Libp2pProbeStage,
    /// Total probe duration.
    pub elapsed: Duration,
    /// Agent version returned by the remote libp2p identify exchange.
    pub client_version: Option<String>,
}

/// Interface used by the HTTP route to execute consensus-layer probes.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ClReachabilityProber: fmt::Debug + Send + Sync {
    /// Probes one consensus-layer target.
    async fn probe(&self, target: Libp2pProbeTarget) -> Libp2pProbeResult;
}

/// Process-local consensus-layer libp2p prober.
///
/// Each probe runs a fresh single-connection libp2p swarm with TCP, Noise,
/// and Yamux — the same transport stack the consensus node listens with — and
/// an identify behaviour to capture the remote agent version.
#[derive(Clone)]
pub struct Libp2pProber {
    keypair: Keypair,
    timeout: Duration,
}

impl fmt::Debug for Libp2pProber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Libp2pProber").finish_non_exhaustive()
    }
}

impl Libp2pProber {
    /// Creates a prober with a fresh ephemeral peer identity.
    pub fn ephemeral() -> Self {
        Self { keypair: Keypair::generate_secp256k1(), timeout: LIBP2P_PROBE_TIMEOUT }
    }

    /// Creates an ephemeral prober with a test-specific timeout.
    #[cfg(test)]
    pub fn ephemeral_with_timeout(timeout: Duration) -> Self {
        Self { timeout, ..Self::ephemeral() }
    }

    /// Runs one probe attempt against the deadline and returns the remote
    /// agent version advertised over identify, if any.
    ///
    /// TCP reachability is verified with a direct throwaway connect before
    /// the libp2p dial: the swarm's boxed transport erases the
    /// TCP-versus-upgrade error split, so connection failures could not
    /// otherwise be told apart from handshake failures.
    pub async fn try_probe(
        &self,
        target: Libp2pProbeTarget,
        deadline: Instant,
    ) -> Result<Option<String>, Libp2pProbeError> {
        let stream = timeout_at(deadline, TcpStream::connect(target.address))
            .await
            .map_err(|_| Libp2pProbeError::TimedOut(Libp2pProbeStage::TcpConnect))??;
        drop(stream);

        let mut swarm = SwarmBuilder::with_existing_identity(self.keypair.clone())
            .with_tokio()
            .with_tcp(TcpConfig::default().nodelay(true), NoiseConfig::new, YamuxConfig::default)?
            .with_behaviour(|keypair| {
                identify::Behaviour::new(
                    identify::Config::new(String::new(), keypair.public()).with_agent_version(
                        format!("base-telemetry/{}", env!("CARGO_PKG_VERSION")),
                    ),
                )
            })
            .expect("infallible behaviour constructor")
            .with_swarm_config(|config| config.with_idle_connection_timeout(self.timeout))
            // The probe deadline must fire before the swarm's own transport
            // timeout so timeouts are reported per stage.
            .with_connection_timeout(self.timeout + Duration::from_secs(1))
            .build();

        let address = Multiaddr::from(target.address.ip())
            .with(Protocol::Tcp(target.address.port()))
            .with(Protocol::P2p(target.peer_id));
        swarm.dial(address)?;

        let mut established = false;
        loop {
            let event = match timeout_at(deadline, swarm.select_next_some()).await {
                Ok(event) => event,
                // Deadline expiry after the connection is established means
                // the identify exchange stalled; the node is still reachable.
                Err(_) if established => return Ok(None),
                Err(_) => {
                    return Err(Libp2pProbeError::TimedOut(Libp2pProbeStage::SecurityHandshake));
                }
            };
            match event {
                SwarmEvent::ConnectionEstablished { .. } => established = true,
                SwarmEvent::Behaviour(identify::Event::Received { info, .. }) => {
                    return Ok((!info.agent_version.is_empty()).then_some(info.agent_version));
                }
                // The connection is established, so the node is reachable even
                // when identify fails or the peer hangs up (e.g. at capacity).
                SwarmEvent::Behaviour(identify::Event::Error { .. }) => return Ok(None),
                SwarmEvent::ConnectionClosed { .. } if established => return Ok(None),
                SwarmEvent::OutgoingConnectionError { error, .. } => return Err(error.into()),
                _ => {}
            }
        }
    }
}

#[async_trait]
impl ClReachabilityProber for Libp2pProber {
    async fn probe(&self, target: Libp2pProbeTarget) -> Libp2pProbeResult {
        let started = Instant::now();
        match self.try_probe(target, started + self.timeout).await {
            Ok(client_version) => Libp2pProbeResult {
                outcome: Libp2pProbeOutcome::Reachable,
                stage: Libp2pProbeStage::Identify,
                elapsed: started.elapsed(),
                client_version,
            },
            Err(error) => {
                debug!(
                    error = %error,
                    target = %target.address,
                    stage = %error.stage(),
                    "cl reachability probe failed"
                );
                Libp2pProbeResult {
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
    use std::{future, net::SocketAddr, time::Duration};

    use futures::StreamExt;
    use libp2p::{
        Multiaddr, PeerId, SwarmBuilder, identify, identity::Keypair, noise::Config as NoiseConfig,
        swarm::SwarmEvent, tcp::Config as TcpConfig, yamux::Config as YamuxConfig,
    };
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    use super::{
        ClReachabilityProber, Libp2pProbeOutcome, Libp2pProbeStage, Libp2pProbeTarget, Libp2pProber,
    };

    const TEST_TIMEOUT: Duration = Duration::from_millis(500);

    /// Starts a libp2p listener that answers identify with `agent_version`.
    async fn start_libp2p_peer(agent_version: &str) -> (SocketAddr, PeerId, JoinHandle<()>) {
        let keypair = Keypair::generate_secp256k1();
        let peer_id = keypair.public().to_peer_id();
        let agent_version = agent_version.to_string();
        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(TcpConfig::default(), NoiseConfig::new, YamuxConfig::default)
            .unwrap()
            .with_behaviour(|keypair| {
                identify::Behaviour::new(
                    identify::Config::new(String::new(), keypair.public())
                        .with_agent_version(agent_version),
                )
            })
            .unwrap()
            .build();
        swarm.listen_on("/ip4/127.0.0.1/tcp/0".parse::<Multiaddr>().unwrap()).unwrap();

        let address = loop {
            if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
                let mut ip = None;
                let mut port = None;
                for protocol in &address {
                    match protocol {
                        libp2p::multiaddr::Protocol::Ip4(v4) => ip = Some(v4),
                        libp2p::multiaddr::Protocol::Tcp(tcp) => port = Some(tcp),
                        _ => {}
                    }
                }
                break SocketAddr::from((ip.unwrap(), port.unwrap()));
            }
        };
        let handle = tokio::spawn(async move {
            loop {
                swarm.select_next_some().await;
            }
        });
        (address, peer_id, handle)
    }

    #[tokio::test]
    async fn completes_probe_against_local_libp2p_peer() {
        let (address, peer_id, server) = start_libp2p_peer("test-peer/1.0").await;

        let result = Libp2pProber::ephemeral().probe(Libp2pProbeTarget { address, peer_id }).await;

        assert_eq!(result.outcome, Libp2pProbeOutcome::Reachable);
        assert_eq!(result.stage, Libp2pProbeStage::Identify);
        assert_eq!(result.client_version.as_deref(), Some("test-peer/1.0"));
        server.abort();
    }

    #[tokio::test]
    async fn reports_wrong_peer_identity_as_handshake_failure() {
        let (address, _, server) = start_libp2p_peer("test-peer/1.0").await;
        let other_peer_id = Keypair::generate_secp256k1().public().to_peer_id();

        let result = Libp2pProber::ephemeral()
            .probe(Libp2pProbeTarget { address, peer_id: other_peer_id })
            .await;

        assert_eq!(result.outcome, Libp2pProbeOutcome::HandshakeFailed);
        assert_eq!(result.stage, Libp2pProbeStage::SecurityHandshake);
        assert_eq!(result.client_version, None);
        server.abort();
    }

    #[tokio::test]
    async fn reports_non_libp2p_peer_as_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer_id = Keypair::generate_secp256k1().public().to_peer_id();
        let server = tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                drop(tcp);
            }
        });

        let result = Libp2pProber::ephemeral().probe(Libp2pProbeTarget { address, peer_id }).await;

        assert_eq!(result.outcome, Libp2pProbeOutcome::HandshakeFailed);
        assert_eq!(result.stage, Libp2pProbeStage::SecurityHandshake);
        server.abort();
    }

    #[tokio::test]
    async fn reports_closed_port_as_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let peer_id = Keypair::generate_secp256k1().public().to_peer_id();

        let result = Libp2pProber::ephemeral().probe(Libp2pProbeTarget { address, peer_id }).await;

        assert_eq!(result.outcome, Libp2pProbeOutcome::ConnectionFailed);
        assert_eq!(result.stage, Libp2pProbeStage::TcpConnect);
        assert_eq!(result.client_version, None);
    }

    #[tokio::test]
    async fn reports_stalled_handshake_as_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer_id = Keypair::generate_secp256k1().public().to_peer_id();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            // First accept serves the probe's TCP pre-flight; the second is
            // the libp2p dial, which is held open without answering Noise.
            let (preflight, _) = listener.accept().await.unwrap();
            drop(preflight);
            let (tcp, _) = listener.accept().await.unwrap();
            accepted_tx.send(()).unwrap();
            future::pending::<()>().await;
            drop(tcp);
        });
        let probe = tokio::spawn(async move {
            Libp2pProber::ephemeral_with_timeout(TEST_TIMEOUT)
                .probe(Libp2pProbeTarget { address, peer_id })
                .await
        });

        accepted_rx.await.unwrap();
        let result = probe.await.unwrap();

        assert_eq!(result.outcome, Libp2pProbeOutcome::TimedOut);
        assert_eq!(result.stage, Libp2pProbeStage::SecurityHandshake);
        server.abort();
    }
}
