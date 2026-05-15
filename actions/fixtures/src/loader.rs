use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;

use crate::{
    ActionFixture, DerivationFixture, ExpectedOutcome, FixtureL1Block, FixtureL1DiskBlock,
    FixtureL1DiskBlockError, FixtureL1DiskCodec, FixtureL2Block, FixtureManifest, FixturePaths,
    FixtureValidationError, FixtureValidator,
};

/// Loads fixtures from either a single JSON file or a fixture directory.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixtureLoader;

impl FixtureLoader {
    /// Load a fixture and validate it before returning.
    pub fn load(path: impl AsRef<Path>) -> Result<ActionFixture, FixtureLoaderError> {
        let path = path.as_ref();
        let fixture = if path.is_dir() { Self::load_dir(path)? } else { Self::load_file(path)? };
        FixtureValidator::validate(&fixture)?;
        Ok(fixture)
    }

    /// Load a single JSON file containing an [`ActionFixture`].
    pub fn load_file(path: &Path) -> Result<ActionFixture, FixtureLoaderError> {
        Self::read_json(path)
    }

    /// Load a fixture directory with `manifest.toml`, L1 data, `l2.json`, and `expected.json`.
    pub fn load_dir(path: &Path) -> Result<ActionFixture, FixtureLoaderError> {
        let manifest = Self::read_toml::<FixtureManifest>(&path.join(FixturePaths::MANIFEST))?;
        let l1_blocks = Self::read_l1_blocks(path)?;
        let l2_blocks =
            Self::read_json_or_default::<Vec<FixtureL2Block>>(&path.join(FixturePaths::L2))?;
        let expected =
            Self::read_json_or_default::<ExpectedOutcome>(&path.join(FixturePaths::EXPECTED))?;
        let derivation =
            Self::read_json_optional::<DerivationFixture>(&path.join(FixturePaths::DERIVATION))?;
        let mut fixture = ActionFixture::new(manifest, l1_blocks, l2_blocks, expected);
        fixture.derivation = derivation;
        Ok(fixture)
    }

    /// Read L1 blocks from the canonical compressed file, falling back to legacy JSON.
    pub fn read_l1_blocks(path: &Path) -> Result<Vec<FixtureL1Block>, FixtureLoaderError> {
        let compressed = path.join(FixturePaths::L1);
        if compressed.exists() {
            let blocks = Self::read_snappy_bincode::<Vec<FixtureL1DiskBlock>>(&compressed)?;
            return FixtureL1DiskCodec::decode_blocks(blocks).map_err(FixtureLoaderError::L1Disk);
        }

        let compressed_json = path.join(FixturePaths::L1_JSON_SNAP);
        if compressed_json.exists() {
            return Self::read_snappy_json(&compressed_json);
        }
        Self::read_json_or_default::<Vec<FixtureL1Block>>(&path.join(FixturePaths::L1_JSON))
    }

    /// Read JSON from a file.
    pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, FixtureLoaderError> {
        let data = fs::read_to_string(path)
            .map_err(|source| FixtureLoaderError::Read { path: path.to_path_buf(), source })?;
        serde_json::from_str(&data)
            .map_err(|source| FixtureLoaderError::Json { path: path.to_path_buf(), source })
    }

    /// Read JSON from a file, returning `T::default()` when the file is absent.
    pub fn read_json_or_default<T>(path: &Path) -> Result<T, FixtureLoaderError>
    where
        T: DeserializeOwned + Default,
    {
        if !path.exists() {
            return Ok(T::default());
        }
        Self::read_json(path)
    }

    /// Read optional JSON from a file, returning `None` when the file is absent.
    pub fn read_json_optional<T>(path: &Path) -> Result<Option<T>, FixtureLoaderError>
    where
        T: DeserializeOwned,
    {
        if !path.exists() {
            return Ok(None);
        }
        Self::read_json(path).map(Some)
    }

    /// Read Snappy-compressed JSON from a file.
    pub fn read_snappy_json<T: DeserializeOwned>(path: &Path) -> Result<T, FixtureLoaderError> {
        let data = fs::read(path)
            .map_err(|source| FixtureLoaderError::Read { path: path.to_path_buf(), source })?;
        let json = snap::raw::Decoder::new()
            .decompress_vec(&data)
            .map_err(|source| FixtureLoaderError::Snap { path: path.to_path_buf(), source })?;
        serde_json::from_slice(&json)
            .map_err(|source| FixtureLoaderError::Json { path: path.to_path_buf(), source })
    }

