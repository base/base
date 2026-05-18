//! An implementation of the [`ConnectionGate`] trait.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, ToSocketAddrs},
    time::Duration,
};

use ipnet::IpNet;
use libp2p::{Multiaddr, PeerId};
use tokio::time::Instant;

use crate::{ConnectionError, ConnectionGate, Metrics};

/// Policy for connection checks when DNS resolution fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsResolutionFailure {
    /// Allow the connection attempt to proceed.
    Allow,
    /// Reject the connection attempt.
    Reject,
}

/// Dial information tracking for peer connection management.
///
/// Tracks connection attempt statistics for rate limiting and connection gating.
/// Used to prevent excessive connection attempts to the same peer within a
/// configured time window.
#[derive(Debug, Clone)]
pub struct DialInfo {
    /// Number of times the peer has been dialed during the current dial period.
    /// This number is reset once the last time the peer was dialed is longer than the dial period.
    pub num_dials: u64,
    /// The last time the peer was dialed.
    pub last_dial: Instant,
}

impl Default for DialInfo {
    fn default() -> Self {
        Self { num_dials: 0, last_dial: Instant::now() }
    }
}

/// Configuration parameters for the connection gater.
///
/// Controls rate limiting, connection management, and peer protection policies
/// to maintain network health and prevent abuse.
#[derive(Debug, Clone)]
pub struct GaterConfig {
    /// Maximum number of connection attempts per dial period for a single peer.
    ///
    /// If set to `None`, redialing is disabled (after one dial, further dials
    /// are blocked until the dial period expires). If set to `Some(0)`,
    /// redialing is unlimited.
    pub peer_redialing: Option<u64>,

    /// Duration of the rate limiting window for peer connections.
    ///
    /// A peer cannot be dialed more than `peer_redialing` times during this
    /// period. The period resets after this duration has elapsed since the
    /// last dial attempt. Default is 1 hour.
    pub dial_period: Duration,
}

impl Default for GaterConfig {
    fn default() -> Self {
        Self { peer_redialing: None, dial_period: Duration::from_secs(60 * 60) }
    }
}

/// Connection Gater
///
/// A connection gate that regulates peer connections for the libp2p gossip swarm.
///
/// An implementation of the [`ConnectionGate`] trait.
#[derive(Default, Debug, Clone)]
pub struct ConnectionGater {
    /// The configuration for the connection gater.
    config: GaterConfig,
    /// A set of [`PeerId`]s that are currently being dialed.
    pub current_dials: HashSet<PeerId>,
    /// A mapping from [`Multiaddr`] to the dial info for the peer.
    pub dialed_peers: HashMap<Multiaddr, DialInfo>,
    /// A set of protected peers that cannot be disconnected.
    ///
    /// Protecting a peer prevents the peer from any redial thresholds or peer scoring.
    pub protected_peers: HashSet<PeerId>,
    /// A set of blocked peer ids.
    pub blocked_peers: HashSet<PeerId>,
    /// A set of blocked ip addresses that cannot be dialed.
    pub blocked_addrs: HashSet<IpAddr>,
    /// A set of blocked subnets that cannot be connected to.
    pub blocked_subnets: HashSet<IpNet>,
}

impl ConnectionGater {
    /// Creates a new instance of the `ConnectionGater`.
    pub fn new(config: GaterConfig) -> Self {
        Self {
            config,
            current_dials: HashSet::new(),
            dialed_peers: HashMap::new(),
            protected_peers: HashSet::new(),
            blocked_peers: HashSet::new(),
            blocked_addrs: HashSet::new(),
            blocked_subnets: HashSet::new(),
        }
    }

    /// Returns if the given [`Multiaddr`] has been dialed the maximum number of times.
    pub fn dial_threshold_reached(&self, addr: &Multiaddr) -> bool {
        // If the peer has not been dialed yet, the threshold is not reached.
        let Some(dialed) = self.dialed_peers.get(addr) else {
            return false;
        };
        // If the peer has been dialed and the threshold is not set, the threshold is reached.
        let Some(redialing) = self.config.peer_redialing else {
            return true;
        };
        // If the threshold is set to `0`, redial indefinitely.
        if redialing == 0 {
            return false;
        }
        if dialed.num_dials >= redialing {
            return true;
        }
        false
    }

