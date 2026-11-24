//! Absolute wire fingerprint shared by the versioned precompile ABI surfaces.
//!
//! Each frozen `sol!` surface pins one of these. It catches both-sides drift that relative
//! cross-surface asserts miss — an alloy `Display` or signature change that moves every copy
//! together still moves the fingerprint.

use alloc::vec::Vec;

use alloy_primitives::{B256, keccak256};

/// Keccak fingerprint of a frozen wire (ABI) surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct AbiFingerprint;

impl AbiFingerprint {
    /// Hashes sorted call selectors, then sorted event topic0s, then sorted error selectors, then
    /// `enum_count`, then `enum_ordinals`. The order is fixed so a single pin catches any
    /// wire-surface edit.
    ///
    /// `enum_ordinals` covers the case the other four terms miss. Solidity encodes enums as
    /// `uint8`, so reordering a surface's enum leaves every selector, topic0, error selector and
    /// count identical — yet the ordinals themselves are load-bearing wherever a discriminant
    /// escapes the ABI, such as `B20Variant` riding byte `[10]` of every B-20 token address.
    /// Surfaces whose discriminants carry no such meaning pass an empty iterator.
    pub fn compute(
        selectors: impl IntoIterator<Item = [u8; 4]>,
        event_hashes: impl IntoIterator<Item = B256>,
        error_selectors: impl IntoIterator<Item = [u8; 4]>,
        enum_count: usize,
        enum_ordinals: impl IntoIterator<Item = u8>,
    ) -> B256 {
        let mut selectors: Vec<[u8; 4]> = selectors.into_iter().collect();
        selectors.sort_unstable();

        let mut event_hashes: Vec<B256> = event_hashes.into_iter().collect();
        event_hashes.sort_unstable();

        let mut error_selectors: Vec<[u8; 4]> = error_selectors.into_iter().collect();
        error_selectors.sort_unstable();

        let mut buf = Vec::with_capacity(
            selectors.len() * 4 + event_hashes.len() * 32 + error_selectors.len() * 4 + 1,
        );
        for selector in &selectors {
            buf.extend_from_slice(selector);
        }
        for hash in &event_hashes {
            buf.extend_from_slice(hash.as_slice());
        }
        for selector in &error_selectors {
            buf.extend_from_slice(selector);
        }
        buf.push(enum_count as u8);
        buf.extend(enum_ordinals);
        keccak256(&buf)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::AbiFingerprint;

    const SELECTORS: [[u8; 4]; 2] = [[0xaa; 4], [0xbb; 4]];

    fn fingerprint_of(enum_count: usize, enum_ordinals: impl IntoIterator<Item = u8>) -> B256 {
        AbiFingerprint::compute(SELECTORS, [], [], enum_count, enum_ordinals)
    }

    /// Input order must not matter: the surface's selector iteration order is not guaranteed.
    #[test]
    fn selector_order_does_not_change_the_fingerprint() {
        let forward = AbiFingerprint::compute(SELECTORS, [], [], 0, []);
        let reversed = AbiFingerprint::compute([SELECTORS[1], SELECTORS[0]], [], [], 0, []);
        assert_eq!(forward, reversed);
    }

    /// An empty ordinal list must be a no-op, so surfaces that opt out keep their existing pins.
    #[test]
    fn empty_ordinals_are_a_no_op() {
        assert_eq!(fingerprint_of(2, []), fingerprint_of(2, core::iter::empty()));
    }

    /// The whole reason the ordinal term exists: a reorder moves the fingerprint even though
    /// selectors, topic0s, error selectors and the count are all unchanged.
    #[test]
    fn reordering_ordinals_changes_the_fingerprint() {
        assert_ne!(fingerprint_of(2, [0, 1]), fingerprint_of(2, [1, 0]));
    }

    #[test]
    fn count_is_covered() {
        assert_ne!(fingerprint_of(2, [0, 1]), fingerprint_of(3, [0, 1]));
    }
}
