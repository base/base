//! Pre-warmed address and storage-slot set for entries warmed before EVM execution begins.
//!
//! [`PrewarmSet`] holds the warm entries that come from chain/transaction configuration rather
//! than from an executing opcode: precompiles, the coinbase/beneficiary (EIP-3651), the EIP-2930
//! access list (addresses and their storage slots), and the non-revertible base warm accounts
//! (sender, recipient, EIP-7702 authorities, system contracts). It is a port of revm's
//! `WarmAddresses`, adapted to evm2's primitive types.
//!
//! Warm addresses are tracked in two places. Short addresses — 18 leading zero bytes and a
//! last-two-byte value below [`SHORT_ADDRESS_CAP`], which is where precompiles and similar
//! low-numbered addresses fall — live in a bit vector for fast lookups. Every other warm address,
//! along with any address that carries warm storage slots, lives in `access_list`.
//!
//! Note this is *not* the complete EIP-2929 initial warm set: the sender and recipient are also
//! warm from the start, but they are tracked per account by [`crate::evm::State`] instead — as is
//! all runtime warmth introduced while executing EVM code.

use alloy_primitives::{
    Address,
    map::{AddressMap, HashSet},
};
use bitvec::{bitvec, vec::BitVec};

use crate::interpreter::Word;

/// Short-address optimization cap. Addresses with 18 leading zero bytes whose last two bytes are
/// less than this value are tracked in a bit vector for fast warm lookups.
const SHORT_ADDRESS_CAP: u8 = u8::MAX;

/// Returns the short-address index for an address, if it qualifies.
///
/// A short address has 18 leading zero bytes and a last-two-byte value below [`SHORT_ADDRESS_CAP`].
#[inline]
fn short_address(address: &Address) -> Option<usize> {
    let (zeros, value) = address.split_at(19);
    if zeros.iter().all(|b| *b == 0) {
        let short_address = value[0];
        if short_address < SHORT_ADDRESS_CAP {
            return Some(short_address as usize);
        }
    }
    None
}

/// Stores addresses (and their storage slots) that are warm-loaded for the current transaction.
///
/// Warm addresses without warm storage are split by shape for lookup speed: short addresses (see
/// `SHORT_ADDRESS_CAP`) set a bit in `short_addresses`, while everything else — and any address
/// carrying warm storage slots — is held in `access_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrewarmSet {
    /// Bit vector of warm short addresses. An address shorter than [`SHORT_ADDRESS_CAP`] sets
    /// its bit here for faster access; precompiles and other low-numbered addresses land here.
    short_addresses: BitVec,
    /// Warm addresses keyed by address, each holding its warm storage slots.
    ///
    /// This holds the EIP-2930 access list, non-short warm addresses, and the non-revertible base
    /// warm entries (sender, recipient, EIP-7702 authorities, system contracts) added via
    /// [`Self::warm`] and [`Self::warm_storage`]: they are warmed
    /// before or alongside execution and survive [`crate::evm::State::rollback`]. An address
    /// present with an empty slot set is warm with no warm storage slots.
    access_list: AddressMap<HashSet<Word>>,
}

impl Default for PrewarmSet {
    fn default() -> Self {
        Self::new()
    }
}

impl PrewarmSet {
    /// Creates a new, empty warm-address set.
    #[inline]
    pub fn new() -> Self {
        Self {
            short_addresses: bitvec![0; SHORT_ADDRESS_CAP as usize],
            access_list: AddressMap::default(),
        }
    }

    /// Returns the access list (non-short warm addresses and all warm storage slots).
    #[inline]
    pub const fn access_list(&self) -> &AddressMap<HashSet<Word>> {
        &self.access_list
    }

    /// Marks an address as warm.
    ///
    /// Short addresses set a bit in `short_addresses`; all others are inserted into `access_list`
    /// with no warm storage slots. Used for precompiles, the coinbase/beneficiary, and the
    /// transaction-initial warmth established before any rollback checkpoint (sender, recipient,
    /// EIP-7702 authorities, system contracts). This warmth survives
    /// [`crate::evm::State::rollback`] and is cleared per transaction by
    /// [`Self::clear`].
    #[inline]
    pub fn warm(&mut self, address: &Address) {
        if self.warm_short_address(address) {
            return;
        }
        self.access_list.entry(*address).or_default();
    }

