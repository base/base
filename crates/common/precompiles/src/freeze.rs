//! Compile-time integrity pinning for hardfork-frozen precompile logic.

use sha2_const_stable::Sha256;

/// Compile-time SHA-256 pin check for hardfork-frozen precompile source files.
///
/// Intended for use in a top-level `const _: () = assert!(...)` alongside
/// `include_bytes!`, so drift in a frozen file fails every build rather than
/// only a specific test.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrozenHash;

impl FrozenHash {
    /// Returns whether `source`'s SHA-256 digest equals `expected`.
    pub const fn matches(source: &[u8], expected: [u8; 32]) -> bool {
        let actual = Sha256::new().update(source).finalize();
        let mut i = 0;
        while i < 32 {
            if actual[i] != expected[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}
