//! Lightweight `enode://` and EIP-778 `enr:` bootnode parsing for `basectl`.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    num::ParseIntError,
    str::FromStr,
};

use alloy_primitives::{B512, keccak256};
use alloy_rlp::{Buf, Decodable, Header};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use k256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};
use url::{Host, Url};

/// Maximum RLP-encoded byte length of an EIP-778 Ethereum Node Record.
const MAX_ENR_RLP_BYTES: usize = 300;

/// Uncompressed secp256k1 SEC1 prefix byte.
const SEC1_UNCOMPRESSED_PREFIX: u8 = 0x04;

/// Bitcoin Base58 alphabet used by libp2p peer IDs.
const BASE58_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Parsed `enode://` node record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NodeRecord {
    /// Advertised IP address.
    pub address: IpAddr,
    /// UDP discovery port.
    pub udp_port: u16,
    /// TCP listener port.
    pub tcp_port: u16,
    /// Uncompressed 64-byte secp256k1 public key.
    pub id: B512,
}

impl NodeRecord {
    /// Returns the TCP socket address of this node.
    pub const fn tcp_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.tcp_port)
    }

    /// Returns the UDP socket address of this node.
    pub const fn udp_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.udp_port)
    }
}

/// Errors returned while parsing a [`NodeRecord`].
#[derive(Debug, thiserror::Error)]
pub enum NodeRecordParseError {
    /// Invalid `enode://` URL syntax or missing host/port.
    #[error("Failed to parse url: {0}")]
    InvalidUrl(String),
    /// Invalid 64-byte hex node ID.
    #[error("Failed to parse id: {0}")]
    InvalidId(String),
    /// Invalid `discport` query parameter.
    #[error("Failed to parse discport query: {0}")]
    Discport(ParseIntError),
}

impl FromStr for NodeRecord {
    type Err = NodeRecordParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(s).map_err(|e| NodeRecordParseError::InvalidUrl(e.to_string()))?;

        let port = url
            .port()
            .ok_or_else(|| NodeRecordParseError::InvalidUrl("no port specified".to_string()))?;

        let address = match url.host() {
            Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
            Some(Host::Domain(dns)) => format!("{dns}:{port}")
                .to_socket_addrs()
                .map_err(|e| NodeRecordParseError::InvalidUrl(e.to_string()))?
                .next()
                .map(|addr| addr.ip())
                .ok_or_else(|| {
                    NodeRecordParseError::InvalidUrl(format!("no IP found for host: {url:?}"))
                })?,
            None => return Err(NodeRecordParseError::InvalidUrl(format!("invalid host: {url:?}"))),
        };

        let udp_port = if let Some(discovery_port) =
            url.query_pairs().find_map(|(key, value)| (key.as_ref() == "discport").then_some(value))
        {
            discovery_port.parse::<u16>().map_err(NodeRecordParseError::Discport)?
        } else {
            port
        };

        let id = url
            .username()
            .parse::<B512>()
            .map_err(|e| NodeRecordParseError::InvalidId(e.to_string()))?;

        Ok(Self { address, udp_port, tcp_port: port, id })
    }
}

/// Parsed EIP-778 `enr:` record (v4 identity scheme).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnrRecord {
    secp256k1: [u8; 33],
    ip4: Option<Ipv4Addr>,
    ip6: Option<Ipv6Addr>,
    tcp4: Option<u16>,
    tcp6: Option<u16>,
    udp4: Option<u16>,
    udp6: Option<u16>,
}

impl EnrRecord {
    /// Returns the `IPv4` address if present.
    pub const fn ip4(&self) -> Option<Ipv4Addr> {
        self.ip4
    }

    /// Returns the `IPv6` address if present.
    pub const fn ip6(&self) -> Option<Ipv6Addr> {
        self.ip6
    }

    /// Returns the `IPv4` TCP port if present.
    pub const fn tcp4(&self) -> Option<u16> {
        self.tcp4
    }

    /// Returns the `IPv6` TCP port if present.
    pub const fn tcp6(&self) -> Option<u16> {
        self.tcp6
    }

    /// Returns the `IPv4` UDP port if present.
    pub const fn udp4(&self) -> Option<u16> {
        self.udp4
    }

    /// Returns the `IPv6` UDP port if present.
    pub const fn udp6(&self) -> Option<u16> {
        self.udp6
    }

    /// Formats the ENR as a libp2p `/ip4|ip6/.../tcp/.../p2p/<peer-id>` multiaddr.
    pub fn to_multiaddr_string(self) -> Option<String> {
        let peer_id = secp256k1_libp2p_peer_id(&self.secp256k1);
        if let (Some(ip), Some(port)) = (self.ip4, self.tcp4) {
            Some(format!("/ip4/{ip}/tcp/{port}/p2p/{peer_id}"))
        } else if let (Some(ip), Some(port)) = (self.ip6, self.tcp6) {
            Some(format!("/ip6/{ip}/tcp/{port}/p2p/{peer_id}"))
        } else {
            None
        }
    }
}