    /// Marks an address and a set of storage slots as warm.
    ///
    /// The slots are unioned into any slots already warm for the address. See [`Self::warm`] for
    /// the rollback and clearing semantics.
    #[inline]
    pub fn warm_storage(&mut self, address: &Address, slots: impl IntoIterator<Item = Word>) {
        let mut slots = slots.into_iter().peekable();
        if self.warm_short_address(address) {
            // A short address only needs a map entry once it carries storage slots.
            if slots.peek().is_none() {
                return;
            }
        }
        self.access_list.entry(*address).or_default().extend(slots);
    }

    /// Marks a short address as warm.
    ///
    /// Returns whether the address was short.
    #[inline]
    fn warm_short_address(&mut self, address: &Address) -> bool {
        if let Some(short_address) = short_address(address) {
            self.short_addresses.set(short_address, true);
            true
        } else {
            false
        }
    }

    /// Clears the per-transaction warm entries, leaving an empty set.
    #[inline]
    pub fn clear(&mut self) {
        self.short_addresses.fill(false);
        self.access_list.clear();
    }

    /// Returns whether the address is warm-loaded.
    pub fn is_warm(&self, address: &Address) -> bool {
        // check if it is a short address
        if let Some(short_address) = short_address(address)
            && self.short_addresses[short_address]
        {
            return true;
        }

        // otherwise check the access list (which also holds the base warm accounts)
        self.access_list.contains_key(address)
    }

    /// Returns whether the storage slot is warm-loaded.
    #[inline]
    pub fn is_storage_warm(&self, address: &Address, key: &Word) -> bool {
        self.access_list.get(address).is_some_and(|slots| slots.contains(key))
    }

    /// Returns whether the address is cold-loaded.
    #[inline]
    pub fn is_cold(&self, address: &Address) -> bool {
        !self.is_warm(address)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn test_initialization() {
        let prewarm_set = PrewarmSet::new();
        assert_eq!(prewarm_set.short_addresses.len(), SHORT_ADDRESS_CAP as usize);
        assert!(!prewarm_set.short_addresses.any());
        assert!(prewarm_set.access_list.is_empty());

        let default_addresses = PrewarmSet::default();
        assert_eq!(prewarm_set, default_addresses);
    }

    #[test]
    fn test_warm_short_addresses() {
        let mut prewarm_set = PrewarmSet::new();

        let short_addr1 = Address::with_last_byte(1);
        let short_addr2 = Address::with_last_byte(5);

        prewarm_set.warm(&short_addr1);
        prewarm_set.warm(&short_addr2);

        assert!(prewarm_set.short_addresses[1]);
        assert!(prewarm_set.short_addresses[5]);
        assert!(!prewarm_set.short_addresses[0]);

        assert!(prewarm_set.is_warm(&short_addr1));
        assert!(prewarm_set.is_warm(&short_addr2));
        assert!(!prewarm_set.is_warm(&Address::with_last_byte(20)));
    }

    #[test]
    fn test_warm_regular_addresses() {
        let mut prewarm_set = PrewarmSet::new();

        let regular_addr = address!("1234567890123456789012345678901234567890");
        let mut bytes = [0u8; 20];
        bytes[18] = 1u8;
        bytes[19] = 44u8; // 300
        let boundary_addr = Address::from(bytes);

        prewarm_set.warm(&regular_addr);
        prewarm_set.warm(&boundary_addr);

        // Non-short addresses are not tracked in the short-address bit vector.
        assert!(!prewarm_set.short_addresses.iter().any(|b| *b));

        assert!(prewarm_set.is_warm(&regular_addr));
        assert!(prewarm_set.is_warm(&boundary_addr));
        assert!(!prewarm_set.is_warm(&address!("0987654321098765432109876543210987654321")));
    }

    #[test]
    fn test_clear() {
        let mut prewarm_set = PrewarmSet::new();
        let short_addr = Address::with_last_byte(1);
        let regular_addr = address!("1234567890123456789012345678901234567890");

        prewarm_set.warm(&short_addr);
        prewarm_set.warm(&regular_addr);
        assert!(prewarm_set.is_warm(&short_addr));
        assert!(prewarm_set.is_warm(&regular_addr));

        prewarm_set.clear();
        assert!(!prewarm_set.is_warm(&short_addr));
        assert!(!prewarm_set.is_warm(&regular_addr));
    }

    #[test]
    fn test_warm_storage_slots() {
        let mut prewarm_set = PrewarmSet::new();
        let addr = address!("1234567890123456789012345678901234567890");
        let key = Word::from(7);

        prewarm_set.warm_storage(&addr, [key]);

        assert!(prewarm_set.is_warm(&addr));
        assert!(prewarm_set.is_storage_warm(&addr, &key));
        assert!(!prewarm_set.is_storage_warm(&addr, &Word::from(8)));
    }
}
