//! Utility functions for system tests.

use uuid::Uuid;

/// Number of random hex characters appended to generated container names.
const NAME_SUFFIX_LEN: usize = 8;

/// Generates a unique container name with the given prefix.
pub fn unique_name(prefix: &str) -> String {
    format!("{}-{}", prefix, &Uuid::new_v4().simple().to_string()[..NAME_SUFFIX_LEN])
}
