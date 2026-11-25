//! Archive config row (`gen‖00`) and the segment arithmetic derived from it.

use std::fmt;

use alloy_primitives::B256;

use crate::{ArchiveError, FORMAT_VERSION, Family};

/// Largest allowed segment size, so offsets fit in a `u16`.
pub const MAX_BLOCKS_PER_SEGMENT: u64 = 1 << 16;

/// Encoded length of a config with retention.
pub const CONFIG_LEN_WITH_RETENTION: usize = 1 + 8 + 32 + 8 + 8 + 1 + 8;

/// Identity and layout of one archive generation, provisioned once by an operator.
///
/// ```text
/// format:u8 ‖ chain_id:u64be ‖ genesis_hash[32] ‖ blocks_per_segment:u64be ‖
/// start_segment:u64be ‖ has_retention:u8 ‖ [retain_segments:u64be]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveConfig {
    /// Format version; must equal [`FORMAT_VERSION`].
    pub format: u8,
    /// Chain id of the archived chain.
    pub chain_id: u64,
    /// Genesis block hash of the archived chain.
    pub genesis_hash: B256,
    /// Blocks per segment (`N`), 1..=65536.
    pub blocks_per_segment: u64,
    /// First archived segment.
    pub start_segment: u64,
    /// Number of most recent segments to keep, if retention is enabled.
    pub retain_segments: Option<u64>,
}

impl fmt::Display for ArchiveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "format={} chain_id={} genesis={} blocks_per_segment={} start_segment={} retain_segments={:?}",
            self.format,
            self.chain_id,
            self.genesis_hash,
            self.blocks_per_segment,
            self.start_segment,
            self.retain_segments
        )
    }
}

impl ArchiveConfig {
    /// Creates a config for the current format, validating it.
    pub fn new(
        chain_id: u64,
        genesis_hash: B256,
        blocks_per_segment: u64,
        start_segment: u64,
        retain_segments: Option<u64>,
    ) -> Result<Self, ArchiveError> {
        let config = Self {
            format: FORMAT_VERSION,
            chain_id,
            genesis_hash,
            blocks_per_segment,
            start_segment,
            retain_segments,
        };
        config.validate()?;
        Ok(config)
    }

    /// Checks value ranges.
    pub fn validate(&self) -> Result<(), ArchiveError> {
        if self.blocks_per_segment == 0 || self.blocks_per_segment > MAX_BLOCKS_PER_SEGMENT {
            return Err(ArchiveError::InvalidConfig {
                reason: format!(
                    "blocks_per_segment {} must be in 1..={MAX_BLOCKS_PER_SEGMENT}",
                    self.blocks_per_segment
                ),
            });
        }
        if self.retain_segments == Some(0) {
            return Err(ArchiveError::InvalidConfig {
                reason: "retain_segments must be at least 1".to_string(),
            });
        }
        if self.start_segment.checked_mul(self.blocks_per_segment).is_none() {
            return Err(ArchiveError::InvalidConfig {
                reason: "start_segment overflows the block number range".to_string(),
            });
        }
        Ok(())
    }

