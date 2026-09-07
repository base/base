//! Artifact storage trait and data model.

use std::{fs, path::PathBuf};

use alloy_primitives::B256;
use dashmap::DashMap;
use revm_primitives::hardfork::SpecId;

use crate::{OptimizationLevel, eyre};

/// Runtime cache key: the minimal identity for a compiled program at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeCacheKey {
    /// The code hash of the contract bytecode.
    pub code_hash: B256,
    /// The EVM spec (hardfork) the code was compiled against.
    pub spec_id: SpecId,
}

/// Full artifact identity for persisted artifacts.
///
/// Persisted artifacts must match all fields of this key to be loaded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArtifactKey {
    /// The runtime cache key (code_hash + spec_id).
    pub runtime: RuntimeCacheKey,
    /// The compiler backend used.
    pub backend: BackendSelection,
    /// The optimization level used.
    pub opt_level: OptimizationLevel,
}

/// A stored artifact consisting of a manifest and a path to the compiled dylib.
#[derive(Clone, Debug)]
pub struct StoredArtifact {
    /// Metadata about the artifact.
    pub manifest: ArtifactManifest,
    /// Path to the shared library on disk. The store owns and manages these files.
    pub dylib_path: PathBuf,
}

/// Metadata for a stored artifact.
#[derive(Clone, Debug)]
pub struct ArtifactManifest {
    /// The full artifact key.
    pub artifact_key: ArtifactKey,
    /// The symbol name to look up in the loaded library.
    pub symbol_name: String,
    /// Length of the original bytecode.
    pub bytecode_len: usize,
    /// Length of the compiled artifact in bytes.
    pub artifact_len: usize,
    /// Creation timestamp (unix seconds).
    pub created_at_unix_secs: u64,
    /// Keccak-256 digest of the dylib bytes.
    pub content_hash: [u8; 32],
}

/// Backend selection for compilation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BackendSelection {
    /// Automatically select the best available backend.
    #[default]
    Auto,
    /// Use the LLVM backend.
    Llvm,
}

/// Trait for loading and storing compiled artifacts.
///
/// Implementations manage artifact files on the filesystem. The store owns the dylib files and
/// returns paths to them. The backend loads shared libraries directly from these paths.
pub trait ArtifactStore: Send + Sync + 'static {
    /// Loads all available artifacts from storage.
    fn load_all(&self) -> eyre::Result<Vec<(ArtifactKey, StoredArtifact)>>;

    /// Loads a single artifact by key.
    fn load(&self, key: &ArtifactKey) -> eyre::Result<Option<StoredArtifact>>;

    /// Stores an artifact. The `dylib_bytes` are the raw shared-library bytes to persist.
    /// The store writes them to disk and the returned path (via subsequent `load`) points there.
    fn store(
        &self,
        key: &ArtifactKey,
        manifest: &ArtifactManifest,
        dylib_bytes: &[u8],
    ) -> eyre::Result<()>;

    /// Deletes an artifact by key.
    fn delete(&self, key: &ArtifactKey) -> eyre::Result<()>;

    /// Clears all stored artifacts.
    fn clear(&self) -> eyre::Result<()>;
}

/// In-memory artifact index backed by dylib files in a temporary directory.
#[derive(Debug)]
pub struct RuntimeArtifactStore {
    dir: tempfile::TempDir,
    artifacts: DashMap<ArtifactKey, StoredArtifact>,
}

impl RuntimeArtifactStore {
    /// Creates an empty temporary runtime artifact store.
    pub fn new() -> eyre::Result<Self> {
        Ok(Self { dir: tempfile::tempdir()?, artifacts: DashMap::default() })
    }

    /// Returns the number of artifacts tracked by this store.
    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    /// Returns whether this store contains no artifacts.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn artifact_path(&self, key: &ArtifactKey) -> PathBuf {
        self.dir.path().join(format!(
            "{:x}_{:?}_{:?}_{:?}.so",
            key.runtime.code_hash, key.runtime.spec_id, key.backend, key.opt_level,
        ))
    }
}

impl ArtifactStore for RuntimeArtifactStore {
    fn load_all(&self) -> eyre::Result<Vec<(ArtifactKey, StoredArtifact)>> {
        Ok(self
            .artifacts
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect())
    }