    fn dial_period_expired(&self, addr: &Multiaddr) -> bool {
        let Some(dial_info) = self.dialed_peers.get(addr) else {
            return false;
        };
        dial_info.last_dial.elapsed() > self.config.dial_period
    }

    /// Gets the [`PeerId`] from a given [`Multiaddr`].
    pub fn peer_id_from_addr(addr: &Multiaddr) -> Option<PeerId> {
        addr.iter().find_map(|component| match component {
            libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
            _ => None,
        })
    }

    /// Constructs the [`IpAddr`] from the given [`Multiaddr`].
    pub fn ip_from_addr(addr: &Multiaddr) -> Option<IpAddr> {
        addr.iter().find_map(|component| match component {
            libp2p::multiaddr::Protocol::Ip4(ip) => Some(IpAddr::V4(ip)),
            libp2p::multiaddr::Protocol::Ip6(ip) => Some(IpAddr::V6(ip)),
            _ => None,
        })
    }

    /// Attempts to resolve a DNS-based [`Multiaddr`] to an [`IpAddr`].
    ///
    /// Returns:
    /// - `None` if the multiaddr does not contain a DNS component (use [`Self::ip_from_addr`])
    /// - `Some(Err(()))` if DNS resolution failed
    /// - `Some(Ok(ip))` if DNS resolution succeeded
    ///
    /// Respects the DNS protocol type: `dns4` only returns `IPv4`, `dns6` only returns `IPv6`.
    pub fn try_resolve_dns(addr: &Multiaddr) -> Option<Result<IpAddr, ()>> {
        // Track which DNS protocol type was used
        let (hostname, ipv4_only, ipv6_only) =
            addr.iter().find_map(|component| match component {
                libp2p::multiaddr::Protocol::Dns(h) | libp2p::multiaddr::Protocol::Dnsaddr(h) => {
                    Some((h.to_string(), false, false))
                }
                libp2p::multiaddr::Protocol::Dns4(h) => Some((h.to_string(), true, false)),
                libp2p::multiaddr::Protocol::Dns6(h) => Some((h.to_string(), false, true)),
                _ => None,
            })?;

        debug!(target: "p2p", %hostname, ipv4_only, ipv6_only, "Resolving DNS hostname");

        let ip = match format!("{hostname}:0").to_socket_addrs() {
            Ok(addrs) => {
                // Filter addresses based on DNS protocol type
                addrs.map(|socket_addr| socket_addr.ip()).find(|ip| {
                    if ipv4_only {
                        ip.is_ipv4()
                    } else if ipv6_only {
                        ip.is_ipv6()
                    } else {
                        true
                    }
                })
            }
            Err(e) => {
                warn!(target: "p2p", %hostname, error = %e, "DNS resolution failed");
                return Some(Err(()));
            }
        };

        ip.map_or_else(
            || {
                warn!(target: "p2p", %hostname, "DNS resolution returned no matching addresses");
                Some(Err(()))
            },
            |resolved_ip| {
                debug!(target: "p2p", %hostname, %resolved_ip, "DNS resolution successful");
                Some(Ok(resolved_ip))
            },
        )
    }

    /// Checks if a given [`IpAddr`] is within any of the `blocked_subnets`.
    pub fn check_ip_in_blocked_subnets(&self, ip_addr: &IpAddr) -> bool {
        for subnet in &self.blocked_subnets {
            if subnet.contains(ip_addr) {
                return true;
            }
        }
        false
    }

    /// Gets the [`IpAddr`] used for blocklist checks from a given [`Multiaddr`].
    ///
    /// Returns `Ok(None)` when DNS resolution fails. Callers choose whether to allow or reject
    /// unresolved DNS addresses based on the connection direction.
    pub fn blocklist_ip_from_addr(addr: &Multiaddr) -> Result<Option<IpAddr>, ConnectionError> {
        match Self::try_resolve_dns(addr) {
            Some(Ok(ip)) => Ok(Some(ip)),
            Some(Err(())) => Ok(None),
            None => Self::ip_from_addr(addr)
                .map(Some)
                .ok_or_else(|| ConnectionError::InvalidIpAddress { addr: addr.clone() }),
        }
    }

