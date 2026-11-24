//! Ethereum Node Records (EIP-778) with the "v4" identity scheme, and their text form.
//!
//! A record is `[signature, seq, k1, v1, k2, v2, ...]` with keys sorted; the signature is a
//! secp256k1 signature of `keccak256(rlp([seq, k1, v1, ...]))`. Values are kept as their raw RLP
//! so unknown keys round-trip unchanged.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use secp256k1::{PublicKey, SecretKey};

use crate::{
    crypto::{PeerId, peer_id, sign_compact, verify_compact},
    forkid::ForkId,
};

/// Largest encoded record accepted (EIP-778).
pub(crate) const MAX_RECORD_SIZE: usize = 300;
/// The ENR key under which base-reth advertises its fork id (reth's `NetworkStackId::OPEL`).
pub const FORK_KEY: &[u8] = b"opel";

/// A signed node record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enr {
    /// The sequence number.
    pub seq: u64,
    /// Key/value pairs sorted by key; values are raw RLP items.
    pub pairs: Vec<(Vec<u8>, Vec<u8>)>,
    signature: [u8; 64],
    public_key: PublicKey,
}

/// Why a record is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrError {
    /// Malformed RLP, unsorted or duplicate keys, or too large.
    Malformed,
    /// Not the "v4" scheme, or no valid public key.
    UnsupportedScheme,
    /// The signature does not verify.
    BadSignature,
}

fn raw<T: Encodable>(value: &T) -> Vec<u8> {
    alloy_rlp::encode(value)
}

