//! Snapshot management: caching and creation of node data directories.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::info;

use crate::config::{DatadirConfig, SnapshotConfig};
use crate::error::BenchmarkError;
use crate::process::ProcessHandle;

/// Cache key identifying a unique snapshot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SnapshotKey {
    node_type: String,
    role: String,
    command_hash: String,
}

impl SnapshotKey {
    fn new(node_type: &str, role: &str, command: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(command.as_bytes());
        let hash = hex::encode(&hasher.finalize()[..6]);
        Self { node_type: node_type.into(), role: role.into(), command_hash: hash }
    }
}

/// Manages creation and caching of node data directory snapshots.
///
/// Snapshots are cached by `(node_type, role, sha256(command)[:12])` so the
/// same snapshot script is only run once per benchmark session unless
/// `force_clean` is set.
#[derive(Debug, Default)]
pub struct SnapshotManager {
    cache: HashMap<SnapshotKey, PathBuf>,
    snapshots_dir: PathBuf,
}

impl SnapshotManager {
    /// Create a new manager that stores snapshots under `snapshots_dir`.
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self { cache: HashMap::new(), snapshots_dir }
    }

    /// Resolve the data directory for a given node type and role.
    ///
    /// If `datadir_config` provides an explicit path, it is used directly with
    /// no snapshot script invocation. Otherwise the snapshot script is run and
    /// its output path is cached for subsequent calls with the same key.
    pub async fn ensure_snapshot(
        &mut self,
        datadir_config: &DatadirConfig,
        snapshot: &SnapshotConfig,
        node_type: &str,
        role: &str,
    ) -> Result<PathBuf, BenchmarkError> {
        if let Some(explicit) = explicit_path(datadir_config, role) {
            return Ok(explicit);
        }

        let key = SnapshotKey::new(node_type, role, &snapshot.command);

        if !snapshot.force_clean {
            if let Some(cached) = self.cache.get(&key) {
                info!(
                    node_type = %node_type,
                    role = %role,
                    path = %cached.display(),
                    "using cached snapshot",
                );
                return Ok(cached.clone());
            }
        }

        let snapshot_path = self.snapshot_path(&key);
        if snapshot.force_clean && snapshot_path.exists() {
            fs::remove_dir_all(&snapshot_path).map_err(|e| {
                BenchmarkError::Snapshot(format!(
                    "failed to clean snapshot dir {}: {e}",
                    snapshot_path.display()
                ))
            })?;
        }
        fs::create_dir_all(&snapshot_path)?;

        self.run_snapshot_script(snapshot, node_type, &snapshot_path).await?;

        info!(
            node_type = %node_type,
            role = %role,
            path = %snapshot_path.display(),
            "snapshot created",
        );
        self.cache.insert(key, snapshot_path.clone());
        Ok(snapshot_path)
    }

    fn snapshot_path(&self, key: &SnapshotKey) -> PathBuf {
        self.snapshots_dir
            .join(format!("{}_{}_{}", key.node_type, key.role, key.command_hash))
    }

    async fn run_snapshot_script(
        &self,
        snapshot: &SnapshotConfig,
        node_type: &str,
        snapshot_path: &Path,
    ) -> Result<(), BenchmarkError> {
        let parts: Vec<&str> = snapshot.command.split_whitespace().collect();
        let (bin, extra_args) = parts.split_first().ok_or_else(|| {
            BenchmarkError::Snapshot("snapshot command is empty".into())
        })?;

        let mut args: Vec<String> = extra_args.iter().map(|s| s.to_string()).collect();
        args.push(node_type.to_string());
        args.push(snapshot_path.to_string_lossy().to_string());

        let devnull = fs::File::open("/dev/null")?;
        let stderr = devnull.try_clone()?;

        let mut handle = ProcessHandle::new(
            PathBuf::from(bin),
            args,
            vec![],
            devnull,
            stderr,
        );

        handle.start().await?;
        handle.wait().await.map_err(|e| {
            BenchmarkError::Snapshot(format!("snapshot script failed: {e}"))
        })
    }
}

fn explicit_path(config: &DatadirConfig, role: &str) -> Option<PathBuf> {
    match role {
        "sequencer" => config.sequencer.clone(),
        "validator" => config.validator.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn snapshot_key_hash_is_12_hex_chars() {
        let key = SnapshotKey::new("base-reth-node", "sequencer", "/usr/bin/my-script.sh");
        assert_eq!(key.command_hash.len(), 12);
        assert!(key.command_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn snapshot_key_same_command_same_hash() {
        let a = SnapshotKey::new("node", "seq", "cmd arg1");
        let b = SnapshotKey::new("node", "seq", "cmd arg1");
        assert_eq!(a.command_hash, b.command_hash);
    }

    #[test]
    fn snapshot_key_different_command_different_hash() {
        let a = SnapshotKey::new("node", "seq", "cmd arg1");
        let b = SnapshotKey::new("node", "seq", "cmd arg2");
        assert_ne!(a.command_hash, b.command_hash);
    }

    #[test]
    fn explicit_path_returns_correct_role() {
        let config = DatadirConfig {
            sequencer: Some(PathBuf::from("/seq")),
            validator: Some(PathBuf::from("/val")),
        };
        assert_eq!(explicit_path(&config, "sequencer"), Some(PathBuf::from("/seq")));
        assert_eq!(explicit_path(&config, "validator"), Some(PathBuf::from("/val")));
        assert_eq!(explicit_path(&config, "unknown"), None);
    }

    #[test]
    fn snapshot_path_format() {
        let tmp = tempdir().unwrap();
        let mgr = SnapshotManager::new(tmp.path().to_path_buf());
        let key = SnapshotKey {
            node_type: "base-reth-node".into(),
            role: "sequencer".into(),
            command_hash: "abcdef012345".into(),
        };
        let path = mgr.snapshot_path(&key);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "base-reth-node_sequencer_abcdef012345"
        );
    }

    #[tokio::test]
    async fn ensure_snapshot_uses_explicit_path() {
        let tmp = tempdir().unwrap();
        let explicit = tmp.path().join("mydata");
        fs::create_dir_all(&explicit).unwrap();

        let mut mgr = SnapshotManager::new(tmp.path().to_path_buf());
        let config = DatadirConfig { sequencer: Some(explicit.clone()), validator: None };
        let snap = SnapshotConfig {
            command: "unused".into(),
            genesis_file: None,
            force_clean: false,
        };

        let result = mgr.ensure_snapshot(&config, &snap, "base-reth-node", "sequencer").await.unwrap();
        assert_eq!(result, explicit);
    }

    #[tokio::test]
    async fn ensure_snapshot_caches_result() {
        let tmp = tempdir().unwrap();
        let mut mgr = SnapshotManager::new(tmp.path().to_path_buf());
        let config = DatadirConfig::default();
        let snap = SnapshotConfig {
            command: "true".into(),
            genesis_file: None,
            force_clean: false,
        };

        let a = mgr.ensure_snapshot(&config, &snap, "node", "sequencer").await.unwrap();
        let b = mgr.ensure_snapshot(&config, &snap, "node", "sequencer").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(mgr.cache.len(), 1);
    }
}