    /// Checks shared peer, address, and subnet blocklists for the given peer and address.
    ///
    /// Outbound checks allow unresolved DNS addresses so libp2p can handle resolution at the
    /// transport layer. Inbound checks reject unresolved DNS addresses because listener endpoints
    /// should have IP-based remote addresses.
    pub fn can_connect(
        &self,
        peer_id: &PeerId,
        addr: &Multiaddr,
        dns_resolution_failure: DnsResolutionFailure,
    ) -> Result<(), ConnectionError> {
        if self.blocked_peers.contains(peer_id) {
            return Err(ConnectionError::PeerBlocked { peer_id: *peer_id });
        }

        let Some(ip_addr) = Self::blocklist_ip_from_addr(addr)? else {
            return match dns_resolution_failure {
                DnsResolutionFailure::Allow => {
                    debug!(target: "gossip", addr = ?addr, "DNS resolution failed, allowing connection");
                    Ok(())
                }
                DnsResolutionFailure::Reject => {
                    warn!(target: "gossip", addr = ?addr, "DNS resolution failed, rejecting connection");
                    Err(ConnectionError::InvalidIpAddress { addr: addr.clone() })
                }
            };
        };

        if self.blocked_addrs.contains(&ip_addr) {
            return Err(ConnectionError::AddressBlocked { ip: ip_addr });
        }

        if self.check_ip_in_blocked_subnets(&ip_addr) {
            return Err(ConnectionError::SubnetBlocked { ip: ip_addr });
        }

        Ok(())
    }
}

impl ConnectionGate for ConnectionGater {
    fn can_connect_outbound(&mut self, addr: &Multiaddr) -> Result<(), ConnectionError> {
        // Get the peer id from the given multiaddr.
        let peer_id = Self::peer_id_from_addr(addr).ok_or_else(|| {
            warn!(target: "p2p", peer=?addr, "Failed to extract PeerId from Multiaddr");
            Metrics::dial_peer_error("invalid_multiaddr").increment(1.0);
            ConnectionError::InvalidMultiaddr { addr: addr.clone() }
        })?;

        // Cannot dial a peer that is already being dialed.
        if self.current_dials.contains(&peer_id) {
            debug!(target: "gossip", peer=?addr, "Already dialing peer, not dialing");
            Metrics::dial_peer_error("already_dialing").increment(1.0);
            return Err(ConnectionError::AlreadyDialing { peer_id });
        }

        // If the peer is protected, do not apply thresholds.
        let protected = self.protected_peers.contains(&peer_id);

        // If the peer is not protected, its dial threshold is reached and dial period is not
        // expired, do not dial.
        if !protected && self.dial_threshold_reached(addr) && !self.dial_period_expired(addr) {
            debug!(target: "gossip", peer=?addr, "Dial threshold reached, not dialing");
            Metrics::dial_peer_error("threshold_reached").increment(1.0);
            return Err(ConnectionError::ThresholdReached { addr: addr.clone() });
        }

        if let Err(error) = self.can_connect(&peer_id, addr, DnsResolutionFailure::Allow) {
            match &error {
                ConnectionError::PeerBlocked { .. } => {
                    debug!(target: "gossip", peer=?addr, "Peer is blocked, not dialing");
                    Metrics::dial_peer_error("blocked_peer").increment(1.0);
                }
                ConnectionError::InvalidIpAddress { .. } => {
                    warn!(target: "p2p", peer=?addr, "Failed to extract IpAddr from Multiaddr");
                }
                ConnectionError::AddressBlocked { ip } => {
                    debug!(target: "gossip", peer=?addr, ip = %ip, "Address is blocked, not dialing");
                    Metrics::dial_peer_error("blocked_address").increment(1.0);
                }
                ConnectionError::SubnetBlocked { ip } => {
                    debug!(target: "gossip", ip = %ip, "IP address is in a blocked subnet, not dialing");
                    Metrics::dial_peer_error("blocked_subnet").increment(1.0);
                }
                _ => {}
            }
            return Err(error);
        }

        Ok(())
    }

