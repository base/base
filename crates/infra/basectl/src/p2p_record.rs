//! Lightweight `enode://` and EIP-778 `enr:` bootnode parser for `basectl`.
//!
//! Uses [`base_consensus_peer_types::NodeRecord`] for unsigned `enode://` records and the pure-Rust
//! `enr` crate (`k256` backend) for signed EIP-778 `enr:` records, avoiding `libp2p`, `discv5`,
//! and C `secp256k1-sys` in the CLI binary.

use std::str::FromStr;

use base_consensus_peer_types::NodeRecord;
use enr::{Enr, k256::ecdsa::SigningKey};
use k256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};

/// A signed secp256k1 EIP-778 Ethereum Node Record.
pub type EnrRecord = Enr<SigningKey>;

/// Errors returned while parsing a [`BootNode`].
#[derive(Debug, thiserror::Error)]
pub enum BootNodeParseError {
    /// Failed to parse ENR.
    #[error("Failed to parse ENR: {0}")]
    Enr(String),
    /// Failed to parse node record.
    #[error("Failed to parse node record: {0}")]
    NodeRecord(String),
    /// The enode peer ID is not a valid uncompressed secp256k1 public key.
    #[error("Invalid peer id: {0}")]
    InvalidPeerId(String),
}

/// A parsed bootnode in either `enode://` or signed `enr:` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootNode {
    /// An unsigned `enode://` record.
    Enode(NodeRecord),
    /// A signed EIP-778 `enr:` record.
    Enr(EnrRecord),
}

impl BootNode {
    /// Parses a bootnode string (`enr:...` or `enode://...`).
    pub fn parse_bootnode(raw: &str) -> Result<Self, BootNodeParseError> {
        if raw.starts_with("enr:") {
            let enr = EnrRecord::from_str(raw).map_err(BootNodeParseError::Enr)?;
            return Ok(Self::Enr(enr));
        }

        let record =
            NodeRecord::from_str(raw).map_err(|e| BootNodeParseError::NodeRecord(e.to_string()))?;
        let mut uncompressed = [0u8; 65];
        uncompressed[0] = 0x04;
        uncompressed[1..].copy_from_slice(record.id.as_slice());
        k256::PublicKey::from_sec1_bytes(&uncompressed)
            .map_err(|e| BootNodeParseError::InvalidPeerId(e.to_string()))?;
        Ok(Self::Enode(record))
    }

    /// Formats this bootnode as a libp2p multiaddr string (matching
    /// `base_consensus_peers::PeerUtils::enr_to_multiaddr`).
    pub fn to_multiaddr_string(&self) -> Option<String> {
        match self {
            Self::Enode(record) => {
                let mut uncompressed = [0u8; 65];
                uncompressed[0] = 0x04;
                uncompressed[1..].copy_from_slice(record.id.as_slice());
                let point = k256::EncodedPoint::from_bytes(uncompressed).ok()?;
                let pubkey: k256::PublicKey =
                    Option::from(k256::PublicKey::from_encoded_point(&point))?;
                let compressed = pubkey.to_encoded_point(true);
                let peer_id = secp256k1_compressed_to_libp2p_peer_id(compressed.as_bytes())?;
                let ip_proto = match record.address {
                    std::net::IpAddr::V4(_) => "ip4",
                    std::net::IpAddr::V6(_) => "ip6",
                };
                Some(format!(
                    "/{ip_proto}/{}/udp/{}/tcp/{}/p2p/{peer_id}",
                    record.address, record.udp_port, record.tcp_port
                ))
            }
            Self::Enr(enr) => {
                let (ip_proto, ip, port) = if let Some(socket) = enr.tcp4_socket() {
                    ("ip4", socket.ip().to_string(), socket.port())
                } else if let Some(socket) = enr.tcp6_socket() {
                    ("ip6", socket.ip().to_string(), socket.port())
                } else {
                    return None;
                };
                let compressed = enr.public_key().to_encoded_point(true);
                let peer_id = secp256k1_compressed_to_libp2p_peer_id(compressed.as_bytes())?;
                Some(format!("/{ip_proto}/{ip}/tcp/{port}/p2p/{peer_id}"))
            }
        }
    }
}

