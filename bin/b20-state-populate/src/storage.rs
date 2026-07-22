//! Storage slot computation for B20 precompile and EVM ERC-20 layouts.

use alloy_primitives::{Address, B256, address, b256, keccak256};
use k256::ecdsa::SigningKey;
use rand::{RngCore, SeedableRng, rngs::StdRng};

/// Root namespace hash for `base.b20` (`B20CoreStorage`).
pub const B20_CORE_ROOT: B256 =
    b256!("c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000");

/// `B20CoreStorage` offset 3: total supply.
pub const B20_TOTAL_SUPPLY_SLOT: B256 =
    b256!("c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434003");

/// `B20CoreStorage` offset 4: balances mapping.
pub const B20_BALANCE_MAPPING_SLOT: B256 =
    b256!("c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434004");

/// `B20CoreStorage` offset 12: supply cap.
pub const B20_SUPPLY_CAP_SLOT: B256 =
    b256!("c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed4843400c");

/// Root namespace hash for `base.b20.asset` (`B20AssetExtensionStorage`).
pub const B20_ASSET_ROOT: B256 =
    b256!("fdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00");

/// `B20AssetExtensionStorage` offset 0: token decimals.
pub const B20_DECIMALS_SLOT: B256 = B20_ASSET_ROOT;

/// `B20AssetExtensionStorage` offset 1: token multiplier.
pub const B20_MULTIPLIER_SLOT: B256 =
    b256!("fdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b01");

/// Returns the balance storage slot for `who` in the B20 precompile layout.
pub fn b20_balance_slot(who: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(who.as_slice());
    buf[32..64].copy_from_slice(B20_BALANCE_MAPPING_SLOT.as_slice());
    keccak256(buf)
}

/// `B20CoreStorage` offset 14: initialized flag (EVM contract only).
pub const B20_INITIALIZED_SLOT: B256 =
    b256!("c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed4843400e");

/// Fixed address where the MockB20Asset EVM contract is deployed for benchmarking.
pub const EVM_TOKEN_ADDRESS: Address = address!("b200000000000000000000000000000000000ee2");

/// Deployed bytecode of `MockB20Asset` (compiled from `base-std`), embedded at build time.
pub const MOCK_B20_ASSET_BYTECODE: &[u8] = include_bytes!("mock_b20_asset.bin");

/// Returns the balance storage slot for `who` in a standard EVM ERC-20 (`_balances` at slot 0).
pub fn evm_erc20_balance_slot(who: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(who.as_slice());
    keccak256(buf)
}

/// Derives the B20 Asset token address from a creator and a salt.
pub fn derive_b20_asset_address(creator: Address, salt: B256) -> Address {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(creator.as_slice());
    buf[32..64].copy_from_slice(salt.as_slice());
    let hash = keccak256(buf);
    let mut addr = [0u8; 20];
    addr[0] = 0xb2;
    addr[10] = 0x00;
    addr[11..20].copy_from_slice(&hash[0..9]);
    Address::from(addr)
}

/// Derives `count` Ethereum addresses using the same seed-driven RNG as the load-tester.
///
/// The derivation must be byte-for-byte identical to `AccountPool::with_offset(seed, count, 0)`
/// in the load-tester so that pre-seeded balances are found by the correct sender accounts.
pub fn derive_sender_addresses(seed: u64, count: usize) -> Vec<Address> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut addresses = Vec::with_capacity(count);
    while addresses.len() < count {
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        if let Ok(key) = SigningKey::from_slice(&bytes) {
            let uncompressed = key.verifying_key().to_encoded_point(false);
            let hash = keccak256(&uncompressed.as_bytes()[1..]);
            addresses.push(Address::from_slice(&hash[12..]));
        }
    }
    addresses
}

/// Returns a deterministic, non-zero Ethereum address for the given sequential index.
pub fn address_for_index(idx: u64) -> Address {
    let idx = idx + 1;
    let mut addr = [0u8; 20];
    addr[12..20].copy_from_slice(&idx.to_be_bytes());
    Address::from(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_for_index_nonzero() {
        assert_ne!(address_for_index(0), Address::ZERO);
        assert_eq!(
            address_for_index(0),
            Address::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
    }

    #[test]
    fn balance_slot_deterministic() {
        let a = address_for_index(0);
        assert_eq!(b20_balance_slot(a), b20_balance_slot(a));
        assert_ne!(b20_balance_slot(a), b20_balance_slot(address_for_index(1)));
    }

    #[test]
    fn derive_address_prefix() {
        let creator = Address::from([0xAB; 20]);
        let salt = B256::ZERO;
        let addr = derive_b20_asset_address(creator, salt);
        assert_eq!(addr[0], 0xb2);
        assert_eq!(addr[10], 0x00);
    }
}