    fn can_connect_inbound(
        &mut self,
        peer_id: &PeerId,
        addr: &Multiaddr,
    ) -> Result<(), ConnectionError> {
        if let Err(error) = self.can_connect(peer_id, addr, DnsResolutionFailure::Reject) {
            match &error {
                ConnectionError::PeerBlocked { .. } => {
                    debug!(target: "gossip", peer = %peer_id, addr = ?addr, "Inbound peer is blocked");
                    Metrics::inbound_connection_error("blocked_peer").increment(1.0);
                }
                ConnectionError::InvalidIpAddress { .. } => {
                    warn!(target: "p2p", addr = ?addr, peer = %peer_id, "Failed to extract IpAddr from inbound Multiaddr");
                    Metrics::inbound_connection_error("invalid_ip_address").increment(1.0);
                }
                ConnectionError::AddressBlocked { ip } => {
                    debug!(target: "gossip", peer = %peer_id, ip = %ip, "Inbound address is blocked");
                    Metrics::inbound_connection_error("blocked_address").increment(1.0);
                }
                ConnectionError::SubnetBlocked { ip } => {
                    debug!(target: "gossip", peer = %peer_id, ip = %ip, "Inbound address is in a blocked subnet");
                    Metrics::inbound_connection_error("blocked_subnet").increment(1.0);
                }
                _ => {}
            }
            return Err(error);
        }

        Ok(())
    }

    fn list_protected_peers(&self) -> Vec<PeerId> {
        self.protected_peers.iter().copied().collect()
    }

    fn dialing(&mut self, addr: &Multiaddr) {
        if let Some(peer_id) = Self::peer_id_from_addr(addr) {
            self.current_dials.insert(peer_id);
        } else {
            warn!(target: "p2p", peer=?addr, "Failed to extract PeerId from Multiaddr when dialing");
        }
    }

    fn dialed(&mut self, addr: &Multiaddr) {
        let dial_info = self
            .dialed_peers
            .entry(addr.clone())
            .or_insert_with(|| DialInfo { num_dials: 0, last_dial: Instant::now() });

        // If the last dial was longer than the dial period, reset the number of dials.
        if dial_info.last_dial.elapsed() > self.config.dial_period {
            dial_info.num_dials = 0;
        }

        dial_info.num_dials += 1;
        dial_info.last_dial = Instant::now();
        trace!(target: "gossip", peer=?addr, count = dial_info.num_dials, "Dialed peer");
    }

    fn remove_dial(&mut self, peer_id: &PeerId) {
        self.current_dials.remove(peer_id);
    }

    fn can_disconnect(&self, addr: &Multiaddr) -> bool {
        let Some(peer_id) = Self::peer_id_from_addr(addr) else {
            warn!(target: "p2p", peer=?addr, "Failed to extract PeerId from Multiaddr when checking disconnect");
            // If we cannot extract the PeerId, disconnection is allowed.
            return true;
        };
        // If the peer is protected, do not disconnect.
        if !self.protected_peers.contains(&peer_id) {
            return true;
        }
        // Peer is protected, cannot disconnect.
        false
    }

    fn block_peer(&mut self, peer_id: &PeerId) {
        self.blocked_peers.insert(*peer_id);
        debug!(target: "gossip", peer=?peer_id, "Blocked peer");
    }

    fn unblock_peer(&mut self, peer_id: &PeerId) {
        self.blocked_peers.remove(peer_id);
        debug!(target: "gossip", peer=?peer_id, "Unblocked peer");
    }

    fn list_blocked_peers(&self) -> Vec<PeerId> {
        self.blocked_peers.iter().copied().collect()
    }

    fn block_addr(&mut self, ip: IpAddr) {
        self.blocked_addrs.insert(ip);
        debug!(target: "gossip", ?ip, "Blocked ip address");
    }

    fn unblock_addr(&mut self, ip: IpAddr) {
        self.blocked_addrs.remove(&ip);
        debug!(target: "gossip", ?ip, "Unblocked ip address");
    }

    fn list_blocked_addrs(&self) -> Vec<IpAddr> {
        self.blocked_addrs.iter().copied().collect()
    }

