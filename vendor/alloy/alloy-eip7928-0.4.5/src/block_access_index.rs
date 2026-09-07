//! Contains the [`BlockAccessIndex`] newtype and its [`BlockAccessPhase`] classification.

use core::fmt;

/// Block access index within a block.
///
/// A block's indices are laid out as:
/// - `0` — pre-execution (system contract calls, block-level setup, ...)
/// - `1..=tx_len` — transaction execution (transaction index `i` maps to index `i + 1`)
/// - `tx_len + 1` — post-execution (block rewards, withdrawals, ...)
///
/// Stored as a `u64` internally, but wrapped as a newtype so it cannot be accidentally
/// confused with other `u64` indices (for example, an `account_id` passed alongside it).
///
/// RLP, borsh, and arbitrary representations are identical to the wrapped `u64`. Serde
/// serializes as a hex `"quantity"` string (e.g. `"0x1a"`), matching the previous
/// `BlockAccessIndex = u64` alias behavior when paired with the `crate::quantity` module.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "rlp", derive(alloy_rlp::RlpEncodableWrapper, alloy_rlp::RlpDecodableWrapper))]
#[cfg_attr(feature = "borsh", derive(borsh::BorshSerialize, borsh::BorshDeserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[repr(transparent)]
pub struct BlockAccessIndex(pub u64);

impl BlockAccessIndex {
    /// Pre-execution slot (index `0`).
    pub const PRE_EXECUTION: Self = Self(0);

    /// Constructs a new [`BlockAccessIndex`] from a raw `u64`.
    #[inline]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Constructs a new [`BlockAccessIndex`] from a 0-based transaction index.
    ///
    /// Transaction changes start at index `1` in a block access list, so transaction
    /// index `0` maps to block access index `1`.
    #[inline]
    pub const fn from_tx_index(tx_index: u64) -> Self {
        Self(tx_index + 1)
    }

    /// Returns the raw `u64` value.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Bumps the index by 1.
    #[inline]
    pub const fn increment(&mut self) {
        self.0 += 1;
    }

    /// Classifies this index into a [`BlockAccessPhase`], given the number of transactions
    /// in the block.
    ///
    /// Returns:
    /// - `Some(BlockAccessPhase::PreExecution)` when the index is `0`.
    /// - `Some(BlockAccessPhase::Transaction(i))` when the index is in `1..=tx_len`, with `i =
    ///   index - 1` as a 0-based transaction index.
    /// - `Some(BlockAccessPhase::PostExecution)` when the index is exactly `tx_len + 1`.
    /// - `None` when the index is strictly greater than `tx_len + 1` (out of range for a block with
    ///   `tx_len` transactions).
    #[inline]
    pub const fn phase(self, tx_len: usize) -> Option<BlockAccessPhase> {
        // Widen `tx_len` to `u64` to compare against the index without risking
        // truncation on 32-bit targets.
        let tx_len_u64 = tx_len as u64;
        if self.0 == 0 {
            Some(BlockAccessPhase::PreExecution)
        } else if self.0 <= tx_len_u64 {
            // `self.0 >= 1` here, so the subtraction cannot underflow.
            // Casting back to `usize` is safe because `self.0 - 1 < tx_len <= usize::MAX`.
            Some(BlockAccessPhase::Transaction((self.0 - 1) as usize))
        } else if self.0 == tx_len_u64 + 1 {
            Some(BlockAccessPhase::PostExecution)
        } else {
            None
        }
    }
}

impl fmt::Display for BlockAccessIndex {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::LowerHex for BlockAccessIndex {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for BlockAccessIndex {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        alloy_primitives::U64::from(self.0).serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for BlockAccessIndex {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        alloy_primitives::U64::deserialize(deserializer).map(|value| Self(value.to()))
    }
}

/// Classification of a [`BlockAccessIndex`] within a block.
///
/// See [`BlockAccessIndex::phase`] for how indices map to phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BlockAccessPhase {
    /// Pre-execution slot (index `0`).
    PreExecution,
    /// Transaction execution slot. The inner value is the 0-based transaction index
    /// within the block (i.e. `block_access_index - 1`).
    Transaction(usize),
    /// Post-execution slot (index `tx_len + 1`).
    PostExecution,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_execution_is_index_zero() {
        assert_eq!(BlockAccessIndex::new(0).phase(0), Some(BlockAccessPhase::PreExecution));
        assert_eq!(BlockAccessIndex::new(0).phase(5), Some(BlockAccessPhase::PreExecution));
        assert_eq!(BlockAccessIndex::PRE_EXECUTION.phase(5), Some(BlockAccessPhase::PreExecution));
    }

    #[test]
    fn transaction_indices_are_one_based() {
        assert_eq!(BlockAccessIndex::new(1).phase(3), Some(BlockAccessPhase::Transaction(0)));
        assert_eq!(BlockAccessIndex::new(2).phase(3), Some(BlockAccessPhase::Transaction(1)));
        assert_eq!(BlockAccessIndex::new(3).phase(3), Some(BlockAccessPhase::Transaction(2)));
    }

    #[test]
    fn from_tx_index_offsets_by_one() {
        assert_eq!(BlockAccessIndex::from_tx_index(0), BlockAccessIndex::new(1));
        assert_eq!(BlockAccessIndex::from_tx_index(1), BlockAccessIndex::new(2));
    }

    #[test]
    fn post_execution_is_tx_len_plus_one() {
        assert_eq!(BlockAccessIndex::new(4).phase(3), Some(BlockAccessPhase::PostExecution));
        assert_eq!(BlockAccessIndex::new(1).phase(0), Some(BlockAccessPhase::PostExecution));
    }

    #[test]
    fn out_of_range_returns_none() {
        assert_eq!(BlockAccessIndex::new(5).phase(3), None);
        assert_eq!(BlockAccessIndex::new(u64::MAX).phase(3), None);
    }

    #[test]
    fn empty_block_has_only_pre_and_post() {
        assert_eq!(BlockAccessIndex::new(0).phase(0), Some(BlockAccessPhase::PreExecution));
        assert_eq!(BlockAccessIndex::new(1).phase(0), Some(BlockAccessPhase::PostExecution));
        assert_eq!(BlockAccessIndex::new(2).phase(0), None);
    }

    #[test]
    fn increment_bumps_by_one() {
        let mut idx = BlockAccessIndex::new(3);
        idx.increment();
        assert_eq!(idx, BlockAccessIndex::new(4));
    }

    #[test]
    fn new_and_get_roundtrip() {
        let idx = BlockAccessIndex::new(42);
        assert_eq!(idx.get(), 42);
    }

    #[test]
    fn display_matches_inner() {
        extern crate alloc;
        assert_eq!(alloc::format!("{}", BlockAccessIndex::new(7)), "7");
        assert_eq!(alloc::format!("{:x}", BlockAccessIndex::new(255)), "ff");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_hex_quantity_roundtrip() {
        let idx = BlockAccessIndex::new(26);
        let json = serde_json::to_string(&idx).unwrap();
        assert_eq!(json, "\"0x1a\"");
        let back: BlockAccessIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back, idx);
    }

    #[cfg(feature = "rlp")]
    #[test]
    fn rlp_matches_raw_u64() {
        use alloy_rlp::Decodable;
        let idx = BlockAccessIndex::new(300);
        let encoded = alloy_rlp::encode(idx);
        assert_eq!(encoded, alloy_rlp::encode(300u64));
        let decoded = BlockAccessIndex::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, idx);
    }
}
