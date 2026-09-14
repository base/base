//! EIP-8130 2D nonce-manager storage-slot derivation.

use alloy_primitives::{Address, B256, U256, address, keccak256, uint};

/// ERC-7201 storage-slot derivation for the EIP-8130 2D nonce manager.
///
/// The nonce manager is a code-less EIP-8130 system account that persists
/// per-`(account, nonce_key)` 2D sequence nonces, plus the nonce-free replay
/// `expiring_nonce_seen` set, in the state trie. This is the engine-neutral slot
/// layout — the system-account address, the ERC-7201 base slots under the
/// `base.nonce_manager` namespace, and the Solidity mapping-slot derivation —
/// shared by every execution engine so their storage layouts cannot diverge.
///
/// It is a pure function of the address/key inputs (no state access): the read
/// and write paths that consume these slots live in the per-engine execution
/// crates. The base slots are hardcoded literals rather than derived at runtime
/// (keeping this crate dependency-free of the ERC-7201 macro machinery); a unit
/// test recomputes them from the namespace string so they cannot silently drift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NonceManagerSlots;

impl NonceManagerSlots {
    /// The nonce-manager system account address (`NONCE_MANAGER_ADDRESS`, EIP-8130
    /// constant table).
    pub const ADDRESS: Address = address!("813000000000000000000000000000000000aa01");

    /// ERC-7201 base storage slot of the `nonces` mapping under the
    /// `base.nonce_manager` namespace (field 0).
    pub const NONCES_BASE_SLOT: U256 =
        uint!(0x9d3ea32ad25774a46482ebbae019e8da4242109d164d324a59aa787515b9fe00_U256);

    /// ERC-7201 base storage slot of the nonce-free replay `expiring_nonce_seen`
    /// mapping (field 1).
    pub const EXPIRING_NONCE_SEEN_BASE_SLOT: U256 =
        uint!(0x9d3ea32ad25774a46482ebbae019e8da4242109d164d324a59aa787515b9fe01_U256);

    /// Nonce key reserved for the protocol nonce, which is held in account state
    /// rather than by the nonce manager.
    pub const PROTOCOL_NONCE_KEY: U256 = U256::ZERO;

    /// Returns the storage slot holding the 2D channel nonce for
    /// `nonces[account][nonce_key]`, or `None` for the reserved protocol nonce key
    /// (`0`), which lives in account state.
    ///
    /// Two nested Solidity mappings, `account => (nonce_key => base)` (outer key
    /// `account`, inner key `nonce_key`, matching the reference
    /// `mapping(address => mapping(uint256 => u64))`), each slot
    /// `keccak256(pad32(key) ++ be32(slot))`.
    #[must_use]
    pub fn nonce_slot(account: Address, nonce_key: U256) -> Option<U256> {
        if nonce_key == Self::PROTOCOL_NONCE_KEY {
            return None;
        }
        let inner = Self::address_mapping_slot(account, Self::NONCES_BASE_SLOT);
        Some(Self::u256_mapping_slot(nonce_key, inner))
    }

    /// Returns the storage slot holding the recorded expiry for a nonce-free
    /// transaction's `replay_id`.
    #[must_use]
    pub fn expiring_nonce_seen_slot(replay_id: B256) -> U256 {
        Self::u256_mapping_slot(
            U256::from_be_bytes(replay_id.0),
            Self::EXPIRING_NONCE_SEEN_BASE_SLOT,
        )
    }

    /// The Solidity slot for `mapping[address_key]` at base `slot`:
    /// `keccak256(pad32(key) ++ slot)`.
    fn address_mapping_slot(key: Address, slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(key.as_slice());
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    /// The Solidity slot for `mapping[u256_key]` at base `slot`:
    /// `keccak256(be32(key) ++ slot)`.
    fn u256_mapping_slot(key: U256, slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&key.to_be_bytes::<32>());
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_slots_match_erc7201_derivation() {
        // Recompute the ERC-7201 root for `base.nonce_manager`:
        // `keccak256(keccak256(namespace) - 1) & ~0xff`. The hardcoded base-slot
        // constants must equal the derived root (field 0) and root + 1 (field 1),
        // so they cannot silently drift from the namespace they claim to derive.
        let id_hash = U256::from_be_bytes(keccak256("base.nonce_manager").0);
        let shifted = id_hash - U256::from(1u64);
        let root =
            U256::from_be_bytes(keccak256(shifted.to_be_bytes::<32>()).0) & !U256::from(0xffu64);

        assert_eq!(NonceManagerSlots::NONCES_BASE_SLOT, root);
        assert_eq!(NonceManagerSlots::EXPIRING_NONCE_SEEN_BASE_SLOT, root + U256::from(1u64));
    }

    #[test]
    fn protocol_nonce_key_has_no_channel_slot() {
        let account = address!("0x00000000000000000000000000000000000000aa");
        assert_eq!(
            NonceManagerSlots::nonce_slot(account, NonceManagerSlots::PROTOCOL_NONCE_KEY),
            None
        );
    }

    #[test]
    fn nonce_slot_is_distinct_per_account_and_key() {
        let a = address!("0x00000000000000000000000000000000000000aa");
        let b = address!("0x1111111111111111111111111111111111111111");
        let slot_a1 = NonceManagerSlots::nonce_slot(a, U256::from(1u64)).unwrap();
        let slot_a2 = NonceManagerSlots::nonce_slot(a, U256::from(2u64)).unwrap();
        let slot_b1 = NonceManagerSlots::nonce_slot(b, U256::from(1u64)).unwrap();
        assert_ne!(slot_a1, slot_a2);
        assert_ne!(slot_a1, slot_b1);
    }
}
