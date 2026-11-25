//! Offset sets: block offsets within a segment, the value of every posting row.
//!
//! ```text
//! tag 0x00 ‖ count:u8 ‖ offsets[count]:u16be   when count ≤ 16 (strictly increasing)
//! tag 0x01 ‖ portable roaring after optimize()   otherwise
//! ```
//!
//! Decoding accepts only the canonical encoding of a set, so equal sets always have equal bytes.

use roaring::RoaringBitmap;

use crate::{ArchiveError, Family};

/// Largest set stored in the compact encoding.
pub const COMPACT_MAX: usize = 16;

/// Compact encoding tag.
pub const TAG_COMPACT: u8 = 0x00;

/// Roaring encoding tag.
pub const TAG_ROARING: u8 = 0x01;

/// A set of `u16` block offsets within one segment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OffsetSet {
    /// Offsets, kept as a bitmap for set algebra.
    pub bits: RoaringBitmap,
}

impl OffsetSet {
    /// Builds a set from offsets.
    pub fn from_offsets(offsets: impl IntoIterator<Item = u16>) -> Self {
        Self { bits: offsets.into_iter().map(u32::from).collect() }
    }

    /// Adds an offset.
    pub fn insert(&mut self, offset: u16) {
        self.bits.insert(u32::from(offset));
    }

    /// Number of offsets.
    pub fn len(&self) -> u64 {
        self.bits.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    /// Offsets in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        // Every member was inserted from a u16 or validated on decode.
        self.bits.iter().map(|offset| offset as u16)
    }

    /// Canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let len = self.bits.len() as usize;
        if len <= COMPACT_MAX {
            let mut out = Vec::with_capacity(2 + 2 * len);
            out.push(TAG_COMPACT);
            out.push(len as u8);
            for offset in self.iter() {
                out.extend_from_slice(&offset.to_be_bytes());
            }
            return out;
        }
        let mut bits = self.bits.clone();
        bits.optimize();
        let mut out = Vec::with_capacity(1 + bits.serialized_size());
        out.push(TAG_ROARING);
        bits.serialize_into(&mut out).expect("writing to a Vec cannot fail");
        out
    }

    /// Decodes a value of `family`, rejecting non-canonical encodings and offsets outside the
    /// 16-bit range.
    pub fn decode(family: Family, raw: &[u8]) -> Result<Self, ArchiveError> {
        let err = |reason: &str| ArchiveError::decode(family, format!("offset set: {reason}"));
        let (&tag, rest) = raw.split_first().ok_or_else(|| err("empty value"))?;
        let set = match tag {
            TAG_COMPACT => {
                let (&count, body) = rest.split_first().ok_or_else(|| err("missing count"))?;
                let count = count as usize;
                if count > COMPACT_MAX || body.len() != 2 * count {
                    return Err(err("bad compact length"));
                }
                let offsets: Vec<u16> =
                    body.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if offsets.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(err("compact offsets not strictly increasing"));
                }
                Self::from_offsets(offsets)
            }
            TAG_ROARING => {
                let bits = RoaringBitmap::deserialize_from(rest).map_err(|e| err(&e.to_string()))?;
                if bits.max().is_some_and(|max| max > u32::from(u16::MAX)) {
                    return Err(err("offset above u16"));
                }
                Self { bits }
            }
            _ => return Err(err("unknown tag")),
        };
        if set.encode() != raw {
            return Err(err("non-canonical encoding"));
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sixteen offsets use the compact form, seventeen switch to roaring, and both round-trip.
    #[test]
    fn compact_and_roaring_boundary() {
        let sixteen = OffsetSet::from_offsets(0..16);
        let encoded = sixteen.encode();
        assert_eq!(encoded.len(), 2 + 32, "16 offsets: tag, count and 16 u16s");
        assert_eq!(encoded[0], TAG_COMPACT, "16 offsets use the compact tag");
        assert_eq!(encoded[1], 16, "count byte");
        assert_eq!(&encoded[2..6], &[0, 0, 0, 1], "offsets are big-endian u16s");
        assert_eq!(OffsetSet::decode(Family::LogAddress, &encoded).unwrap(), sixteen, "16 round-trip");

        let seventeen = OffsetSet::from_offsets(0..17);
        let encoded = seventeen.encode();
        assert_eq!(encoded[0], TAG_ROARING, "17 offsets use the roaring tag");
        assert_eq!(OffsetSet::decode(Family::LogAddress, &encoded).unwrap(), seventeen, "17 round-trip");

        let one = OffsetSet::from_offsets([65_535]);
        assert_eq!(one.encode(), vec![TAG_COMPACT, 1, 0xff, 0xff], "one offset is 4 bytes");
        let full = OffsetSet::from_offsets(0..=u16::MAX);
        let encoded = full.encode();
        assert!(encoded.len() < 64, "a full segment run-optimizes to a few bytes, got {}", encoded.len());
        assert_eq!(OffsetSet::decode(Family::StateHistory, &encoded).unwrap(), full, "full round-trip");
    }

    /// Non-canonical and malformed encodings are rejected.
    #[test]
    fn non_canonical_rejected() {
        for (raw, why) in [
            (vec![], "empty"),
            (vec![TAG_COMPACT, 2, 0, 5, 0, 5], "duplicate offsets"),
            (vec![TAG_COMPACT, 2, 0, 6, 0, 5], "decreasing offsets"),
            (vec![TAG_COMPACT, 2, 0, 5], "short body"),
            (vec![0x02], "unknown tag"),
        ] {
            assert!(OffsetSet::decode(Family::LogTopic, &raw).is_err(), "{why} must be rejected");
        }
        let mut roaring_small = vec![TAG_ROARING];
        OffsetSet::from_offsets([1, 2]).bits.serialize_into(&mut roaring_small).unwrap();
        assert!(
            OffsetSet::decode(Family::LogTopic, &roaring_small).is_err(),
            "a small set in roaring form is non-canonical"
        );
    }
}
