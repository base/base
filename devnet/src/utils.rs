//! Utility functions for system tests.

const ALPHANUMERIC: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

/// Generates a unique container name with the given prefix.
pub fn unique_name(prefix: &str) -> String {
    format!("{}-{}", prefix, nanoid::nanoid!(8, ALPHANUMERIC))
}
