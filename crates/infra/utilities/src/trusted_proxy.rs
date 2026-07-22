//! Client IP resolution through trusted forwarding proxies.

use std::net::IpAddr;

use http::HeaderMap;
use ipnet::IpNet;
use tracing::warn;

/// Failure to extract a client IP from a forwarding header.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForwardedClientIpError {
    /// The configured forwarding header is absent from the request.
    #[error("missing client IP header `{header}`")]
    MissingHeader {
        /// The configured header name.
        header: String,
    },
    /// The extracted entry does not parse as an IP address.
    #[error("client IP header `{header}` entry `{value}` is not a valid IP address")]
    InvalidIp {
        /// The configured header name.
        header: String,
        /// The entry that failed to parse.
        value: String,
    },
}

/// Configuration for resolving client IPs through trusted proxies.
#[derive(Clone, Debug)]
pub struct TrustedProxyConfig {
    ip_addr_http_header: String,
    trusted_proxy_cidrs: Vec<IpNet>,
}

impl TrustedProxyConfig {
    /// Creates a trusted proxy configuration for the given header and proxy CIDRs.
    pub const fn new(ip_addr_http_header: String, trusted_proxy_cidrs: Vec<IpNet>) -> Self {
        Self { ip_addr_http_header, trusted_proxy_cidrs }
    }

    /// Returns whether the direct peer belongs to a configured trusted proxy CIDR.
    pub fn is_trusted_proxy(&self, connect_addr: IpAddr) -> bool {
        let connect_addr = connect_addr.to_canonical();
        self.trusted_proxy_cidrs.iter().any(|cidr| cidr.contains(&connect_addr))
    }

