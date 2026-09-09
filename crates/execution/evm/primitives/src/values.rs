//! EVM storage aliases, currency units, and short-address lookup.

use crate::{Address, U256, U256Map};

/// Type alias for EVM storage keys (256-bit unsigned integers).
/// Used to identify storage slots within smart contract storage.
pub type StorageKey = U256;

/// Type alias for EVM storage values (256-bit unsigned integers).
/// Used to store data values in smart contract storage slots.
pub type StorageValue = U256;

/// Type alias for a map with storage keys (U256) as keys.
pub type StorageKeyMap<V> = U256Map<V>;

/// Optimize short address access.
pub const SHORT_ADDRESS_CAP: usize = 300;

/// Returns the short address from Address.
///
/// Short address is considered address that has 18 leading zeros
/// and last two bytes are less than [`SHORT_ADDRESS_CAP`].
#[inline]
pub fn short_address(address: &Address) -> Option<usize> {
    let (zeros, value) = address.split_at(18);
    if zeros.iter().all(|b| *b == 0) {
        let short_address = u16::from_be_bytes([value[0], value[1]]) as usize;
        if short_address < SHORT_ADDRESS_CAP {
            return Some(short_address);
        }
    }
    None
}

/// 1 ether = 10^18 wei
pub const ONE_ETHER: u128 = 1_000_000_000_000_000_000;

/// 1 gwei = 10^9 wei
pub const ONE_GWEI: u128 = 1_000_000_000;