    /// Canonical encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CONFIG_LEN_WITH_RETENTION);
        out.push(self.format);
        out.extend_from_slice(&self.chain_id.to_be_bytes());
        out.extend_from_slice(self.genesis_hash.as_slice());
        out.extend_from_slice(&self.blocks_per_segment.to_be_bytes());
        out.extend_from_slice(&self.start_segment.to_be_bytes());
        match self.retain_segments {
            Some(retain) => {
                out.push(1);
                out.extend_from_slice(&retain.to_be_bytes());
            }
            None => out.push(0),
        }
        out
    }

    /// Decodes a config row, rejecting trailing bytes and invalid values.
    pub fn decode(raw: &[u8]) -> Result<Self, ArchiveError> {
        let err = |reason: &str| ArchiveError::decode(Family::Config, reason);
        let fixed = CONFIG_LEN_WITH_RETENTION - 8;
        if raw.len() != fixed && raw.len() != CONFIG_LEN_WITH_RETENTION {
            return Err(err("bad length"));
        }
        let u64_at = |at: usize| u64::from_be_bytes(raw[at..at + 8].try_into().expect("8 bytes"));
        let retain_segments = match (raw[fixed - 1], raw.len()) {
            (0, len) if len == fixed => None,
            (1, len) if len == CONFIG_LEN_WITH_RETENTION => Some(u64_at(fixed)),
            _ => return Err(err("bad retention tag")),
        };
        let config = Self {
            format: raw[0],
            chain_id: u64_at(1),
            genesis_hash: B256::from_slice(&raw[9..41]),
            blocks_per_segment: u64_at(41),
            start_segment: u64_at(49),
            retain_segments,
        };
        config.validate().map_err(|e| err(&e.to_string()))?;
        Ok(config)
    }

    /// Segment holding block `number`.
    pub const fn segment_of(&self, number: u64) -> u64 {
        number / self.blocks_per_segment
    }

    /// Offset of block `number` within its segment.
    pub const fn offset_of(&self, number: u64) -> u16 {
        (number % self.blocks_per_segment) as u16
    }

    /// First block of segment `segment` (saturating).
    pub const fn first_block(&self, segment: u64) -> u64 {
        segment.saturating_mul(self.blocks_per_segment)
    }

    /// Last block of segment `segment` (saturating).
    pub const fn last_block(&self, segment: u64) -> u64 {
        self.first_block(segment).saturating_add(self.blocks_per_segment - 1)
    }

    /// Earliest segment readers serve, given the highest published segment: `start_segment`, or
    /// with retention `R`, `max(start_segment, highest + 1 − R)`.
    pub fn earliest_segment(&self, highest: Option<u64>) -> u64 {
        match (self.retain_segments, highest) {
            (Some(retain), Some(highest)) => {
                self.start_segment.max(highest.saturating_add(1).saturating_sub(retain))
            }
            _ => self.start_segment,
        }
    }

    /// Segments strictly below the returned value may be physically pruned: the earliest
    /// served segment minus `grace` segments. `None` when retention is off or nothing is below.
    pub fn prune_cutoff(&self, highest: Option<u64>, grace: u64) -> Option<u64> {
        self.retain_segments?;
        let cutoff = self.earliest_segment(highest).saturating_sub(grace.max(1));
        (cutoff > 0).then_some(cutoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configs round-trip with and without retention and reject malformed rows.
    #[test]
    fn config_roundtrip_and_validation() {
        let with = ArchiveConfig::new(8453, B256::repeat_byte(7), 8192, 3, Some(10)).unwrap();
        let without = ArchiveConfig { retain_segments: None, ..with };
        assert_eq!(with.encode().len(), CONFIG_LEN_WITH_RETENTION, "length with retention");
        assert_eq!(without.encode().len(), CONFIG_LEN_WITH_RETENTION - 8, "length without");
        assert_eq!(ArchiveConfig::decode(&with.encode()).unwrap(), with, "round-trip with");
        assert_eq!(ArchiveConfig::decode(&without.encode()).unwrap(), without, "round-trip without");
        let mut trailing = without.encode();
        trailing.push(0);
        assert!(ArchiveConfig::decode(&trailing).is_err(), "trailing byte rejected");
        for (n, retain) in [(0, None), (65_537, None), (8, Some(0))] {
            assert!(
                ArchiveConfig::new(1, B256::ZERO, n, 0, retain).is_err(),
                "N={n} retain={retain:?} rejected"
            );
        }
        assert!(ArchiveConfig::new(1, B256::ZERO, 65_536, 0, None).is_ok(), "N=65536 allowed");
    }

    /// Retention arithmetic matches the spec and saturates.
    #[test]
    fn retention_arithmetic() {
        let config = ArchiveConfig::new(1, B256::ZERO, 8, 2, Some(3)).unwrap();
        assert_eq!(config.earliest_segment(None), 2, "no markers: start segment");
        assert_eq!(config.earliest_segment(Some(3)), 2, "clamped to start");
        assert_eq!(config.earliest_segment(Some(9)), 7, "highest + 1 − R");
        assert_eq!(config.prune_cutoff(Some(9), 2), Some(5), "earliest − grace");
        assert_eq!(config.prune_cutoff(Some(9), 0), Some(6), "grace is at least one");
        assert_eq!(config.prune_cutoff(Some(2), 2), None, "nothing below segment 0");
        assert_eq!(config.last_block(u64::MAX), u64::MAX, "saturating last block");
        let keep_all = ArchiveConfig { retain_segments: None, ..config };
        assert_eq!(keep_all.prune_cutoff(Some(100), 1), None, "no retention: no prune");
    }
}
