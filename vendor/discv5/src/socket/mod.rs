use std::{
    collections::HashMap,
    io::Error,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::Arc,
    time::Duration,
};

use parking_lot::RwLock;
use recv::*;
use send::*;
use socket2::{Domain, Protocol, Socket as Socket2, Type};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
};

use crate::{Executor, ProtocolIdentity};

mod filter;
mod recv;
mod send;

pub use filter::{
    FilterConfig,
    rate_limiter::{RateLimiter, RateLimiterBuilder},
};
pub use recv::InboundPacket;
pub use send::OutboundPacket;

/// Configuration for the sockets to listen on.
///
/// Default implementation is the UNSPECIFIED ipv4 address with port 9000.
#[derive(Clone, Debug)]
pub enum ListenConfig {
    Ipv4 { ip: Ipv4Addr, port: u16 },
    Ipv6 { ip: Ipv6Addr, port: u16 },
    DualStack { ipv4: Ipv4Addr, ipv4_port: u16, ipv6: Ipv6Addr, ipv6_port: u16 },
}

/// Convenience objects for setting up the recv handler.
pub struct SocketConfig {
    /// The executor to spawn the tasks.
    pub executor: Box<dyn Executor + Send + Sync>,
    /// Configuration details for the packet filter.
    pub filter_config: FilterConfig,
    /// Type of socket to create.
    pub listen_config: ListenConfig,
    /// If the filter is enabled this sets the default timeout for bans enacted by the filter.
    pub ban_duration: Option<Duration>,
    /// The expected responses reference.
    pub expected_responses: Arc<RwLock<HashMap<SocketAddr, usize>>>,
    /// The local node id used to decrypt messages.
    pub local_node_id: enr::NodeId,
    /// The protocol identity used in sent and received packets.
    pub protocol_identity: ProtocolIdentity,
}

/// Creates the UDP socket and handles the exit futures for the send/recv UDP handlers.
pub struct Socket {
    pub send: mpsc::Sender<OutboundPacket>,
    pub recv: mpsc::Receiver<InboundPacket>,
    sender_exit: Option<oneshot::Sender<()>>,
    recv_exit: Option<oneshot::Sender<()>>,
}

impl Socket {
    /// This creates and binds a new UDP socket.
    // In general this function can be expanded to handle more advanced socket creation.
    async fn new_socket(socket_addr: &SocketAddr) -> Result<UdpSocket, Error> {
        match socket_addr {
            SocketAddr::V4(ip4) => UdpSocket::bind(ip4).await,
            SocketAddr::V6(ip6) => {
                let socket = Socket2::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
                socket.set_only_v6(true)?;
                socket.set_nonblocking(true)?;
                socket.bind(&SocketAddr::V6(*ip6).into())?;
                UdpSocket::from_std(socket.into())
            }
        }
    }

    /// Creates a UDP socket, spawns a send/recv task and returns the channels.
    /// If this struct is dropped, the send/recv tasks will shutdown.
    /// This needs to be run inside of a tokio executor.
    pub(crate) async fn new(config: SocketConfig) -> Result<Self, Error> {
        let SocketConfig {
            executor,
            filter_config,
            listen_config,
            ban_duration,
            expected_responses,
            local_node_id,
            protocol_identity,
        } = config;

        // For recv socket, intentionally forgetting which socket is the ipv4 and which is the ipv6 one.
        let (first_recv, second_recv, send_ipv4, send_ipv6): (
            Arc<UdpSocket>,
            Option<_>,
            Option<_>,
            Option<_>,
        ) = match listen_config {
            ListenConfig::Ipv4 { ip, port } => {
                let ipv4_socket = Arc::new(Socket::new_socket(&(ip, port).into()).await?);
                (ipv4_socket.clone(), None, Some(ipv4_socket), None)
            }
            ListenConfig::Ipv6 { ip, port } => {
                let ipv6_socket = Arc::new(Socket::new_socket(&(ip, port).into()).await?);
                (ipv6_socket.clone(), None, None, Some(ipv6_socket))
            }
            ListenConfig::DualStack { ipv4, ipv4_port, ipv6, ipv6_port } => {
                let ipv4_socket = Arc::new(Socket::new_socket(&(ipv4, ipv4_port).into()).await?);
                let ipv6_socket = Arc::new(Socket::new_socket(&(ipv6, ipv6_port).into()).await?);
                (
                    ipv4_socket.clone(),
                    Some(ipv6_socket.clone()),
                    Some(ipv4_socket),
                    Some(ipv6_socket),
                )
            }
        };

        // spawn the recv handler
        let recv_config = RecvHandlerConfig {
            filter_config,
            executor: executor.clone(),
            recv: first_recv,
            second_recv,
            local_node_id,
            protocol_identity,
            expected_responses,
            ban_duration,
        };

        let (recv, recv_exit) = RecvHandler::spawn(recv_config);
        // spawn the sender handler
        let (send, sender_exit) = SendHandler::spawn(executor, send_ipv4, send_ipv6);

        Ok(Socket { send, recv, sender_exit: Some(sender_exit), recv_exit: Some(recv_exit) })
    }
}

