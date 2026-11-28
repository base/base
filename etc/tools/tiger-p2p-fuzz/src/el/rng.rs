//! A small deterministic generator (`SplitMix64`), so every run is reproducible from its seed.

/// Deterministic random source.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    /// Seeds the generator.
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The generator for case `index` of a run seeded with `seed`. Hashing the seed first keeps
    /// the cases of different seeds apart (with `seed ^ index`, seeds 1 and 7 would reuse each
    /// other's node keys, and live targets remember keys across runs through bans and tables).
    pub(crate) const fn case(seed: u64, index: u64) -> Self {
        Self(Self::new(seed).next_u64() ^ index)
    }

    /// The next 64 random bits.
    pub(crate) const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (`n > 0`).
    pub(crate) const fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability `1 / n`.
    pub(crate) const fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    /// One element of `items`.
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// `n` random bytes.
    pub(crate) fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }

    /// Fewer than `n` random bytes.
    pub(crate) fn bytes_below(&mut self, n: usize) -> Vec<u8> {
        let len = self.below(n);
        self.bytes(len)
    }

    /// A fixed-size random array.
    pub(crate) fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for b in &mut out {
            *b = self.next_u64() as u8;
        }
        out
    }

    /// An integer biased towards boundaries that decoders get wrong.
    pub(crate) fn edge_u64(&mut self) -> u64 {
        const EDGES: [u64; 16] = [
            0,
            1,
            0x7f,
            0x80,
            0xff,
            0x100,
            1024,
            1025,
            0xffff,
            0x1_0000,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            i64::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
            8453,
        ];
        match self.below(4) {
            0 => *self.pick(&EDGES),
            1 => self.next_u64() >> self.below(64),
            _ => self.below(64) as u64,
        }
    }
}
