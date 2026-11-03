//! Contains the [`BootNode`] type which is used to represent a boot node in the network.

use std::str::FromStr;

use derive_more::{Display, From};
use discv5::{
    Enr,
    multiaddr::{Multiaddr, Protocol},
};
use serde::{Deserialize, Serialize};

use super::utils::{PeerIdConversionError, PeerUtils};
use crate::{EnodeUrl, NodeRecord};

/// Errors that can occur when parsing a bootnode.
#[derive(Debug, thiserror::Error)]
pub enum BootNodeParseError {
    /// Failed to parse ENR.
    #[error("Failed to parse ENR: {0}")]
    Enr(String),
    /// Failed to parse node record.
    #[error("Failed to parse node record: {0}")]
    NodeRecord(String),
    /// Failed to convert peer ID.
    #[error(transparent)]
    PeerIdConversion(#[from] PeerIdConversionError),
}

/// A boot node can be added either as a string in either 'enode' URL scheme or serialized from
/// [`Enr`] type.
#[derive(Clone, Debug, PartialEq, Eq, Hash, From, Display, Serialize, Deserialize)]
pub enum BootNode {
    /// An unsigned node record.
    #[display("{_0}")]
    Enode(Multiaddr),
    /// A signed node record.
    #[display("{_0:?}")]
    Enr(Enr),
}

impl BootNode {
    /// Parses a [`NodeRecord`] and serializes according to CL format. Note: [`discv5`] is
    /// originally a CL library hence needs this format to add the node.
    pub fn from_unsigned(node_record: NodeRecord) -> Result<Self, PeerIdConversionError> {
        let multi_address = Multiaddr::from(node_record.address)
            .with(Protocol::Udp(node_record.udp_port))
            .with(Protocol::Tcp(node_record.tcp_port))
            .with(Protocol::P2p(PeerUtils::local_id_to_p2p_id(node_record.id)?));
        Ok(Self::Enode(multi_address))
    }

    /// Converts a [`BootNode`] into a [`Multiaddr`].
    pub fn to_multiaddr(&self) -> Option<Multiaddr> {
        match self {
            Self::Enode(addr) => Some(addr.clone()),
            Self::Enr(enr) => PeerUtils::enr_to_multiaddr(enr),
        }
    }

    /// Helper method to parse a bootnode from a string.
    pub fn parse_bootnode(raw: &str) -> Result<Self, BootNodeParseError> {
        // If the string starts with "enr:" it is an ENR record.
        if raw.starts_with("enr:") {
            let enr = Enr::from_str(raw).map_err(BootNodeParseError::Enr)?;
            return Ok(Self::from(enr));
        }
        // Otherwise, attempt to use the Node Record format.
        let record =
            NodeRecord::from_str(raw).map_err(|e| BootNodeParseError::NodeRecord(e.to_string()))?;
        Ok(Self::from_unsigned(record)?)
    }
}

/// A configured bootnode before DNS resolution.
///
/// Parsing is pure: `enode://` URLs with a hostname become [`BootNodeEntry::Host`] and are
/// resolved later by [`BootNodes::resolve`](crate::BootNodes::resolve).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootNodeEntry {
    /// A bootnode that needs no resolution: an ENR or an `enode://` URL with an IP address.
    Node(BootNode),
    /// An `enode://` URL whose host is a DNS hostname.
    Host(EnodeUrl),
}

impl FromStr for BootNodeEntry {
    type Err = BootNodeParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.starts_with("enr:") {
            return BootNode::parse_bootnode(raw).map(Self::Node);
        }
        let url =
            EnodeUrl::from_str(raw).map_err(|e| BootNodeParseError::NodeRecord(e.to_string()))?;
        match url.node_record() {
            Some(record) => Ok(Self::Node(BootNode::from_unsigned(record)?)),
            None => {
                // Reject an unusable node ID now rather than after resolution.
                PeerUtils::local_id_to_p2p_id(url.id)?;
                Ok(Self::Host(url))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

    use discv5::{
        enr::{CombinedPublicKey, k256},
        handler::NodeContact,
    };
    use rstest::rstest;

    use super::*;
    use crate::PeerUtils;

    #[test]
    fn test_derive_bootnode_enode_multiaddr() {
        let hardcoded_enode = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305";
        let node_record = NodeRecord::from_str(hardcoded_enode).unwrap();
        let boot_node = BootNode::from_unsigned(node_record).unwrap();

        // Get the substring from hardcoded_enode between the second / and before the @
        let peer_id = hardcoded_enode
            [hardcoded_enode.find('/').unwrap() + 2..hardcoded_enode.find('@').unwrap()]
            .to_string();

        let peer_id = crate::PeerId::from_str(&peer_id).unwrap();
        let p2p_peer_id = PeerUtils::local_id_to_p2p_id(peer_id).unwrap();

        let expected_multiaddr = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(34, 65, 205, 244)))
            .with(Protocol::Udp(30305))
            .with(Protocol::Tcp(30305))
            .with(Protocol::P2p(p2p_peer_id));

        assert_eq!(boot_node.to_multiaddr(), Some(expected_multiaddr));
    }