    /// Read Snappy-compressed bincode from a file.
    pub fn read_snappy_bincode<T: DeserializeOwned>(path: &Path) -> Result<T, FixtureLoaderError> {
        let data = fs::read(path)
            .map_err(|source| FixtureLoaderError::Read { path: path.to_path_buf(), source })?;
        let bytes = snap::raw::Decoder::new()
            .decompress_vec(&data)
            .map_err(|source| FixtureLoaderError::Snap { path: path.to_path_buf(), source })?;
        let (value, _) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).map_err(
                |source| FixtureLoaderError::BincodeDecode { path: path.to_path_buf(), source },
            )?;
        Ok(value)
    }

    /// Read TOML from a file.
    pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T, FixtureLoaderError> {
        let data = fs::read_to_string(path)
            .map_err(|source| FixtureLoaderError::Read { path: path.to_path_buf(), source })?;
        toml::from_str(&data)
            .map_err(|source| FixtureLoaderError::Toml { path: path.to_path_buf(), source })
    }
}

/// Fixture loading failure.
#[derive(Debug, thiserror::Error)]
pub enum FixtureLoaderError {
    /// Failed to read a fixture file.
    #[error("failed to read fixture file {path:?}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// Failed to parse JSON.
    #[error("failed to parse fixture JSON {path:?}: {source}")]
    Json {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// Failed to decompress Snappy data.
    #[error("failed to decompress fixture file {path:?}: {source}")]
    Snap {
        /// Path that failed to decompress.
        path: PathBuf,
        /// Underlying Snappy error.
        source: snap::Error,
    },
    /// Failed to parse bincode.
    #[error("failed to parse fixture bincode {path:?}: {source}")]
    BincodeDecode {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying bincode error.
        source: bincode::error::DecodeError,
    },
    /// Failed to parse TOML.
    #[error("failed to parse fixture TOML {path:?}: {source}")]
    Toml {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML error.
        source: toml::de::Error,
    },
    /// Fixture validation failed.
    #[error(transparent)]
    Validation(#[from] FixtureValidationError),
    /// L1 disk block decoding failed.
    #[error(transparent)]
    L1Disk(#[from] FixtureL1DiskBlockError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{ExpectedOutcome, FixtureKind, FixtureLoader, FixtureManifest, RpcFixtureCapture};

    #[test]
    fn loads_directory_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let _manifest = FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation);
        fs::write(
            dir.path().join("manifest.toml"),
            r#"
schema-version = 1
name = "window"
network = "base-mainnet"
kind = "derivation"
source = "manual"
"#,
        )
        .unwrap();
        fs::write(dir.path().join("l1.json"), "[]").unwrap();
        fs::write(dir.path().join("l2.json"), "[]").unwrap();
        fs::write(
            dir.path().join("expected.json"),
            serde_json::to_string(&ExpectedOutcome::default()).unwrap(),
        )
        .unwrap();
        let fixture = FixtureLoader::load(dir.path()).unwrap();
        assert_eq!(fixture.manifest.name, "window");
    }

    #[test]
    fn loads_compressed_l1_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let _manifest = FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation);
        fs::write(
            dir.path().join("manifest.toml"),
            r#"
schema-version = 1
name = "window"
network = "base-mainnet"
kind = "derivation"
source = "manual"
"#,
        )
        .unwrap();
        RpcFixtureCapture::write_snappy_bincode(
            &dir.path().join("l1.bin.snap"),
            &Vec::<crate::FixtureL1Block>::new(),
        )
        .unwrap();
        fs::write(dir.path().join("l2.json"), "[]").unwrap();
        fs::write(
            dir.path().join("expected.json"),
            serde_json::to_string(&ExpectedOutcome::default()).unwrap(),
        )
        .unwrap();
        let fixture = FixtureLoader::load(dir.path()).unwrap();
        assert_eq!(fixture.l1_blocks.len(), 0);
    }
}
