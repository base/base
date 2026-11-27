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

/// SHA-256 over one family's rows in key order, each framed as
/// `len(key):u64be ‖ key ‖ len(value):u64be ‖ value`.
#[derive(Debug, Clone, Default)]
pub struct FamilyDigest {
    /// Running hash.
    pub hasher: Sha256,
    /// Rows added.
    pub rows: u64,
}

impl FamilyDigest {
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
}

/// Families a segment digest covers, in family-byte order.
pub const DIGEST_FAMILIES: [Family; 11] = [
    Family::BlockHash,
    Family::TxHash,
    Family::StateHistory,
    Family::LogAddress,
    Family::LogTopic,
    Family::Header,
    Family::Body,
    Family::Receipts,
    Family::Senders,
    Family::AccountChanges,
    Family::StorageChanges,
];

/// Segment digest: SHA-256 over the concatenation, in family-byte order
/// ([`DIGEST_FAMILIES`]), of every family's [`FamilyDigest`] (an empty family contributes the
/// digest of no rows). Families are hashed independently, so block rows can be hashed as they
/// are streamed, a batch at a time, while index rows are hashed at the end.
#[derive(Debug, Clone, Default)]
pub struct SegmentDigest {
    /// One running digest per entry of [`DIGEST_FAMILIES`].
    pub families: [FamilyDigest; 11],
}

impl SegmentDigest {
    /// Adds one row of `family`. Rows of one family must be added in increasing key order;
    /// families may interleave. Rows of families outside [`DIGEST_FAMILIES`] (config, marker)
    /// are not segment rows and are ignored.
    pub fn update(&mut self, family: Family, key: &[u8], value: &[u8]) {
        if let Some(slot) = DIGEST_FAMILIES.iter().position(|f| *f == family) {
            self.families[slot].update(key, value);
        }
    }

    /// Rows added across every family.
    pub fn rows(&self) -> u64 {
        self.families.iter().map(|f| f.rows).sum()
    }

    /// Final digest.
    pub fn finish(self) -> B256 {
        let mut outer = Sha256::new();
        for family in self.families {
            outer.update(family.finish());
        }
        B256::from_slice(&outer.finalize())
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
        let digest = |key: &[u8], value: &[u8]| {
            let mut d = FamilyDigest::default();
            d.update(key, value);
            d.finish()
        };
        assert_ne!(
            digest(b"ab", b"c"),
            digest(b"a", b"bc"),
            "length framing separates key and value"
        );
    }

    /// The segment digest hashes per-family digests in family-byte order, so interleaving
    /// families gives the same result and moving a row to another family changes it.
    #[test]
    fn segment_digest_is_per_family() {
        let mut a = SegmentDigest::default();
        a.update(Family::Header, b"h1", b"x");
        a.update(Family::BlockHash, b"b1", b"y");
        a.update(Family::Header, b"h2", b"z");
        let mut b = SegmentDigest::default();
        b.update(Family::BlockHash, b"b1", b"y");
        b.update(Family::Header, b"h1", b"x");
        b.update(Family::Header, b"h2", b"z");
        assert_eq!(a.rows(), 3, "three rows");
        let mut expected = Sha256::new();
        for family in DIGEST_FAMILIES {
            let mut d = FamilyDigest::default();
            match family {
                Family::BlockHash => d.update(b"b1", b"y"),
                Family::Header => {
                    d.update(b"h1", b"x");
                    d.update(b"h2", b"z");
                }
                _ => {}
            }
            expected.update(d.finish());
        }
        let expected = B256::from_slice(&expected.finalize());
        let a = a.finish();
        assert_eq!(a, expected, "outer hash over the eleven family digests in byte order");
        assert_eq!(b.finish(), a, "interleaving families does not change the digest");
        let mut moved = SegmentDigest::default();
        moved.update(Family::Body, b"h1", b"x");
        moved.update(Family::BlockHash, b"b1", b"y");
        moved.update(Family::Header, b"h2", b"z");
        assert_ne!(moved.finish(), a, "the family a row belongs to is part of the digest");
        let mut ignored = SegmentDigest::default();
        ignored.update(Family::Marker, b"m", b"v");
        assert_eq!(ignored.rows(), 0, "marker rows are not segment rows");
    }
}
