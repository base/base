//! Address derivation and ERC-20 storage-slot computation.

use alloy_primitives::{Address, B256, keccak256};
use k256::ecdsa::SigningKey;
use rand::{RngCore, SeedableRng, rngs::StdRng};

/// Computes the storage slot of `_balances[who]` for a Solidity `mapping(address => uint256)`.
///
/// The slot is `keccak256(pad12(who) ++ mapping_slot)`, matching Solidity's mapping layout.
/// For a standard ERC-20 whose `_balances` mapping is the first state variable, `mapping_slot`
/// is `B256::ZERO`.
pub fn erc20_balance_slot(who: Address, mapping_slot: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(who.as_slice());
    buf[32..64].copy_from_slice(mapping_slot.as_slice());
    keccak256(buf)
}

/// Returns a deterministic non-zero address for the `idx`-th synthetic holder.
///
/// The address encodes `idx + 1` big-endian in its low 8 bytes, so index 0 maps to
/// `0x0000…0001` (never the zero address) and every index is unique.
pub fn address_for_index(idx: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..20].copy_from_slice(&(idx + 1).to_be_bytes());
    Address::from(bytes)
}

/// Derives the benchmark sender addresses for a `(seed, count)` pair.
///
/// This MUST stay byte-for-byte identical to the load generator's account derivation
/// (`AccountPool::with_offset(seed, count, 0)`): a `StdRng` seeded from `seed` produces each
/// secp256k1 private key, and the address is the low 20 bytes of the keccak256 of the
/// uncompressed public key. Any drift here would fund different accounts than the load test
/// signs from, leaving its senders with zero token balance.
pub fn derive_sender_addresses(seed: u64, count: usize) -> Vec<Address> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut addresses = Vec::with_capacity(count);
    for _ in 0..count {
        let mut key_bytes = [0u8; 32];
        rng.fill_bytes(&mut key_bytes);
        let signing_key = SigningKey::from_slice(&key_bytes).expect("valid secp256k1 key");
        let verifying_key = signing_key.verifying_key();
        let uncompressed = verifying_key.to_encoded_point(false);
        let hash = keccak256(&uncompressed.as_bytes()[1..]);
        addresses.push(Address::from_slice(&hash[12..]));
    }
    addresses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_for_index_is_nonzero_and_unique() {
        assert_ne!(address_for_index(0), Address::ZERO);
        assert_eq!(address_for_index(0), address_for_index(0));
        assert_ne!(address_for_index(0), address_for_index(1));
    }

    #[test]
    fn erc20_balance_slot_is_deterministic_and_key_dependent() {
        let a = address_for_index(1);
        let b = address_for_index(2);
        assert_eq!(erc20_balance_slot(a, B256::ZERO), erc20_balance_slot(a, B256::ZERO));
        assert_ne!(erc20_balance_slot(a, B256::ZERO), erc20_balance_slot(b, B256::ZERO));
    }

    #[test]
    fn derive_sender_addresses_is_reproducible() {
        assert_eq!(derive_sender_addresses(12345, 4), derive_sender_addresses(12345, 4));
        assert_ne!(derive_sender_addresses(1, 4), derive_sender_addresses(2, 4));
    }
}