impl FromStr for EnrRecord {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let encoded =
            raw.strip_prefix("enr:").ok_or_else(|| "missing `enr:` prefix".to_string())?;
        let rlp = URL_SAFE_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|e| format!("invalid base64url ENR payload: {e}"))?;
        if rlp.len() > MAX_ENR_RLP_BYTES {
            return Err(format!("ENR exceeds {MAX_ENR_RLP_BYTES} bytes"));
        }

        let mut slice = rlp.as_slice();
        let header = Header::decode(&mut slice).map_err(|e| format!("invalid RLP header: {e}"))?;
        if !header.list {
            return Err("ENR payload is not an RLP list".to_string());
        }
        if slice.len() != header.payload_length {
            return Err("ENR payload length mismatch".to_string());
        }

        let sig_bytes = decode_rlp_bytes(&mut slice)?;
        let content_bytes = slice;

        // Decode `seq` (unused after validation) followed by sorted `(key, value)` pairs.
        let _seq = u64::decode(&mut slice).map_err(|e| format!("invalid ENR sequence: {e}"))?;

        let mut prev_key: Option<&[u8]> = None;
        let mut scheme: Option<&[u8]> = None;
        let mut secp256k1: Option<[u8; 33]> = None;
        let mut ip4 = None;
        let mut ip6 = None;
        let mut tcp4 = None;
        let mut tcp6 = None;
        let mut udp4 = None;
        let mut udp6 = None;

        while !slice.is_empty() {
            let key = decode_rlp_bytes(&mut slice)?;
            if let Some(prev) = prev_key
                && key <= prev
            {
                return Err("ENR keys are not strictly sorted".to_string());
            }
            prev_key = Some(key);

            let val_header =
                Header::decode(&mut slice).map_err(|e| format!("invalid ENR value RLP: {e}"))?;
            if slice.len() < val_header.payload_length {
                return Err("truncated ENR value".to_string());
            }
            let val_payload = &slice[..val_header.payload_length];
            slice.advance(val_header.payload_length);

            if val_header.list {
                continue;
            }

            match key {
                b"id" => scheme = Some(val_payload),
                b"secp256k1" => {
                    secp256k1 = Some(
                        val_payload
                            .try_into()
                            .map_err(|_| "invalid secp256k1 key length".to_string())?,
                    );
                }
                b"ip" => {
                    let bytes: [u8; 4] = val_payload
                        .try_into()
                        .map_err(|_| "invalid IPv4 address length".to_string())?;
                    ip4 = Some(Ipv4Addr::from(bytes));
                }
                b"ip6" => {
                    let bytes: [u8; 16] = val_payload
                        .try_into()
                        .map_err(|_| "invalid IPv6 address length".to_string())?;
                    ip6 = Some(Ipv6Addr::from(bytes));
                }
                b"tcp" => tcp4 = Some(decode_be_u16(val_payload)?),
                b"tcp6" => tcp6 = Some(decode_be_u16(val_payload)?),
                b"udp" => udp4 = Some(decode_be_u16(val_payload)?),
                b"udp6" => udp6 = Some(decode_be_u16(val_payload)?),
                _ => {}
            }
        }

        if scheme != Some(b"v4".as_slice()) {
            return Err("unsupported ENR identity scheme".to_string());
        }
        let secp256k1 = secp256k1.ok_or_else(|| "ENR missing secp256k1 public key".to_string())?;
        let verifying_key = VerifyingKey::from_sec1_bytes(&secp256k1)
            .map_err(|e| format!("invalid ENR secp256k1 public key: {e}"))?;
        let signature =
            Signature::from_slice(sig_bytes).map_err(|e| format!("invalid ENR signature: {e}"))?;

        let mut signing_rlp = Vec::with_capacity(content_bytes.len() + 3);
        Header { list: true, payload_length: content_bytes.len() }.encode(&mut signing_rlp);
        signing_rlp.extend_from_slice(content_bytes);
        let digest = keccak256(&signing_rlp);
        verifying_key
            .verify_prehash(digest.as_slice(), &signature)
            .map_err(|e| format!("ENR signature verification failed: {e}"))?;

        Ok(Self { secp256k1, ip4, ip6, tcp4, tcp6, udp4, udp6 })
    }
}

/// Errors returned while parsing a [`BootNode`].
#[derive(Debug, thiserror::Error)]
pub enum BootNodeParseError {
    /// Failed to parse an `enr:` record.
    #[error("Failed to parse ENR: {0}")]
    Enr(String),
    /// Failed to parse an `enode://` record.
    #[error("Failed to parse node record: {0}")]
    NodeRecord(String),
    /// Uncompressed public key in `enode://` is not a valid secp256k1 point.
    #[error("Invalid peer id: {0}")]
    InvalidPeerId(String),
}

/// Parsed bootnode: either an `enode://` record or a signed EIP-778 `enr:` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootNode {
    /// Unsigned `enode://` node record.
    Enode(NodeRecord),
    /// Signed EIP-778 `enr:` node record.
    Enr(EnrRecord),
}

