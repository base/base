//! Commonly used `NodeRecord` type for peers.
//!
//! This is a simplified version of the `NodeRecord` type in reth.
//!
//! Adapted from <https://github.com/paradigmxyz/reth/blob/0e087ae1c35502f0b8d128c64e4c57269af20c0e/crates/net/peers/src/node_record.rs>.

use core::{
    fmt,
    fmt::Write,
    net::{IpAddr, SocketAddr},
    num::ParseIntError,
    str::FromStr,
};

use crate::{EnodeUrl, PeerId};

/// Represents an ENR in discovery.
///
/// Note: this is only an excerpt of the [`NodeRecord`] data structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NodeRecord {
    /// The Address of a node.
    pub address: IpAddr,
    /// UDP discovery port.
    pub udp_port: u16,
    /// TCP port of the port that accepts connections.
    pub tcp_port: u16,
    /// Public key of the discovery service
    pub id: PeerId,
}

impl NodeRecord {
    /// The TCP socket address of this node
    #[must_use]
    pub const fn tcp_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.tcp_port)
    }

    /// The UDP socket address of this node
    #[must_use]
    pub const fn udp_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.udp_port)
    }
}

impl fmt::Display for NodeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("enode://")?;
        alloy_primitives::hex::encode(self.id.as_slice()).fmt(f)?;
        f.write_char('@')?;
        match self.address {
            IpAddr::V4(ip) => {
                ip.fmt(f)?;
            }
            IpAddr::V6(ip) => {
                // encapsulate with brackets
                f.write_char('[')?;
                ip.fmt(f)?;
                f.write_char(']')?;
            }
        }
        f.write_char(':')?;
        self.tcp_port.fmt(f)?;
        if self.tcp_port != self.udp_port {
            f.write_str("?discport=")?;
            self.udp_port.fmt(f)?;
        }

        Ok(())
    }
}

/// Possible error types when parsing a [`NodeRecord`]
#[derive(Debug, thiserror::Error)]
pub enum NodeRecordParseError {
    /// Invalid url
    #[error("Failed to parse url: {0}")]
    InvalidUrl(String),
    /// Invalid id
    #[error("Failed to parse id")]
    InvalidId(String),
    /// Invalid discport
    #[error("Failed to discport query: {0}")]
    Discport(ParseIntError),
    /// The host is a DNS hostname that has not been resolved.
    #[error("Host {0} is a hostname and must be resolved first")]
    UnresolvedHost(String),
}

impl FromStr for NodeRecord {
    type Err = NodeRecordParseError;

    /// Parses an `enode://` URL with an IP address host.
    ///
    /// Hostnames are rejected with [`NodeRecordParseError::UnresolvedHost`]; parse them as an
    /// [`EnodeUrl`] and resolve them with [`EnodeUrl::resolve`] instead.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let url = EnodeUrl::from_str(s)?;
        url.node_record().ok_or_else(|| NodeRecordParseError::UnresolvedHost(url.host.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_parse_domain_requires_resolution() {
        // Parsing must not perform DNS; `.invalid` never resolves (RFC 6761).
        let url = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@bootnode.invalid:30303?discport=30301";
        let err = url.parse::<NodeRecord>().unwrap_err();
        assert!(
            matches!(err, NodeRecordParseError::UnresolvedHost(host) if host == "bootnode.invalid")
        );
    }

    #[test]
    fn test_url_parse() {
        let url = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@10.3.58.6:30303?discport=30301";
        let node: NodeRecord = url.parse().unwrap();
        assert_eq!(node, NodeRecord {
            address: IpAddr::V4([10,3,58,6].into()),
            tcp_port: 30303,
            udp_port: 30301,
            id: "6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0".parse().unwrap(),
        })
    }

    #[test]
    fn test_node_display() {
        let url = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@10.3.58.6:30303";
        let node: NodeRecord = url.parse().unwrap();
        assert_eq!(url, &format!("{node}"));
    }

    #[test]
    fn test_node_display_discport() {
        let url = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@10.3.58.6:30303?discport=30301";
        let node: NodeRecord = url.parse().unwrap();
        assert_eq!(url, &format!("{node}"));
    }
}
