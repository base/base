use std::hash::{BuildHasher, Hasher};

/// A custom "hasher" that performs no hashing.
/// Instead, it uses the first 8 bytes of a byte array.
/// SAFETY: This is intended to be used only with keys that are certain
/// to have unique first 8 bytes, which are ~uniformly distributed.
pub struct BytesHasher {
    hash: u64,
}

impl Hasher for BytesHasher {
    /// Takes the first 8 bytes of the PreimageKey (which includes the type)
    /// and then converts them in little-endian order to a u64.
    fn write(&mut self, bytes: &[u8]) {
        // Note: write() will be called with the length first, but this check will skip it.
        if bytes.len() == 32 {
            self.hash = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
        }
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

/// A builder for the BytesHasher.
#[derive(Debug, Clone, Default)]
pub struct BytesHasherBuilder;

impl BuildHasher for BytesHasherBuilder {
    type Hasher = BytesHasher;

    fn build_hasher(&self) -> BytesHasher {
        BytesHasher { hash: 0 }
    }
}
