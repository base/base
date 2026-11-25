//! Segment marker (`gen‖1F‖k`) and the digest it carries.

use alloy_primitives::B256;
use sha2::{Digest, Sha256};

use crate::{ArchiveError, Family};

/// Encoded marker length.
pub const MARKER_LEN: usize = 32 + 8 + 32;

/// Published once every row of a segment is acknowledged: `last_hash ‖ row_count:u64be ‖
/// digest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentMarker {
    /// Hash of the segment's last block.
    pub last_hash: B256,
    /// Number of rows in the segment (all families but config and marker).
    pub row_count: u64,
    /// [`SegmentDigest`] over those rows.
    pub digest: B256,
}

impl SegmentMarker {
    /// Canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MARKER_LEN);
        out.extend_from_slice(self.last_hash.as_slice());
        out.extend_from_slice(&self.row_count.to_be_bytes());
        out.extend_from_slice(self.digest.as_slice());
        out
    }

    /// Decodes a marker value.
    pub fn decode(raw: &[u8]) -> Result<Self, ArchiveError> {
        if raw.len() != MARKER_LEN {
            return Err(ArchiveError::decode(Family::Marker, "bad length"));
        }
        Ok(Self {
            last_hash: B256::from_slice(&raw[..32]),
            row_count: u64::from_be_bytes(raw[32..40].try_into().expect("8 bytes")),
            digest: B256::from_slice(&raw[40..]),
        })
    }
}

/// SHA-256 over a segment's rows in key order, each framed as
/// `len(key):u64be ‖ key ‖ len(value):u64be ‖ value`.
#[derive(Debug, Clone, Default)]
pub struct SegmentDigest {
    /// Running hash.
    pub hasher: Sha256,
    /// Rows added.
    pub rows: u64,
}

impl SegmentDigest {
    /// Adds one row. Rows must be added in increasing key order.
    pub fn update(&mut self, key: &[u8], value: &[u8]) {
        self.hasher.update((key.len() as u64).to_be_bytes());
        self.hasher.update(key);
        self.hasher.update((value.len() as u64).to_be_bytes());
        self.hasher.update(value);
        self.rows += 1;
    }

    /// Final digest.
    pub fn finish(self) -> B256 {
        B256::from_slice(&self.hasher.finalize())
    }

    /// Digest of `rows`, which must be sorted by key.
    pub fn of<'a>(rows: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> B256 {
        let mut digest = Self::default();
        for (key, value) in rows {
            digest.update(key, value);
        }
        digest.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markers round-trip, and the digest frames lengths so that moving a byte between key and
    /// value changes it.
    #[test]
    fn marker_roundtrip_and_digest_framing() {
        let marker = SegmentMarker {
            last_hash: B256::repeat_byte(1),
            row_count: 42,
            digest: B256::repeat_byte(2),
        };
        assert_eq!(marker.encode().len(), MARKER_LEN, "marker length");
        assert_eq!(SegmentMarker::decode(&marker.encode()).unwrap(), marker, "round-trip");
        assert!(SegmentMarker::decode(&[0; 71]).is_err(), "short marker rejected");
        let a = SegmentDigest::of([(&b"ab"[..], &b"c"[..])]);
        let b = SegmentDigest::of([(&b"a"[..], &b"bc"[..])]);
        assert_ne!(a, b, "length framing separates key and value");
    }
}
