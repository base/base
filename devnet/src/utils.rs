//! Utility functions for system tests.

const ALPHANUMERIC: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

/// Chain ID for the L1 devnet.
pub const L1_CHAIN_ID: u64 = 1337;
/// Chain ID for the L2 devnet.
pub const L2_CHAIN_ID: u64 = 84538453;

/// Generates a unique container name with the given prefix.
pub fn unique_name(prefix: &str) -> String {
    format!("{}-{}", prefix, nanoid::nanoid!(8, ALPHANUMERIC))
}