    #[test]
    fn test_derive_bootnode_enode_multiaddr_back_and_forth() {
        let hardcoded_enode = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305";
        let node_record = NodeRecord::from_str(hardcoded_enode).unwrap();
        let boot_node = BootNode::from_unsigned(node_record).unwrap();
        let multiaddr = boot_node.to_multiaddr().unwrap();

        let node_contact = NodeContact::try_from_multiaddr(multiaddr).unwrap();

        // The public key contained in the NodeContact is used to connect to the
        // bootnode.
        let contact_pkey = node_contact.public_key();

        // We get the expected public key from the enode information.
        let peer_id = hardcoded_enode
            [hardcoded_enode.find('/').unwrap() + 2..hardcoded_enode.find('@').unwrap()]
            .to_string();
        let peer_id = crate::PeerId::from_str(&peer_id).unwrap();

        // The public key from the peer id is using the uncompressed form.
        let pkey_secp256k1 = PeerUtils::peer_id_to_secp256k1_pubkey(peer_id).unwrap();
        let p2p_public_key: discv5::libp2p_identity::secp256k1::PublicKey =
            discv5::libp2p_identity::secp256k1::PublicKey::try_from_bytes(
                &pkey_secp256k1.serialize(),
            )
            .unwrap();

        let expected_pkey: CombinedPublicKey =
            k256::ecdsa::VerifyingKey::from_sec1_bytes(&p2p_public_key.to_bytes()).unwrap().into();

        // These two keys should be equal.
        assert_eq!(contact_pkey, expected_pkey);
    }

    #[derive(Debug, Clone, Copy)]
    enum ParseErrorKind {
        Enr,
        NodeRecord,
        PeerIdConversion,
    }

    #[rstest]
    #[case("enr:invalid", ParseErrorKind::Enr)]
    #[case("enode://invalid@127.0.0.1:30303", ParseErrorKind::NodeRecord)]
    #[case(
        "enode://00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000@127.0.0.1:30303",
        ParseErrorKind::PeerIdConversion
    )]
    fn test_parse_bootnode_error_variants(#[case] raw: &str, #[case] expected: ParseErrorKind) {
        let err = BootNode::parse_bootnode(raw).expect_err("input should fail to parse");
        match (expected, err) {
            (ParseErrorKind::Enr, BootNodeParseError::Enr(_))
            | (ParseErrorKind::NodeRecord, BootNodeParseError::NodeRecord(_))
            | (ParseErrorKind::PeerIdConversion, BootNodeParseError::PeerIdConversion(_)) => {}
            (expected, actual) => {
                panic!("expected {expected:?}, got {actual:?}");
            }
        }
    }

    const HOSTNAME_ENODE: &str = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@bootnode.invalid:30305";

    #[test]
    fn test_entry_keeps_hostname_unresolved() {
        // `.invalid` never resolves (RFC 6761), so this only passes if parsing skips DNS.
        let entry = BootNodeEntry::from_str(HOSTNAME_ENODE).expect("hostname enode should parse");
        let BootNodeEntry::Host(url) = entry else { panic!("expected a hostname entry") };
        assert_eq!(url.host, url::Host::Domain("bootnode.invalid".to_string()));
    }

    #[test]
    fn test_entry_ip_enode_is_resolved_node() {
        let raw = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305";
        let expected = BootNode::parse_bootnode(raw).unwrap();
        assert_eq!(BootNodeEntry::from_str(raw).unwrap(), BootNodeEntry::Node(expected));
    }

    #[test]
    fn test_entry_hostname_with_invalid_id_fails() {
        let raw = "enode://00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000@bootnode.invalid:30303";
        assert!(matches!(
            BootNodeEntry::from_str(raw),
            Err(BootNodeParseError::PeerIdConversion(_))
        ));
    }
}
