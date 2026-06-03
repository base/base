//! ZK proof configuration types.

use eyre::Result;
use serde::{Deserialize, Serialize};

/// Proof request configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProofConfig {
    /// Proof backend mode.
    #[serde(default)]
    pub mode: ProofMode,
}

impl ProofConfig {
    /// Validates proof configuration.
    pub const fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// Proof backend mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProofMode {
    /// Dry-run local proof backend.
    #[default]
    DryRun,
    /// Cluster proof backend.
    Cluster,
}
