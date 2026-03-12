use rand::{
    Rng, SeedableRng,
    distr::{Distribution, StandardUniform},
    rngs::StdRng,
};

/// A seeded random number generator for reproducible workloads.
#[derive(Debug)]
pub struct SeededRng {
    rng: StdRng,
    seed: u64,
}

impl SeededRng {
    /// Creates a new seeded RNG.
    pub fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed), seed }
    }

    /// Returns the seed value.
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Resets the RNG to its initial state.
    pub fn reset(&mut self) {
        self.rng = StdRng::seed_from_u64(self.seed);
    }

    /// Generates a random value in the given range.
    pub fn gen_range<T, R>(&mut self, range: R) -> T
    where
        T: rand::distr::uniform::SampleUniform,
        R: rand::distr::uniform::SampleRange<T>,
    {
        self.rng.random_range(range)
    }

    /// Generates a random value.
    pub fn random<T>(&mut self) -> T
    where
        StandardUniform: Distribution<T>,
    {
        self.rng.random()
    }

    /// Generates random bytes.
    pub fn gen_bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut bytes = [0u8; N];
        self.rng.fill(&mut bytes);
        bytes
    }

    /// Returns a mutable reference to the inner RNG.
    pub const fn inner(&mut self) -> &mut StdRng {
        &mut self.rng
    }
}

impl Default for SeededRng {
    fn default() -> Self {
        Self::new(0)
    }
}