    /// Extracts the client IP from the configured forwarding header.
    ///
    /// Callers must verify the direct peer with [`Self::is_trusted_proxy`]
    /// first; forwarding headers from untrusted peers are attacker-controlled.
    ///
    /// Only the **last occurrence** of the header is inspected, and within it
    /// the **rightmost** comma-separated entry wins. Proxies append the real
    /// client address after any client-supplied values (as a new list entry
    /// or a new header line), so the rightmost value of the last line is the
    /// only one the trusted proxy vouches for.
    pub fn forwarded_client_ip(
        &self,
        headers: &HeaderMap,
    ) -> Result<IpAddr, ForwardedClientIpError> {
        let header = &self.ip_addr_http_header;
        let Some(value) = headers.get_all(header).iter().next_back() else {
            return Err(ForwardedClientIpError::MissingHeader { header: header.clone() });
        };
        let value = value.to_str().map_err(|_| ForwardedClientIpError::InvalidIp {
            header: header.clone(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })?;
        let entry = value.rsplit_once(',').map_or(value, |(_, entry)| entry).trim();
        entry.parse::<IpAddr>().map(|ip| ip.to_canonical()).map_err(|_| {
            ForwardedClientIpError::InvalidIp { header: header.clone(), value: entry.to_string() }
        })
    }

    /// Resolves the client IP, trusting forwarding headers only from configured proxy CIDRs.
    ///
    /// Untrusted or direct peers resolve to their (canonicalized) socket
    /// address and forwarding headers are ignored. A trusted proxy must
    /// supply a valid forwarding header; a missing or invalid one surfaces
    /// as an error for the caller to handle.
    pub fn try_client_ip(
        &self,
        connect_addr: IpAddr,
        headers: &HeaderMap,
    ) -> Result<IpAddr, ForwardedClientIpError> {
        // Dual-stack listeners present IPv4 peers as IPv4-mapped IPv6 (`::ffff:x.x.x.x`).
        // Canonicalize so IPv4 CIDRs still match those peers and rate-limit buckets stay
        // consistent across address forms.
        let connect_addr = connect_addr.to_canonical();
        if self.is_trusted_proxy(connect_addr) {
            self.forwarded_client_ip(headers)
        } else {
            Ok(connect_addr)
        }
    }

    /// Resolves the client IP, trusting forwarding headers only from configured proxy CIDRs.
    ///
    /// Unlike [`Self::try_client_ip`], this is lenient: a missing or
    /// invalid header from a trusted proxy falls back to the peer address
    /// with a warning instead of failing.
    pub fn client_ip(&self, connect_addr: IpAddr, headers: &HeaderMap) -> IpAddr {
        let connect_addr = connect_addr.to_canonical();
        match self.try_client_ip(connect_addr, headers) {
            Ok(client_ip) => client_ip,
            Err(error) => {
                warn!(error = %error, peer = %connect_addr, "could not resolve forwarded client IP");
                connect_addr
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use http::{HeaderMap, HeaderValue};

    use super::{ForwardedClientIpError, TrustedProxyConfig};

    fn xff_config(cidrs: Vec<&str>) -> TrustedProxyConfig {
        TrustedProxyConfig::new(
            "x-forwarded-for".to_string(),
            cidrs.into_iter().map(|cidr| cidr.parse().unwrap()).collect(),
        )
    }

    #[test]
    fn trusted_proxy_config_resolves_client_ip() {
        let config = xff_config(vec!["127.0.0.0/8"]);
        let trusted_proxy = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let untrusted_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let client = IpAddr::V4(Ipv4Addr::new(130, 1, 1, 1));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("129.1.1.1, 130.1.1.1"));

        assert_eq!(config.client_ip(trusted_proxy, &headers), client);
        assert_eq!(config.client_ip(untrusted_peer, &headers), untrusted_peer);

        headers.insert("x-forwarded-for", HeaderValue::from_static("nonsense"));
        assert_eq!(config.client_ip(trusted_proxy, &headers), trusted_proxy);

        headers.clear();
        assert_eq!(config.client_ip(trusted_proxy, &headers), trusted_proxy);
    }

    #[test]
    fn last_header_occurrence_wins_over_spoofed_first_line() {
        // A client sends its own X-Forwarded-For line; the trusted proxy
        // appends a second line with the real address. The proxy-appended
        // line must win.
        let config = xff_config(vec!["127.0.0.0/8"]);
        let trusted_proxy = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let real_client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));

        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("203.0.113.99"));
        headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));

        assert_eq!(config.client_ip(trusted_proxy, &headers), real_client);
        assert_eq!(config.forwarded_client_ip(&headers).unwrap(), real_client);
    }

    #[test]
    fn rightmost_entry_of_last_line_wins() {
        let config = xff_config(vec!["127.0.0.0/8"]);
        let real_client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));

        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("203.0.113.1, 203.0.113.2"));
        headers.append("x-forwarded-for", HeaderValue::from_static("203.0.113.3, 198.51.100.7"));

        assert_eq!(config.forwarded_client_ip(&headers).unwrap(), real_client);
    }

    #[test]
    fn empty_trusted_cidrs_always_uses_peer_address() {
        // Direct deployments (no fronting proxy) configure no CIDRs, so
        // forwarding headers are never honored.
        let config = xff_config(vec![]);
        let peer = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.7"));

        assert_eq!(config.client_ip(peer, &headers), peer);
    }

    #[test]
    fn try_client_ip_gates_on_trust() {
        let config = xff_config(vec!["127.0.0.0/8"]);
        let untrusted = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let headers = HeaderMap::new();

        // Untrusted peers resolve to themselves even without a header.
        assert_eq!(config.try_client_ip(untrusted, &headers), Ok(untrusted));
        // Trusted proxies must supply the header.
        assert!(config.try_client_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), &headers).is_err());
    }

    #[test]
    fn forwarded_client_ip_reports_extraction_failures() {
        let config = xff_config(vec!["127.0.0.0/8"]);

        let headers = HeaderMap::new();
        assert_eq!(
            config.forwarded_client_ip(&headers),
            Err(ForwardedClientIpError::MissingHeader { header: "x-forwarded-for".to_string() })
        );

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        assert_eq!(
            config.forwarded_client_ip(&headers),
            Err(ForwardedClientIpError::InvalidIp {
                header: "x-forwarded-for".to_string(),
                value: "not-an-ip".to_string(),
            })
        );

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(
            config.forwarded_client_ip(&headers),
            Err(ForwardedClientIpError::InvalidIp {
                header: "x-forwarded-for".to_string(),
                value: "\u{FFFD}".to_string(),
            })
        );
    }

    #[test]
    fn trusted_proxy_config_matches_ipv4_mapped_peers() {
        let config = xff_config(vec!["10.0.0.0/8"]);
        let mapped_proxy = IpAddr::V6("::ffff:10.0.0.1".parse().unwrap());
        let client = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));

        assert_eq!(config.client_ip(mapped_proxy, &headers), client);

        headers.insert("x-forwarded-for", HeaderValue::from_static("::ffff:203.0.113.10"));
        assert_eq!(config.client_ip(mapped_proxy, &headers), client);
    }
}
