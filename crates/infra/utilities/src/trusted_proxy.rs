//! Client IP resolution through trusted forwarding proxies.

use std::net::IpAddr;

use http::HeaderMap;
use ipnet::IpNet;
use tracing::warn;

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

    /// Resolves the client IP, trusting forwarding headers only from configured proxy CIDRs.
    pub fn client_ip(&self, connect_addr: IpAddr, headers: &HeaderMap) -> IpAddr {
        // Dual-stack listeners present IPv4 peers as IPv4-mapped IPv6 (`::ffff:x.x.x.x`).
        // Canonicalize so IPv4 CIDRs still match those peers and rate-limit buckets stay
        // consistent across address forms.
        let connect_addr = connect_addr.to_canonical();

        if !self.trusted_proxy_cidrs.iter().any(|cidr| cidr.contains(&connect_addr)) {
            return connect_addr;
        }

        let Some(header) = headers.get(&self.ip_addr_http_header) else {
            return connect_addr;
        };

        let header_value = match header.to_str() {
            Ok(header_value) => header_value,
            Err(error) => {
                warn!(error = %error, "Could not read client IP header");
                return connect_addr;
            }
        };

        header_value
            .split(',')
            .next_back()
            .and_then(|ip| {
                let trimmed = ip.trim();
                match trimmed.parse::<IpAddr>() {
                    Ok(addr) => Some(addr.to_canonical()),
                    Err(error) => {
                        warn!(
                            error = %error,
                            value = %trimmed,
                            "Failed to parse forwarded client IP"
                        );
                        None
                    }
                }
            })
            .unwrap_or(connect_addr)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use http::{HeaderMap, HeaderValue};

    use super::TrustedProxyConfig;

    #[test]
    fn trusted_proxy_config_resolves_client_ip() {
        let config = TrustedProxyConfig::new(
            "x-forwarded-for".to_string(),
            vec!["127.0.0.0/8".parse().unwrap()],
        );
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
    fn trusted_proxy_config_matches_ipv4_mapped_peers() {
        let config = TrustedProxyConfig::new(
            "x-forwarded-for".to_string(),
            vec!["10.0.0.0/8".parse().unwrap()],
        );
        let mapped_proxy = IpAddr::V6("::ffff:10.0.0.1".parse().unwrap());
        let client = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));

        assert_eq!(config.client_ip(mapped_proxy, &headers), client);

        headers.insert("x-forwarded-for", HeaderValue::from_static("::ffff:203.0.113.10"));
        assert_eq!(config.client_ip(mapped_proxy, &headers), client);
    }
}
