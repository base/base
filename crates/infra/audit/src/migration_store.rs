//! Atomic, same-pod state records. An emptyDir does not survive pod deletion.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{AuditMigration, MigrationBackend, MigrationError, MigrationStatus};

/// Persisted record; backend ownership is never exposed by public HTTP status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRecord {
    /// Exact immutable schema and registered-work identity.
    pub fingerprint: String,
    /// Public result/progress.
    pub status: MigrationStatus,
    /// Previous owned backend, including interrupted attempts.
    pub backend: Option<MigrationBackend>,
}

/// State file backed by deployment's same-pod volume.
#[derive(Debug, Clone)]
pub struct MigrationStore {
    /// File location; never included in diagnostic text.
    pub path: PathBuf,
}

impl MigrationStore {
    /// Reads a compatible record; new deliberate identity authorizes a new attempt.
    pub fn load(&self, run_id: &str) -> Result<Option<MigrationRecord>, MigrationError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(MigrationError::StateIo),
        };
        let record: MigrationRecord =
            serde_json::from_slice(&bytes).map_err(|_| MigrationError::StateCorrupt)?;
        if record.status.run_id != run_id {
            return Ok(None);
        }
        if record.fingerprint != AuditMigration::fingerprint()
            || record.status.version != 1
            || record.status.mode != "migrate_up"
        {
            return Err(MigrationError::StateCorrupt);
        }
        Ok(Some(record))
    }

    /// Saves mode0600 file, fsyncs data, atomically renames, and fsyncs directory.
    pub fn save(
        &self,
        status: &MigrationStatus,
        backend: Option<&MigrationBackend>,
    ) -> Result<(), MigrationError> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or(MigrationError::Configuration)?;
        let name = self.path.file_name().ok_or(MigrationError::Configuration)?;
        static TEMPORARY: AtomicU64 = AtomicU64::new(0);
        let temporary = parent.join(format!(
            ".{}.{}-{}-{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| MigrationError::StateIo)?
                .as_nanos(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            status: status.clone(),
            backend: backend.cloned(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| MigrationError::StateIo)?;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map_err(|_: std::io::Error| MigrationError::StateIo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MigrationState;
    use std::{
        env,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn atomic_terminal_restoration_and_deliberate_identity() {
        let dir = env::temp_dir().join(format!(
            "audit-state-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let store = MigrationStore { path: dir.join("state.json") };
        for state in [MigrationState::Succeeded, MigrationState::Failed, MigrationState::Stopped] {
            let mut status = MigrationStatus::new("pod-one".into());
            status.state = state;
            store.save(&status, None).unwrap();
            assert_eq!(store.load("pod-one").unwrap().unwrap().status.state, state);
            assert!(store.load("pod-two").unwrap().is_none());
        }
        fs::write(&store.path, "broken secret-text").unwrap();
        assert_eq!(store.load("pod-one").unwrap_err().code(), "state_corrupt");
        fs::remove_file(&store.path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
