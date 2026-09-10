use std::path::{Path, PathBuf};

/// Common ETL related configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct EtlConfig {
    /// Data directory where temporary files are created.
    pub dir: Option<PathBuf>,
    /// The maximum size in bytes of data held in memory before being flushed to disk as a file.
    pub file_size: usize,
}

impl Default for EtlConfig {
    fn default() -> Self {
        Self { dir: None, file_size: Self::default_file_size() }
    }
}

impl EtlConfig {
    /// Creates an ETL configuration
    pub const fn new(dir: Option<PathBuf>, file_size: usize) -> Self {
        Self { dir, file_size }
    }

    /// Return default ETL directory from datadir path.
    pub fn from_datadir(path: &Path) -> PathBuf {
        path.join("etl-tmp")
    }

    /// Default size in bytes of data held in memory before being flushed to disk as a file.
    pub const fn default_file_size() -> usize {
        // 500 MB
        500 * (1024 * 1024)
    }
}
