use serde::{Deserialize, Serialize};

/// Configuration for a workload generator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadConfig {
    /// Name of the workload.
    pub name: String,
    /// Random seed for reproducibility.
    pub seed: Option<u64>,
}

impl WorkloadConfig {
    /// Creates a new workload configuration with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), seed: None }
    }

    /// Sets the random seed for reproducible workload generation.
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self::new("default")
    }
}
