//! A set of configuration parameters to tune the discovery protocol.
use std::net::SocketAddr;

use crate::{
    Enr,
    IpMode::{DualStack, Ip4, Ip6},
    socket::ListenConfig,
};

/// Sets the socket type to be established and also determines the type of ENRs that we will store
/// in our routing table.
/// We store ENR's that have a `get_contractable_addr()` based on the `IpMode` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpMode {
    /// IPv4 only. This creates an IPv4 only UDP socket and will only store ENRs in the local
    /// routing table if they contain a contactable IPv4 address.
    #[default]
    Ip4,
    /// IPv6 only. This creates an IPv6 only UDP socket and will only store ENRs in the local
    /// routing table if they contain a contactable IPv6 address. Mapped addresses will be
    /// disabled.
    Ip6,
    /// Two UDP sockets are in use. One for Ipv4 and one for Ipv6.
    DualStack,
}

impl IpMode {
    pub(crate) fn new_from_listen_config(listen_config: &ListenConfig) -> Self {
        match listen_config {
            ListenConfig::Ipv4 { .. } => Ip4,
            ListenConfig::Ipv6 { .. } => Ip6,
            ListenConfig::DualStack { .. } => DualStack,
        }
    }

    pub fn is_ipv4(&self) -> bool {
        self == &Ip4
    }

    /// Get the contactable Socket address of an Enr under current configuration. When running in
    /// dual stack, an Enr that advertises both an Ipv4 and a canonical Ipv6 address will be
    /// contacted using their Ipv6 address.
    pub fn get_contactable_addr(&self, enr: &Enr) -> Option<SocketAddr> {
        // A function to get a canonical ipv6 address from an Enr

        /// NOTE: There is nothing in the spec preventing compat/mapped addresses from being
        /// transmitted in the ENR. Here we choose to enforce canonical addresses since
        /// it simplifies the logic of matching socket_addr verification. For this we prevent
        /// communications with Ipv4 addresses advertised in the Ipv6 field.
        fn canonical_ipv6_enr_addr(enr: &Enr) -> Option<std::net::SocketAddrV6> {
            enr.udp6_socket().and_then(|socket_addr| {
                if to_ipv4_mapped(socket_addr.ip()).is_some() { None } else { Some(socket_addr) }
            })
        }

        match self {
            Ip4 => enr.udp4_socket().map(SocketAddr::V4),
            Ip6 => canonical_ipv6_enr_addr(enr).map(SocketAddr::V6),
            DualStack => {
                canonical_ipv6_enr_addr(enr)
                    .map(SocketAddr::V6)
                    // NOTE: general consensus is that ipv6 addresses should be preferred.
                    .or_else(|| enr.udp4_socket().map(SocketAddr::V4))
            }
        }
    }
}

/// Copied from the standard library. See <https://github.com/rust-lang/rust/issues/27709>
/// The current code is behind the `ip` feature.
pub const fn to_ipv4_mapped(ip: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    match ip.octets() {
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, a, b, c, d] => {
            Some(std::net::Ipv4Addr::new(a, b, c, d))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    use super::*;

    const IP6_TEST_PORT: u16 = 9000;
    const IP4_TEST_PORT: u16 = 9090;

    /// Structure to make each case clear
    struct TestCase {
        name: &'static str,
        enr_ip4: Option<Ipv4Addr>,
        enr_ip6: Option<Ipv6Addr>,
        ip_mode: IpMode,
        expected_socket_addr: Option<SocketAddr>,
    }

    impl TestCase {
        fn new(name: &'static str) -> TestCase {
            TestCase {
                name,
                enr_ip4: None,
                enr_ip6: None,
                ip_mode: Ip4,
                expected_socket_addr: None,
            }
        }

        fn enr_ip4(&mut self, ip4: Ipv4Addr) -> &mut Self {
            self.enr_ip4 = Some(ip4);
            self
        }

        fn enr_ip6(&mut self, ip6: Ipv6Addr) -> &mut Self {
            self.enr_ip6 = Some(ip6);
            self
        }

        fn ip_mode(&mut self, mode: IpMode) -> &mut Self {
            self.ip_mode = mode;
            self
        }

        fn expect_ip4(&mut self, ip4: Ipv4Addr) -> &mut Self {
            self.expected_socket_addr = Some(SocketAddr::V4(SocketAddrV4::new(ip4, IP4_TEST_PORT)));
            self
        }

        fn expect_ip6(&mut self, ip6: Ipv6Addr) -> &mut Self {
            self.expected_socket_addr =
                Some(SocketAddr::V6(SocketAddrV6::new(ip6, IP6_TEST_PORT, 0, 0)));
            self
        }

        fn test(&self) {
            let test_enr = {
                let builder = &mut enr::Enr::builder();
                if let Some(ip4) = self.enr_ip4 {
                    builder.ip4(ip4).udp4(IP4_TEST_PORT);
                }
                if let Some(ip6) = self.enr_ip6 {
                    builder.ip6(ip6).udp6(IP6_TEST_PORT);
                }
                builder.build(&enr::CombinedKey::generate_secp256k1()).unwrap()
            };

            assert_eq!(
                self.ip_mode.get_contactable_addr(&test_enr),
                self.expected_socket_addr,
                "Wrong contactable address for test '{}' with ip mode {:?}",
                self.name,
                self.ip_mode
            )
        }
    }

    #[test]
    fn empty_enr_no_contactable_address() {
        // Empty ENR
        TestCase::new("Empty enr is non contactable by ip4 node").ip_mode(Ip4).test();

        TestCase::new("Empty enr is not contactable by ip6 only node").ip_mode(Ip6).test();

        TestCase::new("Empty enr is not contactable by dual stack node").ip_mode(DualStack).test();
    }

    #[test]
    fn ipv4_only_enr_contactable_addresses() {
        // Ip4 only ENR
        TestCase::new("Ipv4 only enr is contactable by ip4 node")
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(Ip4)
            .expect_ip4(Ipv4Addr::LOCALHOST)
            .test();

        TestCase::new("Ipv4 only enr is not contactable by ip6 only node")
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(Ip6)
            .test();

        TestCase::new("Ipv4 only enr is contactable by dual stack node")
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(DualStack)
            .expect_ip4(Ipv4Addr::LOCALHOST)
            .test();
    }

    #[test]
    fn ipv6_only_enr_contactable_addresses() {
        // Ip4 only ENR
        TestCase::new("Ipv6 only enr is not contactable by ip4 node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .ip_mode(Ip4)
            .test();

        TestCase::new("Ipv6 only enr is contactable by ip6 only node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .ip_mode(Ip6)
            .expect_ip6(Ipv6Addr::LOCALHOST)
            .test();

        TestCase::new("Ipv6 only enr is contactable by dual stack node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .ip_mode(DualStack)
            .expect_ip6(Ipv6Addr::LOCALHOST)
            .test();
    }

    #[test]
    fn dual_stack_enr_contactable_addresses() {
        // Ip4 only ENR
        TestCase::new("Dual stack enr is contactable by ip4 node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(Ip4)
            .expect_ip4(Ipv4Addr::LOCALHOST)
            .test();

        TestCase::new("Dual stack enr is contactable by ip6 only node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(Ip6)
            .expect_ip6(Ipv6Addr::LOCALHOST)
            .test();

        TestCase::new("Dual stack enr is contactable by dual stack node")
            .enr_ip6(Ipv6Addr::LOCALHOST)
            .enr_ip4(Ipv4Addr::LOCALHOST)
            .ip_mode(Ip6)
            .expect_ip6(Ipv6Addr::LOCALHOST)
            .test();
    }
}
