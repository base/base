//! Compile-time integrity pinning for hardfork-frozen precompile logic.

use sha2_const_stable::Sha256;

/// Compile-time SHA-256 pin check for hardfork-frozen precompile source files.
///
/// Use alongside `include_bytes!` in a unit test to assert that a frozen
/// source file has not drifted. For small files, this can also be used in a
/// top-level `const _: () = assert!(...)` to fail the build instead of only
/// a test, though larger files may exceed rustc's const-eval step budget.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrozenHash;

impl FrozenHash {
    /// Returns the SHA-256 digest of `source`.
    pub const fn digest(source: &[u8]) -> [u8; 32] {
        Sha256::new().update(source).finalize()
    }

    /// Returns whether `source`'s SHA-256 digest equals `expected`.
    pub const fn matches(source: &[u8], expected: [u8; 32]) -> bool {
        let actual = Self::digest(source);
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
