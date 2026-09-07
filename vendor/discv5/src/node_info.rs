use std::net::SocketAddr;

use enr::{CombinedPublicKey, NodeId};
#[cfg(feature = "libp2p")]
use libp2p_identity::{KeyType, PublicKey};
#[cfg(feature = "libp2p")]
use multiaddr::{Multiaddr, Protocol};

use super::*;
use crate::Enr;

/// This type relaxes the requirement of having an ENR to connect to a node, to allow for unsigned
/// connection types, such as multiaddrs.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeContact {
    /// Key to use for communications with this node.
    public_key: CombinedPublicKey,
    /// Address to use to contact the node.
    socket_addr: SocketAddr,
    /// The ENR of the node if known.
    enr: Option<Enr>,
}

#[derive(Debug, Clone)]
pub struct NonContactable {
    pub enr: Enr,
}

impl NodeContact {
    pub fn new(public_key: CombinedPublicKey, socket_addr: SocketAddr, enr: Option<Enr>) -> Self {
        NodeContact { public_key, socket_addr, enr }
    }

    pub fn node_id(&self) -> NodeId {
        match self.enr {
            Some(ref enr) => enr.node_id(),
            None => self.public_key.clone().into(),
        }
    }

    pub fn seq_no(&self) -> Option<u64> {
        self.enr.as_ref().map(|enr| enr.seq())
    }

    pub fn public_key(&self) -> CombinedPublicKey {
        self.public_key.clone()
    }

    pub fn enr(&self) -> Option<Enr> {
        self.enr.clone()
    }

    pub fn socket_addr(&self) -> SocketAddr {
        self.socket_addr
    }

    pub fn node_address(&self) -> NodeAddress {
        NodeAddress { socket_addr: self.socket_addr, node_id: self.node_id() }
    }

    pub fn to_address_and_enr(self) -> (NodeAddress, Option<Enr>) {
        let NodeContact { public_key, socket_addr, enr } = self;
        (NodeAddress { node_id: public_key.into(), socket_addr }, enr)
    }

    pub fn try_from_enr(enr: Enr, ip_mode: IpMode) -> Result<Self, NonContactable> {
        let socket_addr = match ip_mode.get_contactable_addr(&enr) {
            Some(socket_addr) => socket_addr,
            None => return Err(NonContactable { enr }),
        };

        Ok(NodeContact { public_key: enr.public_key(), socket_addr, enr: Some(enr) })
    }

    #[cfg(feature = "libp2p")]
    pub fn try_from_multiaddr(multiaddr: Multiaddr) -> Result<Self, &'static str> {
        // The multiaddr must contain either the ip4 or ip6 protocols, the UDP protocol and the P2P
        // protocol with either secp256k1 or ed25519 keys.

        // perform a single pass and try to fill all required protocols from the multiaddr
        let mut ip_addr = None;
        let mut udp_port = None;
        let mut p2p = None;

        for protocol in multiaddr.into_iter() {
            match protocol {
                Protocol::Udp(port) => udp_port = Some(port),
                Protocol::Ip4(addr) => ip_addr = Some(addr.into()),
                Protocol::Ip6(addr) => ip_addr = Some(addr.into()),
                Protocol::P2p(peer_id) => p2p = Some(peer_id),
                _ => {}
            }
        }

        let udp_port = udp_port.ok_or("A UDP port must be specified in the multiaddr")?;
        let ip_addr = ip_addr.ok_or("An IP address must be specified in the multiaddr")?;
        let peer_id = p2p.ok_or("The p2p protocol must be specified in the multiaddr")?;

        let public_key: CombinedPublicKey = {
            let pk = PublicKey::try_decode_protobuf(&peer_id.to_bytes()[2..])
                .map_err(|_| "Invalid public key")?;
            match pk.key_type() {
                KeyType::Secp256k1 => enr::k256::ecdsa::VerifyingKey::from_sec1_bytes(
                    &pk.try_into_secp256k1().expect("Must be secp256k1").to_bytes_uncompressed(),
                )
                .expect("Libp2p key conversion, always valid")
                .into(),
                KeyType::Ed25519 => enr::ed25519_dalek::VerifyingKey::from_bytes(
                    &pk.try_into_ed25519().expect("Must be ed25519").to_bytes(),
                )
                .expect("Libp2p key conversion, always valid")
                .into(),
                _ => return Err("The key type is not supported"),
            }
        };

        Ok(NodeContact { public_key, socket_addr: SocketAddr::new(ip_addr, udp_port), enr: None })
    }
}

#[cfg(test)]
impl From<Enr> for NodeContact {
    #[track_caller]
    fn from(enr: Enr) -> Self {
        NodeContact::try_from_enr(enr, IpMode::default()).unwrap()
    }
}

impl std::fmt::Display for NodeContact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node: {}, addr: {:?}", self.node_id(), self.socket_addr)
    }
}

/// A representation of an unsigned contactable node.
#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub struct NodeAddress {
    /// The destination socket address.
    pub socket_addr: SocketAddr,
    /// The destination Node Id.
    pub node_id: NodeId,
}

impl Ord for NodeAddress {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ord = self.node_id.raw().cmp(&other.node_id.raw());
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        let ord = self.socket_addr.ip().cmp(&other.socket_addr.ip());
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        self.socket_addr.port().cmp(&other.socket_addr.port())
    }
}

impl PartialOrd for NodeAddress {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl NodeAddress {
    pub fn new(socket_addr: SocketAddr, node_id: NodeId) -> Self {
        Self { socket_addr, node_id }
    }
}

impl std::fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node: {}, addr: {:?}", self.node_id, self.socket_addr)
    }
}
