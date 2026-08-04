//! Prepared L1 origin state shared across the origin selector and the attributes builder.

use std::sync::Arc;

use alloy_consensus::{Header, Receipt};
use alloy_primitives::B256;
use base_protocol::BlockInfo;

/// Fully-prepared L1 origin state: the header and receipts the build loop needs to open an epoch
/// without inline L1 I/O.
///
/// The origin selector stores this for both the current and next origins so it can publish the
/// selected origin (header + receipts) to the sequencer's attributes builder without a second L1
/// round-trip. See [`PrefetchedChainProvider`](super::PrefetchedChainProvider).
#[derive(Debug, Clone)]
pub struct PreparedL1Origin {
    /// The origin block hash, cached to avoid recomputing [`Header::hash_slow`] per lookup.
    pub hash: B256,
    /// The full L1 header, needed for `mix_hash`, `parent_beacon_block_root`, base-fee fields, and
    /// the origin timestamp.
    pub header: Header,
    /// The origin's receipts, used to derive deposit transactions and system-config updates.
    /// [`Arc`]-wrapped so publishing the origin clones cheaply regardless of receipt volume.
    pub receipts: Arc<Vec<Receipt>>,
}

impl PreparedL1Origin {
    /// Returns the lightweight [`BlockInfo`] view of this origin.
    pub const fn block_info(&self) -> BlockInfo {
        BlockInfo {
            hash: self.hash,
            number: self.header.number,
            parent_hash: self.header.parent_hash,
            timestamp: self.header.timestamp,
        }
    }
}

/// A next origin proven to extend a specific parent by `parent_hash`.
///
/// The only constructor, [`LinkedOrigin::link`], performs the linkage check, so holding a
/// `LinkedOrigin` is itself the proof that it chains onto the origin it was verified against. This
/// makes "the buffered `next` links to `current`" a type-level invariant rather than a check that
/// callers must remember to perform.
#[derive(Debug, Clone)]
pub struct LinkedOrigin(PreparedL1Origin);

impl LinkedOrigin {
    /// Returns `Some` iff `candidate` extends `parent` (its `parent_hash` equals `parent.hash`).
    pub fn link(parent: &PreparedL1Origin, candidate: PreparedL1Origin) -> Option<Self> {
        (candidate.header.parent_hash == parent.hash).then_some(Self(candidate))
    }

    /// Returns the underlying prepared origin.
    pub const fn get(&self) -> &PreparedL1Origin {
        &self.0
    }

    /// Consumes the proof, yielding the prepared origin to be promoted into `current`.
    pub fn into_current(self) -> PreparedL1Origin {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    fn prepared(hash: u8, parent: u8) -> PreparedL1Origin {
        PreparedL1Origin {
            hash: B256::with_last_byte(hash),
            header: Header { parent_hash: B256::with_last_byte(parent), ..Default::default() },
            receipts: Arc::new(vec![]),
        }
    }

    #[test]
    fn test_block_info_reads_from_header() {
        let origin = PreparedL1Origin {
            hash: B256::with_last_byte(9),
            header: Header {
                number: 7,
                timestamp: 84,
                parent_hash: B256::with_last_byte(8),
                ..Default::default()
            },
            receipts: Arc::new(vec![]),
        };
        let info = origin.block_info();
        assert_eq!(info.hash, B256::with_last_byte(9));
        assert_eq!(info.number, 7);
        assert_eq!(info.parent_hash, B256::with_last_byte(8));
        assert_eq!(info.timestamp, 84);
    }

    #[test]
    fn test_link_accepts_matching_parent() {
        let current = prepared(1, 0);
        let candidate = prepared(2, 1);
        let linked = LinkedOrigin::link(&current, candidate).expect("candidate extends current");
        assert_eq!(linked.get().hash, B256::with_last_byte(2));
    }

    #[test]
    fn test_link_rejects_mismatched_parent() {
        let current = prepared(1, 0);
        // Candidate's parent_hash (0xEE) does not link back to current's hash (0x01).
        let candidate = prepared(2, 0xEE);
        assert!(LinkedOrigin::link(&current, candidate).is_none());
    }
}
