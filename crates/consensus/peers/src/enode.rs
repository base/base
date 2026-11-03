//! Contains [`EnodeUrl`], an `enode://` URL whose host may still be a DNS hostname.

use core::{net::IpAddr, str::FromStr};

use url::{Host, Url};

use crate::{HostResolveError, HostResolver, NodeRecord, NodeRecordParseError, PeerId};

/// An `enode://` URL whose host is an IP address or a DNS hostname.
///
/// This is the consensus-layer counterpart of reth's `TrustedPeer`. Parsing never performs a DNS
/// lookup; [`EnodeUrl::resolve`] turns a hostname into a [`NodeRecord`] through a
/// [`HostResolver`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnodeUrl {
    /// The host: an `IPv4` address, an `IPv6` address, or a DNS hostname.
    pub host: Host,
    /// TCP port that accepts connections.
    pub tcp_port: u16,
    /// UDP discovery port.
    pub udp_port: u16,
    /// Public key of the node.
    pub id: PeerId,
}

impl EnodeUrl {
    /// Returns the [`NodeRecord`] for this URL at the given address.
    pub const fn with_address(&self, address: IpAddr) -> NodeRecord {
        NodeRecord { address, udp_port: self.udp_port, tcp_port: self.tcp_port, id: self.id }
    }

    /// Returns the [`NodeRecord`] if the host is an IP address, or `None` for a hostname.
    pub const fn node_record(&self) -> Option<NodeRecord> {
        match &self.host {
            Host::Ipv4(ip) => Some(self.with_address(IpAddr::V4(*ip))),
            Host::Ipv6(ip) => Some(self.with_address(IpAddr::V6(*ip))),
            Host::Domain(_) => None,
        }
    }

    /// Resolves the host into a [`NodeRecord`].
    ///
    /// IP hosts are returned without calling `resolver`. For a hostname, the first address the
    /// resolver returns is used, matching reth's `TrustedPeer::resolve`.
    pub async fn resolve<R: HostResolver + ?Sized>(
        &self,
        resolver: &R,
    ) -> Result<NodeRecord, HostResolveError> {
        let domain = match &self.host {
            Host::Domain(domain) => domain,
            Host::Ipv4(ip) => return Ok(self.with_address(IpAddr::V4(*ip))),
            Host::Ipv6(ip) => return Ok(self.with_address(IpAddr::V6(*ip))),
        };
        let address = resolver
            .lookup_host(domain)
            .await
            .map_err(HostResolveError::Lookup)?
            .into_iter()
            .next()
            .ok_or(HostResolveError::NoAddresses)?;
        Ok(self.with_address(address))
    }
}

impl FromStr for EnodeUrl {
    type Err = NodeRecordParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(s).map_err(|e| NodeRecordParseError::InvalidUrl(e.to_string()))?;

        let port = url
            .port()
            .ok_or_else(|| NodeRecordParseError::InvalidUrl("no port specified".to_string()))?;

        // `enode` is not a special URL scheme, so the `url` crate reports IPv4 literals as
        // opaque domains. Recover them here so only real hostnames need resolution.
        let host = match url.host() {
            Some(Host::Domain(domain)) => match domain.parse::<IpAddr>() {
                Ok(IpAddr::V4(ip)) => Host::Ipv4(ip),
                Ok(IpAddr::V6(ip)) => Host::Ipv6(ip),
                Err(_) => Host::Domain(domain.to_string()),
            },
            Some(Host::Ipv4(ip)) => Host::Ipv4(ip),
            Some(Host::Ipv6(ip)) => Host::Ipv6(ip),
            None => return Err(NodeRecordParseError::InvalidUrl("no host specified".to_string())),
        };

        let udp_port = if let Some(discovery_port) = url
            .query_pairs()
            .find_map(|(maybe_disc, port)| (maybe_disc.as_ref() == "discport").then_some(port))
        {
            discovery_port.parse::<u16>().map_err(NodeRecordParseError::Discport)?
        } else {
            port
        };

        let id = url
            .username()
            .parse::<PeerId>()
            .map_err(|e| NodeRecordParseError::InvalidId(e.to_string()))?;

        Ok(Self { host, tcp_port: port, udp_port, id })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{Ipv4Addr, Ipv6Addr},
    };

    use super::*;
    use crate::MockHostResolver;

    const ID: &str = "6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0";

    #[test]
    fn parses_hostname_without_dns() {
        // `.invalid` never resolves (RFC 6761), so this only passes if parsing skips DNS.
        let url: EnodeUrl =
            format!("enode://{ID}@bootnode.invalid:9222?discport=9223").parse().unwrap();
        assert_eq!(url.host, Host::Domain("bootnode.invalid".to_string()));
        assert_eq!(url.tcp_port, 9222);
        assert_eq!(url.udp_port, 9223);
        assert_eq!(url.node_record(), None);
    }

    #[test]
    fn parses_ip_literals_as_ip_hosts() {
        let v4: EnodeUrl = format!("enode://{ID}@10.3.58.6:30303").parse().unwrap();
        assert_eq!(v4.host, Host::<String>::Ipv4(Ipv4Addr::new(10, 3, 58, 6)));
        let v6: EnodeUrl = format!("enode://{ID}@[::1]:30303").parse().unwrap();
        assert_eq!(v6.host, Host::<String>::Ipv6(Ipv6Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn resolve_uses_first_resolved_address() {
        let mut resolver = MockHostResolver::new();
        resolver.expect_lookup_host().withf(|host| host == "bootnode.example.org").returning(
            |_| {
                Ok(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), IpAddr::V6(Ipv6Addr::LOCALHOST)])
            },
        );
        let url: EnodeUrl = format!("enode://{ID}@bootnode.example.org:9222").parse().unwrap();

        let record = url.resolve(&resolver).await.unwrap();

        assert_eq!(record, url.with_address(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))));
    }

    #[tokio::test]
    async fn resolve_ip_host_skips_resolver() {
        let mut resolver = MockHostResolver::new();
        resolver.expect_lookup_host().never();
        let url: EnodeUrl = format!("enode://{ID}@10.3.58.6:30303").parse().unwrap();

        let record = url.resolve(&resolver).await.unwrap();

        assert_eq!(record.address, IpAddr::V4(Ipv4Addr::new(10, 3, 58, 6)));
    }

    #[tokio::test]
    async fn resolve_reports_lookup_failure_and_empty_answer() {
        let mut resolver = MockHostResolver::new();
        resolver
            .expect_lookup_host()
            .withf(|host| host == "missing.example.org")
            .returning(|_| Err(io::Error::other("no such host")));
        resolver
            .expect_lookup_host()
            .withf(|host| host == "empty.example.org")
            .returning(|_| Ok(vec![]));

        let missing: EnodeUrl = format!("enode://{ID}@missing.example.org:9222").parse().unwrap();
        let empty: EnodeUrl = format!("enode://{ID}@empty.example.org:9222").parse().unwrap();

        assert!(matches!(missing.resolve(&resolver).await, Err(HostResolveError::Lookup(_))));
        assert!(matches!(empty.resolve(&resolver).await, Err(HostResolveError::NoAddresses)));
    }
}