impl BootNode {
    /// Parses either an `enr:` record or an `enode://` URL.
    pub fn parse_bootnode(raw: &str) -> Result<Self, BootNodeParseError> {
        if raw.starts_with("enr:") {
            let enr = EnrRecord::from_str(raw).map_err(BootNodeParseError::Enr)?;
            return Ok(Self::Enr(enr));
        }
        let record =
            NodeRecord::from_str(raw).map_err(|e| BootNodeParseError::NodeRecord(e.to_string()))?;
        let mut full_pubkey = [0u8; 65];
        full_pubkey[0] = SEC1_UNCOMPRESSED_PREFIX;
        full_pubkey[1..].copy_from_slice(record.id.as_slice());
        VerifyingKey::from_sec1_bytes(&full_pubkey)
            .map_err(|e| BootNodeParseError::InvalidPeerId(e.to_string()))?;
        Ok(Self::Enode(record))
    }

    /// Converts an ENR bootnode into a libp2p multiaddr string.
    pub fn to_multiaddr_string(self) -> Option<String> {
        match self {
            Self::Enode(_) => None,
            Self::Enr(enr) => enr.to_multiaddr_string(),
        }
    }
}

fn decode_rlp_bytes<'a>(slice: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let header = Header::decode(slice).map_err(|e| format!("invalid RLP string: {e}"))?;
    if header.list {
        return Err("expected RLP byte string, found list".to_string());
    }
    if slice.len() < header.payload_length {
        return Err("truncated RLP byte string".to_string());
    }
    let bytes = &slice[..header.payload_length];
    slice.advance(header.payload_length);
    Ok(bytes)
}

fn decode_be_u16(bytes: &[u8]) -> Result<u16, String> {
    match bytes {
        [] => Ok(0),
        [b] => Ok(u16::from(*b)),
        [hi, lo] => Ok(u16::from_be_bytes([*hi, *lo])),
        _ => Err("invalid u16 port length in ENR".to_string()),
    }
}

/// Derives the base58btc libp2p `PeerId` string from a 33-byte compressed secp256k1 public key.
///
/// Encoding per libp2p peer-ids spec:
/// - Protobuf `PublicKey { Type: Secp256k1 (2), Data: <33 bytes> }` = `[0x08, 0x02, 0x12, 0x21, ..]` (37 bytes)
/// - Identity multihash (`<= 42` bytes): `[0x00, 0x25, ..]` (39 bytes)
/// - Base58btc encoding.
fn secp256k1_libp2p_peer_id(compressed_pubkey: &[u8; 33]) -> String {
    let mut multihash = [0u8; 39];
    multihash[0] = 0x00; // identity multihash code
    multihash[1] = 0x25; // length = 37
    multihash[2] = 0x08; // field 1 (Type), wire type varint
    multihash[3] = 0x02; // KeyType::Secp256k1
    multihash[4] = 0x12; // field 2 (Data), wire type length-delimited
    multihash[5] = 0x21; // length = 33
    multihash[6..].copy_from_slice(compressed_pubkey);
    encode_base58btc(&multihash)
}

fn encode_base58btc(input: &[u8]) -> String {
    let leading_zeros = input.iter().take_while(|&&b| b == 0).count();
    let mut digits = Vec::new();
    for &byte in input {
        let mut carry = u32::from(byte);
        for d in &mut digits {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
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
    for &d in digits.iter().rev() {
        out.push(BASE58_ALPHABET[usize::from(d)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    const VALID_ENR: &str = "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg";

    #[test]
    fn parses_valid_signed_enr_and_derives_multiaddr() {
        let BootNode::Enr(enr) = BootNode::parse_bootnode(VALID_ENR).unwrap() else {
            panic!("expected ENR");
        };
        assert_eq!(enr.ip4(), Some(Ipv4Addr::new(54, 145, 88, 85)));
        assert_eq!(enr.tcp4(), Some(9222));
        assert_eq!(enr.udp4(), Some(9222));
        let multiaddr = enr.to_multiaddr_string().unwrap();
        assert!(multiaddr.starts_with("/ip4/54.145.88.85/tcp/9222/p2p/16Uiu2HAm"), "{multiaddr}");
    }

    #[test]
    fn rejects_invalid_enr() {
        assert!(BootNode::parse_bootnode("enr:!!!").is_err());
        assert!(BootNode::parse_bootnode("enr:-cl-record").is_err());
    }

    #[test]
    fn parses_enode_with_discport() {
        let raw = "enode://2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4@34.65.205.244:30305?discport=30301";
        let record = NodeRecord::from_str(raw).unwrap();
        assert_eq!(record.address, IpAddr::V4(Ipv4Addr::new(34, 65, 205, 244)));
        assert_eq!(record.tcp_port, 30305);
        assert_eq!(record.udp_port, 30301);
        assert!(matches!(BootNode::parse_bootnode(raw).unwrap(), BootNode::Enode(_)));
    }
}