/// Encodes a 33-byte compressed secp256k1 public key as a base58btc libp2p `PeerId`.
///
/// Per the libp2p peer-id specification, a 33-byte secp256k1 key is serialized as the protobuf
/// message `PublicKey { Type = Secp256k1 (2), Data = <33 bytes> }` (37 bytes total) and wrapped
/// in an identity multihash (`code = 0x00`, `length = 0x25` = 37) since `37 <= 42`.
fn secp256k1_compressed_to_libp2p_peer_id(compressed: &[u8]) -> Option<String> {
    if compressed.len() != 33 {
        return None;
    }
    let mut multihash = [0u8; 39];
    multihash[0] = 0x00; // multihash: identity
    multihash[1] = 0x25; // length: 37 bytes
    multihash[2] = 0x08; // field 1 (KeyType), varint
    multihash[3] = 0x02; // KeyType::Secp256k1
    multihash[4] = 0x12; // field 2 (Data), length-delimited
    multihash[5] = 0x21; // length: 33 bytes
    multihash[6..].copy_from_slice(compressed);
    Some(base58_encode(&multihash))
}

fn base58_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let leading_zeros = input.iter().take_while(|&&b| b == 0).count();
    let mut digits = Vec::with_capacity(input.len() * 2);
    for &byte in input {
        let mut carry = u32::from(byte);
        for digit in &mut digits {
            carry += (*digit as u32) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::with_capacity(leading_zeros + digits.len());
    for _ in 0..leading_zeros {
        out.push('1');
    }
    for &digit in digits.iter().rev() {
        out.push(ALPHABET[digit as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn rejects_invalid_enr_signature() {
        let raw = "enr:-J24QNz9lbrKbN4iSmmjtnr7SjUMk4zB7f1krHZcTZx-JRKZd0kA2gjufUROD6T3sOWDVDnFLSumBFRHQnD2eLUNoVWGAYug2-lKgmlkgnY0gmlwhA_f17SJc2VjcDI1NmsxoQMc-V0s-6xR0n5k6l1x8l7F2v2P7m4o9x8t6q4w2e1r0oN0Y3CCIyiDdWRwgiMo";
        assert!(BootNode::parse_bootnode(raw).is_err());
    }

    #[test]
    fn parses_valid_signed_enr_and_derives_multiaddr() {
        let raw = base_common_chains::ChainConfig::mainnet()
            .bootnodes
            .consensus
            .iter()
            .copied()
            .find(|b| b.starts_with("enr:"))
            .expect("mainnet config should contain at least one ENR bootnode");
        let bootnode = BootNode::parse_bootnode(raw).expect("mainnet CL bootnode should parse");
        let BootNode::Enr(ref enr) = bootnode else {
            panic!("expected ENR bootnode");
        };
        assert!(enr.ip4().is_some() || enr.ip6().is_some());
        let multiaddr = bootnode.to_multiaddr_string().expect("expected multiaddr");
        assert!(multiaddr.contains("/tcp/"));
        assert!(multiaddr.contains("/p2p/16Uiu2HA"));
    }

    #[test]
    fn rejects_invalid_enr() {
        assert!(BootNode::parse_bootnode("enr:!!!").is_err());
        assert!(BootNode::parse_bootnode("enr:-cl-record").is_err());
    }

    #[test]
    fn parses_enode_and_derives_multiaddr() {
        let raw = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305";
        let record = NodeRecord::from_str(raw).unwrap();
        assert_eq!(record.address, IpAddr::V4(Ipv4Addr::new(34, 65, 205, 244)));
        assert_eq!(record.tcp_port, 30305);
        assert_eq!(record.udp_port, 30305);
        let bootnode = BootNode::parse_bootnode(raw).unwrap();
        assert!(matches!(bootnode, BootNode::Enode(_)));
        let multiaddr = bootnode.to_multiaddr_string().unwrap();
        assert!(multiaddr.starts_with("/ip4/34.65.205.244/udp/30305/tcp/30305/p2p/16Uiu2HA"));
    }
}