fn content_rlp(seq: u64, pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut payload = Vec::new();
    seq.encode(&mut payload);
    for (key, value) in pairs {
        key.as_slice().encode(&mut payload);
        payload.extend_from_slice(value);
    }
    let mut out = Vec::new();
    RlpHeader { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    out
}

impl Enr {
    /// Builds and signs a record for `key` with an IPv4 endpoint and optional fork id.
    pub fn build(
        key: &SecretKey,
        seq: u64,
        ip: Option<Ipv4Addr>,
        tcp: u16,
        udp: u16,
        fork_id: Option<ForkId>,
    ) -> Self {
        let public_key = PublicKey::from_secret_key_global(key);
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"id".to_vec(), raw(&&b"v4"[..])),
            (b"secp256k1".to_vec(), raw(&&public_key.serialize()[..])),
        ];
        if let Some(ip) = ip {
            pairs.push((b"ip".to_vec(), raw(&&ip.octets()[..])));
        }
        if tcp != 0 {
            pairs.push((b"tcp".to_vec(), raw(&tcp)));
        }
        if udp != 0 {
            pairs.push((b"udp".to_vec(), raw(&udp)));
        }
        if let Some(fork_id) = fork_id {
            // reth's `EnrForkIdEntry`: the RLP list `[fork_id]`.
            pairs.push((FORK_KEY.to_vec(), raw(&vec![fork_id])));
        }
        pairs.sort();
        let digest = keccak256(content_rlp(seq, &pairs));
        let signature = sign_compact(digest, key);
        Self { seq, pairs, signature, public_key }
    }

    /// The raw RLP value of `key`.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice())
    }

    fn get_decoded<T: Decodable>(&self, key: &[u8]) -> Option<T> {
        let mut value = self.get(key)?;
        T::decode(&mut value).ok()
    }

    /// The record's public key.
    pub const fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// The RLPx peer id (uncompressed public key).
    pub fn peer_id(&self) -> PeerId {
        peer_id(&self.public_key)
    }

    /// The discv5 node id: keccak256 of the uncompressed public key.
    pub fn node_id(&self) -> B256 {
        keccak256(self.peer_id())
    }

    /// The IPv4 address.
    pub fn ip4(&self) -> Option<Ipv4Addr> {
        let bytes: [u8; 4] = self.get_decoded::<alloy_primitives::Bytes>(b"ip")?.as_ref().try_into().ok()?;
        Some(Ipv4Addr::from(bytes))
    }

    /// The UDP endpoint for discovery.
    pub fn udp_socket(&self) -> Option<SocketAddr> {
        let port: u16 = self.get_decoded(b"udp")?;
        Some(SocketAddr::V4(SocketAddrV4::new(self.ip4()?, port)))
    }

    /// The TCP endpoint for RLPx.
    pub fn tcp_socket(&self) -> Option<SocketAddr> {
        let port: u16 = self.get_decoded(b"tcp")?;
        Some(SocketAddr::V4(SocketAddrV4::new(self.ip4()?, port)))
    }

    /// The fork id under base-reth's `opel` key.
    pub fn fork_id(&self) -> Option<ForkId> {
        let entries: Vec<ForkId> = self.get_decoded(FORK_KEY)?;
        entries.first().copied()
    }

    /// The canonical RLP encoding.
    pub fn encode(&self) -> Vec<u8> {
        let content = content_rlp(self.seq, &self.pairs);
        let mut buf = &content[..];
        let header = RlpHeader::decode(&mut buf).expect("own encoding");
        let mut payload = Vec::with_capacity(content.len() + 66);
        self.signature.as_slice().encode(&mut payload);
        payload.extend_from_slice(&buf[..header.payload_length]);
        let mut out = Vec::new();
        RlpHeader { list: true, payload_length: payload.len() }.encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    /// Decodes and verifies a record.
    pub fn decode(buf: &mut &[u8]) -> Result<Self, EnrError> {
        let start = *buf;
        let header = RlpHeader::decode(buf).map_err(|_| EnrError::Malformed)?;
        if !header.list || buf.len() < header.payload_length {
            return Err(EnrError::Malformed);
        }
        if start.len() - buf.len() + header.payload_length > MAX_RECORD_SIZE {
            return Err(EnrError::Malformed);
        }
        let mut items = &buf[..header.payload_length];
        *buf = &buf[header.payload_length..];
        let signature: [u8; 64] = RlpHeader::decode_bytes(&mut items, false)
            .map_err(|_| EnrError::Malformed)?
            .try_into()
            .map_err(|_| EnrError::UnsupportedScheme)?;
        let seq = u64::decode(&mut items).map_err(|_| EnrError::Malformed)?;
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        while !items.is_empty() {
            let key = RlpHeader::decode_bytes(&mut items, false).map_err(|_| EnrError::Malformed)?.to_vec();
            let value_start = items;
            let value_header = RlpHeader::decode(&mut items).map_err(|_| EnrError::Malformed)?;
            let consumed = value_start.len() - items.len() + value_header.payload_length;
            let value = value_start.get(..consumed).ok_or(EnrError::Malformed)?.to_vec();
            items = &value_start[consumed..];
            if pairs.last().is_some_and(|(last, _)| *last >= key) {
                return Err(EnrError::Malformed);
            }
            pairs.push((key, value));
        }
        let find = |k: &[u8]| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let id = find(b"id").ok_or(EnrError::UnsupportedScheme)?;
        if id != raw(&&b"v4"[..]) {
            return Err(EnrError::UnsupportedScheme);
        }
        let key_value = find(b"secp256k1").ok_or(EnrError::UnsupportedScheme)?;
        let key_bytes = RlpHeader::decode_bytes(&mut key_value.as_slice(), false)
            .map_err(|_| EnrError::UnsupportedScheme)?;
        let public_key = PublicKey::from_slice(key_bytes).map_err(|_| EnrError::UnsupportedScheme)?;
        let digest = keccak256(content_rlp(seq, &pairs));
        if !verify_compact(digest, &signature, &public_key) {
            return Err(EnrError::BadSignature);
        }
        Ok(Self { seq, pairs, signature, public_key })
    }

    /// The text form, `enr:` plus unpadded URL-safe base64.
    pub fn to_text(&self) -> String {
        format!("enr:{}", base64url_encode(&self.encode()))
    }

    /// Parses the text form.
    pub fn from_text(text: &str) -> Result<Self, EnrError> {
        let body = text.strip_prefix("enr:").ok_or(EnrError::Malformed)?;
        let bytes = base64url_decode(body).ok_or(EnrError::Malformed)?;
        Self::decode(&mut bytes.as_slice())
    }
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 |
            (*chunk.get(1).unwrap_or(&0) as u32) << 8 |
            *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..=chunk.len() {
            out.push(BASE64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = BASE64URL.iter().position(|b| *b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    const VECTOR: &str = "enr:-IS4QHCYrYZbAKWCBRlAy5zzaDZXJBGkcnh4MHcBFZntXNFrdvJjX04jRzjzCBOonrkTfj499SZuOh8R33Ls8RRcy5wBgmlkgnY0gmlwhH8AAAGJc2VjcDI1NmsxoQPKY0yuDUmstAHYpMa2_oxVtw0RW_QAdpzBQA8yWM0xOIN1ZHCCdl8";

    #[test]
    fn decodes_and_reproduces_the_eip778_vector() {
        let enr = Enr::from_text(VECTOR).unwrap();
        assert_eq!(enr.seq, 1);
        assert_eq!(
            hex::encode(enr.node_id()),
            "a448f24c6d18e575453db13171562b71999873db5b286df957af199ec94617f7"
        );
        assert_eq!(enr.udp_socket().unwrap(), "127.0.0.1:30303".parse().unwrap());
        assert_eq!(enr.to_text(), VECTOR);

        // Signing the same content with the vector's key reproduces the same record content
        // (secp256k1 signing is deterministic, RFC 6979).
        let key = SecretKey::from_byte_array(
            hex::decode("b71c71a67e1177ad4e901695e1b4b9ee17ae16c6668d313eac2f96dbcda3f291").unwrap().try_into().unwrap(),
        )
        .unwrap();
        let built = Enr::build(&key, 1, Some(Ipv4Addr::LOCALHOST), 0, 30303, None);
        assert_eq!(built.to_text(), VECTOR);
    }

    #[test]
    fn rejects_tampered_records_and_reads_fork_ids() {
        let key = crate::crypto::random_secret_key();
        let fork_id = ForkId { hash: [1, 2, 3, 4], next: 5 };
        let enr = Enr::build(&key, 7, Some(Ipv4Addr::new(10, 0, 0, 1)), 30303, 30304, Some(fork_id));
        let mut bytes = enr.encode();
        let decoded = Enr::decode(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.fork_id(), Some(fork_id));
        assert_eq!(decoded.tcp_socket().unwrap(), "10.0.0.1:30303".parse().unwrap());
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Enr::decode(&mut bytes.as_slice()).is_err());
    }
}