impl ListenConfig {
    /// If an [`IpAddr`] is known, a ListenConfig can be created based on the version. This will
    /// not create a dual stack configuration.
    pub fn from_ip(ip: IpAddr, port: u16) -> ListenConfig {
        match ip {
            IpAddr::V4(ip) => ListenConfig::Ipv4 { ip, port },
            IpAddr::V6(ip) => ListenConfig::Ipv6 { ip, port },
        }
    }

    /// Allows optional ipv4 and ipv6 addresses to be entered to create a [`ListenConfig`]. If both
    /// are specified a dual-stack configuration will result. This will panic if both parameters
    /// are None.
    pub fn from_two_sockets(
        ipv4: Option<SocketAddrV4>,
        ipv6: Option<SocketAddrV6>,
    ) -> ListenConfig {
        match (ipv4, ipv6) {
            (Some(ipv4), None) => ListenConfig::Ipv4 { ip: *ipv4.ip(), port: ipv4.port() },
            (None, Some(ipv6)) => ListenConfig::Ipv6 { ip: *ipv6.ip(), port: ipv6.port() },
            (Some(ipv4), Some(ipv6)) => ListenConfig::DualStack {
                ipv4: *ipv4.ip(),
                ipv4_port: ipv4.port(),
                ipv6: *ipv6.ip(),
                ipv6_port: ipv6.port(),
            },
            (None, None) => panic!("At least one IP address must be entered."),
        }
    }

    // Overrides the ipv4 address and port of ipv4 and dual stack configurations. Ipv6
    // configurations are added ipv4 info making them into dual stack configs.
    /// Sets an ipv4 socket. This will override any past ipv4 configuration and will promote the configuration to dual socket if an ipv6 socket is configured.
    pub fn with_ipv4(self, ip: Ipv4Addr, port: u16) -> ListenConfig {
        match self {
            ListenConfig::Ipv4 { .. } => ListenConfig::Ipv4 { ip, port },
            ListenConfig::Ipv6 { ip: ipv6, port: ipv6_port } => {
                ListenConfig::DualStack { ipv4: ip, ipv4_port: port, ipv6, ipv6_port }
            }
            ListenConfig::DualStack { ipv6, ipv6_port, .. } => {
                ListenConfig::DualStack { ipv4: ip, ipv4_port: port, ipv6, ipv6_port }
            }
        }
    }

    // Overrides the ipv6 address and port of ipv6 and dual stack configurations. Ipv4
    // configurations are added ipv6 info making them into dual stack configs.
    /// Sets an ipv6 socket. This will override any past ipv6 configuration and will promote the configuration to dual socket if an ipv4 socket is configured.
    pub fn with_ipv6(self, ip: Ipv6Addr, port: u16) -> ListenConfig {
        match self {
            ListenConfig::Ipv6 { .. } => ListenConfig::Ipv6 { ip, port },
            ListenConfig::Ipv4 { ip: ipv4, port: ipv4_port } => {
                ListenConfig::DualStack { ipv4, ipv4_port, ipv6: ip, ipv6_port: port }
            }
            ListenConfig::DualStack { ipv4, ipv4_port, .. } => {
                ListenConfig::DualStack { ipv4, ipv4_port, ipv6: ip, ipv6_port: port }
            }
        }
    }
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: 9000 }
    }
}

impl From<SocketAddr> for ListenConfig {
    fn from(socket_addr: SocketAddr) -> Self {
        match socket_addr {
            SocketAddr::V4(socket) => ListenConfig::Ipv4 { ip: *socket.ip(), port: socket.port() },
            SocketAddr::V6(socket) => ListenConfig::Ipv6 { ip: *socket.ip(), port: socket.port() },
        }
    }
}

impl Drop for Socket {
    // close the send/recv handlers
    fn drop(&mut self) {
        let _ = self.sender_exit.take().expect("Exit always exists").send(());
        let _ = self.recv_exit.take().expect("Exit always exists").send(());
    }
}