    fn load(&self, key: &ArtifactKey) -> eyre::Result<Option<StoredArtifact>> {
        Ok(self.artifacts.get(key).map(|entry| entry.value().clone()))
    }

    fn store(
        &self,
        key: &ArtifactKey,
        manifest: &ArtifactManifest,
        dylib_bytes: &[u8],
    ) -> eyre::Result<()> {
        let path = self.artifact_path(key);
        fs::write(&path, dylib_bytes)?;
        self.artifacts
            .insert(key.clone(), StoredArtifact { manifest: manifest.clone(), dylib_path: path });
        Ok(())
    }

    fn delete(&self, key: &ArtifactKey) -> eyre::Result<()> {
        if let Some((_, artifact)) = self.artifacts.remove(key) {
            let _ = fs::remove_file(artifact.dylib_path);
        }
        Ok(())
    }

    fn clear(&self) -> eyre::Result<()> {
        let paths = self.artifacts.iter().map(|entry| entry.dylib_path.clone()).collect::<Vec<_>>();
        self.artifacts.clear();
        for path in paths {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use revm_primitives::hardfork::SpecId;
    use revmc_backend::OptimizationLevel;

    use super::{
        ArtifactKey, ArtifactManifest, ArtifactStore, BackendSelection, RuntimeArtifactStore,
        RuntimeCacheKey,
    };

    fn artifact_key(code_hash: B256) -> ArtifactKey {
        ArtifactKey {
            runtime: RuntimeCacheKey { code_hash, spec_id: SpecId::OSAKA },
            backend: BackendSelection::Llvm,
            opt_level: OptimizationLevel::Default,
        }
    }

    fn manifest(key: ArtifactKey, artifact_len: usize) -> ArtifactManifest {
        ArtifactManifest {
            artifact_key: key,
            symbol_name: "main".to_string(),
            bytecode_len: 3,
            artifact_len,
            created_at_unix_secs: 42,
            content_hash: [7; 32],
        }
    }

    #[test]
    fn runtime_artifact_store_starts_empty() {
        let store = RuntimeArtifactStore::new().unwrap();

        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn runtime_artifact_store_round_trips_artifacts() {
        let store = RuntimeArtifactStore::new().unwrap();
        let key = artifact_key(B256::with_last_byte(1));
        let manifest = manifest(key.clone(), 4);

        store.store(&key, &manifest, b"dylib").unwrap();

        assert_eq!(store.len(), 1);
        let loaded = store.load(&key).unwrap().unwrap();
        assert_eq!(loaded.manifest.symbol_name, "main");
        assert_eq!(loaded.manifest.artifact_len, 4);
        assert_eq!(std::fs::read(&loaded.dylib_path).unwrap(), b"dylib");

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, key);
        assert_eq!(all[0].1.manifest.content_hash, [7; 32]);
    }

    #[test]
    fn runtime_artifact_store_replaces_artifacts() {
        let store = RuntimeArtifactStore::new().unwrap();
        let key = artifact_key(B256::with_last_byte(2));

        store.store(&key, &manifest(key.clone(), 3), b"one").unwrap();
        store.store(&key, &manifest(key.clone(), 5), b"three").unwrap();

        assert_eq!(store.len(), 1);
        let loaded = store.load(&key).unwrap().unwrap();
        assert_eq!(loaded.manifest.artifact_len, 5);
        assert_eq!(std::fs::read(&loaded.dylib_path).unwrap(), b"three");
    }

    #[test]
    fn runtime_artifact_store_delete_and_clear_remove_files() {
        let store = RuntimeArtifactStore::new().unwrap();
        let first = artifact_key(B256::with_last_byte(3));
        let second = artifact_key(B256::with_last_byte(4));

        store.store(&first, &manifest(first.clone(), 1), b"a").unwrap();
        store.store(&second, &manifest(second.clone(), 1), b"b").unwrap();
        let first_path = store.load(&first).unwrap().unwrap().dylib_path;
        let second_path = store.load(&second).unwrap().unwrap().dylib_path;

        store.delete(&first).unwrap();
        assert!(store.load(&first).unwrap().is_none());
        assert!(!first_path.exists());
        assert!(second_path.exists());

        store.clear().unwrap();
        assert!(store.is_empty());
        assert!(!second_path.exists());
    }

    #[test]
    fn backend_selection_defaults_to_auto() {
        assert_eq!(BackendSelection::default(), BackendSelection::Auto);
    }
}
