use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{ActionFixture, FixtureLoader, FixtureLoaderError, FixturePaths};

/// Catalog helper for loading checked-in fixtures by network and fixture name.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionFixtureCatalog;

impl ActionFixtureCatalog {
    /// Return the root directory that stores checked-in fixtures for this crate.
    pub fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    /// Return a catalog entry for a fixture.
    pub fn entry(network: impl Into<String>, name: impl Into<String>) -> FixtureCatalogEntry {
        let network = network.into();
        let name = name.into();
        let path = Self::root().join(&network).join(&name);
        FixtureCatalogEntry { network, name, path }
    }

    /// Load a checked-in fixture by network and name.
    pub fn load(
        network: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<ActionFixture, FixtureCatalogError> {
        let entry = Self::entry(network, name);
        entry.load()
    }

    /// List catalog entries under the fixture root.
    pub fn list() -> Result<Vec<FixtureCatalogEntry>, FixtureCatalogError> {
        let root = Self::root();
        let mut entries = Vec::new();
        if !root.exists() {
            return Ok(entries);
        }

        for network_entry in fs::read_dir(&root)
            .map_err(|source| FixtureCatalogError::ReadDir { path: root.clone(), source })?
        {
            let network_entry = network_entry
                .map_err(|source| FixtureCatalogError::ReadDir { path: root.clone(), source })?;
            let network_path = network_entry.path();
            if !network_path.is_dir() {
                continue;
            }
            let network = network_entry.file_name().to_string_lossy().into_owned();
            for fixture_entry in fs::read_dir(&network_path).map_err(|source| {
                FixtureCatalogError::ReadDir { path: network_path.clone(), source }
            })? {
                let fixture_entry = fixture_entry.map_err(|source| {
                    FixtureCatalogError::ReadDir { path: network_path.clone(), source }
                })?;
                let path = fixture_entry.path();
                if path.join(FixturePaths::MANIFEST).exists()
                    || path.join(FixturePaths::FIXTURE).exists()
                {
                    let name = fixture_entry.file_name().to_string_lossy().into_owned();
                    entries.push(FixtureCatalogEntry { network: network.clone(), name, path });
                }
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
}

/// One checked-in fixture entry in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureCatalogEntry {
    /// Network directory name.
    pub network: String,
    /// Fixture directory name.
    pub name: String,
    /// Absolute path to the fixture directory or file.
    pub path: PathBuf,
}

impl FixtureCatalogEntry {
    /// Create a catalog entry from a path.
    pub fn from_path(
        network: impl Into<String>,
        name: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Self {
        Self { network: network.into(), name: name.into(), path: path.as_ref().to_path_buf() }
    }

    /// Load this catalog entry.
    pub fn load(&self) -> Result<ActionFixture, FixtureCatalogError> {
        if self.path.join(FixturePaths::FIXTURE).exists() {
            return Ok(FixtureLoader::load(self.path.join(FixturePaths::FIXTURE))?);
        }
        Ok(FixtureLoader::load(&self.path)?)
    }
}

/// Catalog loading failure.
#[derive(Debug, thiserror::Error)]
pub enum FixtureCatalogError {
    /// Failed to read a catalog directory.
    #[error("failed to read fixture catalog directory {path:?}: {source}")]
    ReadDir {
        /// Directory path.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Fixture loading failed.
    #[error(transparent)]
    Load(#[from] FixtureLoaderError),
}

#[cfg(test)]
mod tests {
    use crate::ActionFixtureCatalog;

    #[test]
    fn root_points_at_crate_fixtures_dir() {
        assert!(ActionFixtureCatalog::root().ends_with("actions/fixtures/fixtures"));
    }
}
