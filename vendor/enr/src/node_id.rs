//! The identifier for an ENR record. This is the keccak256 hash of the public key (for secp256k1
//! keys this is the uncompressed encoded form of the public key).

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{Enr, EnrKey, digest, keys::EnrPublicKey};

type RawNodeId = [u8; 32];

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
/// The `NodeId` of an ENR (a 32 byte identifier).
pub struct NodeId {
    #[cfg_attr(feature = "serde", serde(with = "serde_hex_prfx"))]
    raw: RawNodeId,
}

impl NodeId {
    /// Creates a new node record from 32 bytes.
    #[must_use]
    pub const fn new(raw_input: &[u8; 32]) -> Self {
        Self { raw: *raw_input }
    }

    /// Parses a byte slice to form a node Id. This fails if the slice isn't of length 32.
    pub fn parse(raw_input: &[u8]) -> Result<Self, &'static str> {
        if raw_input.len() > 32 {
            return Err("Input too large");
        }

        let mut raw: RawNodeId = [0_u8; 32];
        raw[..std::cmp::min(32, raw_input.len())].copy_from_slice(raw_input);

        Ok(Self { raw })
    }

    /// Generates a random `NodeId`.
    #[must_use]
    pub fn random() -> Self {
        Self { raw: rand_08::random() }
    }

    /// Returns a `RawNodeId` which is a 32 byte list.
    #[must_use]
    pub const fn raw(&self) -> RawNodeId {
        self.raw
    }
}

impl<T: EnrPublicKey> From<T> for NodeId {
    fn from(public_key: T) -> Self {
        Self::parse(&digest(public_key.encode_uncompressed().as_ref()))
            .expect("always of correct length; qed")
    }
}

impl<T: EnrKey> From<Enr<T>> for NodeId {
    fn from(enr: Enr<T>) -> Self {
        enr.node_id()
    }
}

impl<T: EnrKey> From<&Enr<T>> for NodeId {
    fn from(enr: &Enr<T>) -> Self {
        enr.node_id()
    }
}

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.raw[..]
    }
}

impl PartialEq<RawNodeId> for NodeId {
    fn eq(&self, other: &RawNodeId) -> bool {
        self.raw.eq(other)
    }
}

impl From<RawNodeId> for NodeId {
    fn from(raw: RawNodeId) -> Self {
        Self { raw }
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex_encode = hex::encode(self.raw);
        write!(f, "0x{}..{}", &hex_encode[0..4], &hex_encode[hex_encode.len() - 4..])
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{}", hex::encode(self.raw))
    }
}

/// Serialize with the 0x prefix.
#[cfg(feature = "serde")]
mod serde_hex_prfx {

    pub fn serialize<T: AsRef<[u8]> + hex::ToHex, S: serde::Serializer>(
        data: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let dst = format!("0x{}", hex::encode(data));
        serializer.serialize_str(&dst)
    }

    /// Deserialize with the 0x prefix.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: hex::FromHex,
        <T as hex::FromHex>::Error: std::fmt::Display,
    {
        /// Helper struct to obtain a owned string when necessary (using [`serde_json`], for
        /// example) or a borrowed string with the appropriate lifetime (most the time).
        // NOTE: see https://github.com/serde-rs/serde/issues/1413#issuecomment-494892266 and
        // https://github.com/sigp/enr/issues/62
        #[derive(serde::Deserialize)]
        struct CowNodeId<'a>(#[serde(borrow)] std::borrow::Cow<'a, str>);

        let CowNodeId::<'de>(raw) = serde::Deserialize::deserialize(deserializer)?;

        let src = raw.strip_prefix("0x").unwrap_or(&raw);
        hex::FromHex::from_hex(src).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eq_node_raw_node() {
        let node = NodeId::random();
        let raw = node.raw;
        assert_eq!(node, raw);
        assert_eq!(node.as_ref(), &raw[..]);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_str() {
        let node = NodeId::random();
        let json_string = serde_json::to_string(&node).unwrap();
        assert_eq!(node, serde_json::from_str::<NodeId>(&json_string).unwrap());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_slice() {
        let node = NodeId::random();
        let json_bytes = serde_json::to_vec(&node).unwrap();
        assert_eq!(node, serde_json::from_slice::<NodeId>(&json_bytes).unwrap());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_value() {
        let node = NodeId::random();
        let value = serde_json::to_value(node).unwrap();
        assert_eq!(node, serde_json::from_value::<NodeId>(value).unwrap());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_0x() {
        let raw = [
            154, 95, 80, 100, 224, 32, 222, 137, 157, 219, 197, 24, 45, 143, 90, 106, 99, 12, 9,
            93, 44, 66, 196, 203, 35, 233, 26, 59, 50, 128, 168, 180,
        ];
        let node = NodeId::parse(&raw).unwrap();
        let json_string = serde_json::to_string(&node).unwrap();
        assert_eq!(
            json_string,
            "\"0x9a5f5064e020de899ddbc5182d8f5a6a630c095d2c42c4cb23e91a3b3280a8b4\""
        );
        let snode = serde_json::from_str::<NodeId>(&json_string).unwrap();
        assert_eq!(node, snode);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_as_hashmap_key() {
        use std::collections::HashMap;

        let mut responses: HashMap<NodeId, u8> = HashMap::default();
        responses.insert(NodeId::random(), 1);
        let _ = serde_json::json!(responses);
    }
}
