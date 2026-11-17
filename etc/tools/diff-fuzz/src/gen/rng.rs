//! Deterministic random source for the case generator.

use alloy_primitives::{Address, B256, U256, keccak256};

/// A small deterministic PRNG (xoshiro256** seeded through `SplitMix64`).
///
/// Its output is part of the case format: a case is regenerated from its seed and index, so the
/// sequence must never depend on platform, library versions, or hash-map ordering.
#[derive(Debug, Clone)]
pub struct Rng {
    state: [u64; 4],
}

impl Rng {
    /// Creates a generator from a list of seed words.
    pub fn new(words: &[u64]) -> Self {
        let mut bytes = Vec::with_capacity(words.len() * 8);
        for word in words {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        let digest = keccak256(&bytes);
        let mut seed = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
        let mut state = [0u64; 4];
        for slot in &mut state {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            *slot = z ^ (z >> 31);
        }
        Self { state }
    }

    /// Derives an independent generator for a named sub-stream.
    pub fn fork(&mut self, label: u64) -> Self {
        Self::new(&[self.next_u64(), label])
    }

    /// Returns the next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        let result = self.state[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..n` (`n > 0`).
    pub const fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform value in `lo..=hi`.
    pub const fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// Uniform index into a collection of length `len` (`len > 0`).
    pub const fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// True with probability `percent / 100`.
    pub const fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// Picks an element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }

    /// Picks an index according to integer weights.
    pub fn weighted(&mut self, weights: &[u64]) -> usize {
        let total: u64 = weights.iter().sum();
        let mut roll = self.below(total.max(1));
        for (index, weight) in weights.iter().enumerate() {
            if roll < *weight {
                return index;
            }
            roll -= weight;
        }
        weights.len() - 1
    }

    /// Random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next_u64().to_be_bytes());
        }
        out.truncate(len);
        out
    }

    /// Random bytes of a random length in `lo..=hi`.
    pub fn bytes_between(&mut self, lo: u64, hi: u64) -> Vec<u8> {
        let len = self.range(lo, hi) as usize;
        self.bytes(len)
    }

    /// A random 32-byte word.
    pub fn b256(&mut self) -> B256 {
        B256::from_slice(&self.bytes(32))
    }

    /// A random address.
    pub fn address(&mut self) -> Address {
        Address::from_slice(&self.bytes(20))
    }

    /// A random 256-bit integer, uniformly distributed.
    pub fn u256(&mut self) -> U256 {
        U256::from_be_slice(&self.bytes(32))
    }

    /// A 256-bit integer biased towards values where arithmetic changes behavior.
    pub fn edge_u256(&mut self) -> U256 {
        match self.below(14) {
            0 => U256::ZERO,
            1 => U256::from(1),
            2 => U256::from(2),
            3 => U256::MAX,
            4 => U256::MAX - U256::from(1),
            5 => U256::from(1) << 255,
            6 => (U256::from(1) << 255) - U256::from(1),
            7 => U256::from(1) << self.below(256) as usize,
            8 => (U256::from(1) << self.below(256) as usize) - U256::from(1),
            9 => U256::from(self.below(256)),
            10 => U256::from(self.next_u64()),
            11 => U256::MAX - U256::from(self.below(256)),
            12 => U256::from(32 * self.below(64)),
            _ => self.u256(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(&[1, 2]);
        let mut b = Rng::new(&[1, 2]);
        let mut c = Rng::new(&[1, 3]);
        let xs: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
    }
}
