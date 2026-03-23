//! Precompile test scenario definitions.

use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::utils::{BaselineError, Result};

/// Configuration for a single precompile test scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    /// Target precompile name (sha256, blake2f, ecrecover, etc.).
    pub precompile: String,

    /// Number of rounds for blake2f.
    #[serde(default)]
    pub rounds: Option<u32>,

    /// Number of pairing pairs for `bn254_pairing`.
    #[serde(default)]
    pub pairs: Option<u32>,

    /// Input size in bytes for hash functions and identity.
    #[serde(default)]
    pub input_size: Option<usize>,

    /// Base length for modexp.
    #[serde(default)]
    pub base_len: Option<usize>,

    /// Exponent length for modexp.
    #[serde(default)]
    pub exp_len: Option<usize>,

    /// Modulus length for modexp.
    #[serde(default)]
    pub mod_len: Option<usize>,

    /// Number of iterations (calls per transaction).
    #[serde(default)]
    pub iterations: Option<u32>,

    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScenariosFile {
    scenarios: HashMap<String, ScenarioConfig>,
}

/// Registry of available precompile test scenarios.
#[derive(Debug, Clone)]
pub struct ScenariosRegistry {
    scenarios: HashMap<String, ScenarioConfig>,
}

impl ScenariosRegistry {
    /// Loads the default scenarios from the embedded config.
    pub fn load_default() -> Self {
        let yaml = include_str!("../../scenarios/precompiles.yaml");
        Self::from_yaml(yaml).expect("default scenarios should be valid")
    }

    /// Loads scenarios from a YAML file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| {
            BaselineError::Config(format!(
                "failed to read scenarios file {}: {}",
                path.display(),
                e
            ))
        })?;
        Self::from_yaml(&contents)
    }

    /// Parses scenarios from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let file: ScenariosFile = serde_yaml::from_str(yaml)
            .map_err(|e| BaselineError::Config(format!("failed to parse scenarios YAML: {e}")))?;
        Ok(Self { scenarios: file.scenarios })
    }

    /// Gets a scenario by name.
    pub fn get(&self, name: &str) -> Option<&ScenarioConfig> {
        self.scenarios.get(name)
    }

    /// Returns all scenario names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.scenarios.keys().map(String::as_str)
    }

    /// Returns the number of scenarios.
    pub fn len(&self) -> usize {
        self.scenarios.len()
    }

    /// Returns true if there are no scenarios.
    pub fn is_empty(&self) -> bool {
        self.scenarios.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_default_scenarios() {
        let registry = ScenariosRegistry::load_default();
        assert!(!registry.is_empty());
        assert!(registry.get("blake2f_heavy").is_some());
        assert!(registry.get("groth16_verify").is_some());
        assert!(registry.get("rsa_2048").is_some());
    }

    #[test]
    fn scenario_has_expected_fields() {
        let registry = ScenariosRegistry::load_default();

        let blake2f = registry.get("blake2f_heavy").unwrap();
        assert_eq!(blake2f.precompile, "blake2f");
        assert_eq!(blake2f.rounds, Some(200000));

        let rsa = registry.get("rsa_2048").unwrap();
        assert_eq!(rsa.precompile, "modexp");
        assert_eq!(rsa.base_len, Some(256));
    }
}