    fn block_subnet(&mut self, subnet: IpNet) {
        self.blocked_subnets.insert(subnet);
        debug!(target: "gossip", ?subnet, "Blocked subnet");
    }

    fn unblock_subnet(&mut self, subnet: IpNet) {
        self.blocked_subnets.remove(&subnet);
        debug!(target: "gossip", ?subnet, "Unblocked subnet");
    }

    fn list_blocked_subnets(&self) -> Vec<IpNet> {
        self.blocked_subnets.iter().copied().collect()
    }

    fn protect_peer(&mut self, peer_id: PeerId) {
        self.protected_peers.insert(peer_id);
        debug!(target: "gossip", peer=?peer_id, "Protected peer");
    }

    fn unprotect_peer(&mut self, peer_id: PeerId) {
        self.protected_peers.remove(&peer_id);
        debug!(target: "gossip", peer=?peer_id, "Unprotected peer");
    }

    fn prune(&mut self) {
        let period = self.config.dial_period;
        let before = self.dialed_peers.len();
        self.dialed_peers.retain(|_, info| info.last_dial.elapsed() <= period);
        let removed = before - self.dialed_peers.len();
        if removed > 0 {
            trace!(
                target: "gossip",
                removed,
                retained = self.dialed_peers.len(),
                "pruned expired dialed_peers"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_check_ip_in_blocked_subnets_ipv4() {
        let mut gater = ConnectionGater::new(GaterConfig {
            peer_redialing: None,
            dial_period: Duration::from_secs(60 * 60),
        });
        gater.blocked_subnets.insert("192.168.1.0/24".parse::<IpNet>().unwrap());
        gater.blocked_subnets.insert("10.0.0.0/8".parse::<IpNet>().unwrap());
        gater.blocked_subnets.insert("172.16.0.0/16".parse::<IpNet>().unwrap());

        // IP in blocked subnet
        assert!(gater.check_ip_in_blocked_subnets(&IpAddr::from_str("192.168.1.100").unwrap()));
        assert!(gater.check_ip_in_blocked_subnets(&IpAddr::from_str("10.0.0.5").unwrap()));
        assert!(gater.check_ip_in_blocked_subnets(&IpAddr::from_str("172.16.255.255").unwrap()));

        // IP not in any blocked subnet
        assert!(!gater.check_ip_in_blocked_subnets(&IpAddr::from_str("192.168.2.1").unwrap()));
        assert!(!gater.check_ip_in_blocked_subnets(&IpAddr::from_str("172.17.0.1").unwrap()));
        assert!(!gater.check_ip_in_blocked_subnets(&IpAddr::from_str("8.8.8.8").unwrap()));
    }

    #[test]
    fn test_connection_error_handling() {
        let mut gater = ConnectionGater::new(GaterConfig::default());

        // Test invalid multiaddr (missing peer ID)
        let invalid_addr = Multiaddr::from_str("/ip4/127.0.0.1/tcp/8080").unwrap();
        let result = gater.can_connect_outbound(&invalid_addr);
        assert!(matches!(result, Err(ConnectionError::InvalidMultiaddr { .. })));

        // Test with valid address
        let valid_addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        // First dial should succeed
        assert!(gater.can_connect_outbound(&valid_addr).is_ok());

        // Mark as dialing
        gater.dialing(&valid_addr);

        // Second dial should fail with AlreadyDialing
        let result = gater.can_connect_outbound(&valid_addr);
        assert!(matches!(result, Err(ConnectionError::AlreadyDialing { .. })));
    }

    #[test]
    fn test_dns_multiaddr_detection() {
        // Test DNS4 multiaddr (try_resolve_dns returns Some for DNS addresses)
        let dns4_addr = Multiaddr::from_str(
            "/dns4/example.com/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&dns4_addr).is_some());

        // Test DNS6 multiaddr
        let dns6_addr = Multiaddr::from_str(
            "/dns6/example.com/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&dns6_addr).is_some());

        // Test DNS multiaddr (generic)
        let dns_addr = Multiaddr::from_str(
            "/dns/example.com/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&dns_addr).is_some());

        // Test dnsaddr multiaddr
        let dnsaddr = Multiaddr::from_str(
            "/dnsaddr/example.com/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&dnsaddr).is_some());

        // Test IP4 multiaddr (should NOT be detected as DNS - returns None)
        let ip4_addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&ip4_addr).is_none());

        // Test IP6 multiaddr (should NOT be detected as DNS - returns None)
        let ip6_addr = Multiaddr::from_str(
            "/ip6/::1/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(ConnectionGater::try_resolve_dns(&ip6_addr).is_none());
    }

    #[test]
    fn test_dns_multiaddr_can_connect_outbound() {
        let mut gater = ConnectionGater::new(GaterConfig::default());

        // DNS4 multiaddr should be allowed to dial (IP checks skipped)
        let dns4_addr = Multiaddr::from_str(
            "/dns4/example.com/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();
        assert!(gater.can_connect_outbound(&dns4_addr).is_ok());

        // Real-world DNS multiaddr format (like the one from the issue)
        let real_world_dns = Multiaddr::from_str(
            "/dns4/alfonso-0-opn-reth-a-rpc-1-p2p.primary.infra.dev.oplabs.cloud/tcp/9003/p2p/16Uiu2HAmUSo81N6iNQNKZCiqDAg5Mcmh9gwvPgKmKj1HH6qCR4Kq",
        )
        .unwrap();
        assert!(gater.can_connect_outbound(&real_world_dns).is_ok());

        // DNS multiaddr with blocked peer should still be blocked
        let peer_id = ConnectionGater::peer_id_from_addr(&dns4_addr).unwrap();
        gater.block_peer(&peer_id);
        assert!(gater.can_connect_outbound(&dns4_addr).is_err());
    }

    #[test]
    fn test_dns_multiaddr_blocked_by_resolved_ip() {
        let mut gater = ConnectionGater::new(GaterConfig::default());

        let dns_localhost = Multiaddr::from_str(
            "/dns4/localhost/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        assert!(gater.can_connect_outbound(&dns_localhost).is_ok());

        gater.block_addr(IpAddr::from_str("127.0.0.1").unwrap());

        let result = gater.can_connect_outbound(&dns_localhost);
        assert!(matches!(result, Err(ConnectionError::AddressBlocked { .. })));
    }

    #[test]
    fn test_dns_multiaddr_blocked_by_subnet() {
        let mut gater = ConnectionGater::new(GaterConfig::default());

        let dns_localhost = Multiaddr::from_str(
            "/dns4/localhost/tcp/9003/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        assert!(gater.can_connect_outbound(&dns_localhost).is_ok());

        gater.block_subnet("127.0.0.0/8".parse().unwrap());

        let result = gater.can_connect_outbound(&dns_localhost);
        assert!(matches!(result, Err(ConnectionError::SubnetBlocked { .. })));
    }

    #[test]
    fn test_inbound_blocked_peer() {
        let mut gater = ConnectionGater::new(GaterConfig::default());
        let addr = Multiaddr::from_str("/ip4/127.0.0.1/tcp/9003").unwrap();
        let peer_id: PeerId =
            "12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp".parse().unwrap();

        gater.block_peer(&peer_id);

        let result = gater.can_connect_inbound(&peer_id, &addr);
        assert!(matches!(result, Err(ConnectionError::PeerBlocked { .. })));
    }

    #[test]
    fn test_inbound_blocked_address() {
        let mut gater = ConnectionGater::new(GaterConfig::default());
        let addr = Multiaddr::from_str("/ip4/127.0.0.1/tcp/9003").unwrap();
        let peer_id: PeerId =
            "12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp".parse().unwrap();

        gater.block_addr(IpAddr::from_str("127.0.0.1").unwrap());

        let result = gater.can_connect_inbound(&peer_id, &addr);
        assert!(matches!(result, Err(ConnectionError::AddressBlocked { .. })));
    }

    #[test]
    fn test_inbound_blocked_subnet() {
        let mut gater = ConnectionGater::new(GaterConfig::default());
        let addr = Multiaddr::from_str("/ip4/127.0.0.1/tcp/9003").unwrap();
        let peer_id: PeerId =
            "12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp".parse().unwrap();

        gater.block_subnet("127.0.0.0/8".parse().unwrap());

        let result = gater.can_connect_inbound(&peer_id, &addr);
        assert!(matches!(result, Err(ConnectionError::SubnetBlocked { .. })));
    }

    #[test]
    fn test_peer_redialing_none_disables_redials() {
        let mut gater = ConnectionGater::new(GaterConfig {
            peer_redialing: None,
            dial_period: Duration::from_secs(60 * 60),
        });
        let addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        assert!(!gater.dial_threshold_reached(&addr));
        gater.dialed(&addr);
        assert!(gater.dial_threshold_reached(&addr));
    }

    #[test]
    fn test_peer_redialing_zero_allows_unlimited_redials() {
        let mut gater = ConnectionGater::new(GaterConfig {
            peer_redialing: Some(0),
            dial_period: Duration::from_secs(60 * 60),
        });
        let addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        gater.dialed(&addr);
        gater.dialed(&addr);
        gater.dialed(&addr);
        assert!(!gater.dial_threshold_reached(&addr));
    }

    #[test]
    fn test_prune_removes_expired_dialed_peers() {
        let mut gater = ConnectionGater::new(GaterConfig {
            dial_period: Duration::from_millis(1),
            ..GaterConfig::default()
        });
        let addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        gater.dialed(&addr);
        assert_eq!(gater.dialed_peers.len(), 1);

        // Wait for dial_period to expire.
        std::thread::sleep(Duration::from_millis(5));

        gater.prune();
        assert_eq!(gater.dialed_peers.len(), 0, "expired entry should be pruned");
    }

    #[test]
    fn test_prune_retains_fresh_dialed_peers() {
        let mut gater = ConnectionGater::new(GaterConfig {
            dial_period: Duration::from_secs(3600),
            ..GaterConfig::default()
        });
        let addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        gater.dialed(&addr);
        gater.prune();
        assert_eq!(gater.dialed_peers.len(), 1, "fresh entry should not be pruned");
    }

    #[test]
    fn test_prune_drains_many_expired_entries() {
        let mut gater = ConnectionGater::new(GaterConfig {
            dial_period: Duration::from_millis(1),
            ..GaterConfig::default()
        });

        // Dial 50 unique addresses.
        for i in 0..50u8 {
            let addr = Multiaddr::from_str(&format!(
                "/ip4/10.0.0.{}/tcp/8080/p2p/{}",
                i,
                PeerId::random()
            ))
            .unwrap();
            gater.dialed(&addr);
        }
        assert_eq!(gater.dialed_peers.len(), 50);

        // Wait for dial_period to expire.
        std::thread::sleep(Duration::from_millis(5));

        gater.prune();
        assert_eq!(
            gater.dialed_peers.len(),
            0,
            "all expired dialed_peers entries should be pruned"
        );
    }

    /// Simulates the full lifecycle that the network actor's control loop exercises:
    /// dial → threshold reached → period expires → prune → peer is dialable again.
    #[test]
    fn test_prune_restores_dialability_after_expiry() {
        let mut gater = ConnectionGater::new(GaterConfig {
            peer_redialing: None,
            dial_period: Duration::from_millis(1),
        });
        let addr = Multiaddr::from_str(
            "/ip4/127.0.0.1/tcp/8080/p2p/12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp",
        )
        .unwrap();

        // First outbound check passes.
        assert!(gater.can_connect_outbound(&addr).is_ok());

        // Simulate successful dial.
        gater.dialing(&addr);
        gater.dialed(&addr);
        gater.remove_dial(&ConnectionGater::peer_id_from_addr(&addr).unwrap());

        // Threshold reached: second outbound check is rejected.
        assert!(
            matches!(
                gater.can_connect_outbound(&addr),
                Err(ConnectionError::ThresholdReached { .. })
            ),
            "dial should be rejected while threshold is reached and period has not expired"
        );

        // Wait for dial_period to expire, then prune.
        std::thread::sleep(Duration::from_millis(5));
        gater.prune();

        // After prune, the peer is dialable again.
        assert!(
            gater.can_connect_outbound(&addr).is_ok(),
            "dial should succeed after prune clears expired dialed_peers entry"
        );
        assert_eq!(gater.dialed_peers.len(), 0, "dialed_peers should be empty after prune");
    }
}
