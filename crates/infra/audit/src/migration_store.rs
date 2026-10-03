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
#[derive(Clone, Serialize, Deserialize)]
pub struct MigrationRecord {
    /// Exact immutable schema and registered-work identity.
    pub fingerprint: String,
    /// Actual database binding; local cache is never authoritative.
    #[serde(default)]
    pub target: Option<String>,
    /// Public result/progress.
    pub status: MigrationStatus,
    /// Previous owned backend, including interrupted attempts.
    pub backend: Option<MigrationBackend>,
}

impl std::fmt::Debug for MigrationRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MigrationRecord { persisted content redacted }")
    }
}

impl MigrationRecord {
    /// Rejects untrusted persisted text before publishing cached HTTP/log fields.
    pub fn validate(&self) -> Result<(), MigrationError> {
        let status = &self.status;
        let safe_error = status.error_code.as_deref().is_none_or(|code| {
            matches!(
                code,
                "configuration"
                    | "state_io"
                    | "state_corrupt"
                    | "database"
                    | "stop_requested"
                    | "cancellation_unconfirmed"
                    | "http"
                    | "worker"
            )
        });
        let safe_sqlstate = status.sqlstate.as_deref().is_none_or(|code| {
            code.len() == 5 && code.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        });
        let safe_operation = status
            .operation
            .as_deref()
            .is_none_or(|id| crate::RequiredAuditWork::ALL.iter().any(|work| work.id() == id));
        let safe_partition = status.partition.as_deref().is_none_or(|name| {
            ["hot", "warm", "cold"].iter().any(|class| {
                name.strip_prefix(&format!("transaction_events_{class}_"))
                    .is_some_and(|day| day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit()))
            })
        });
        if status.version != 1
            || status.mode != "migrate_up"
            || !safe_error
            || !safe_sqlstate
            || !safe_operation
            || !safe_partition
        {
            return Err(MigrationError::StateCorrupt);
        }
        Ok(())
    }
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
        record.validate()?;
        Ok(Some(record))
    }

    /// Saves mode0600 file, fsyncs data, atomically renames, and fsyncs directory.
    pub fn save(
        &self,
        status: &MigrationStatus,
        backend: Option<&MigrationBackend>,
    ) -> Result<(), MigrationError> {
        self.save_bound(status, backend, None)
    }

    /// Saves a local cache with actual target binding.
    pub fn save_bound(
        &self,
        status: &MigrationStatus,
        backend: Option<&MigrationBackend>,
        target: Option<&str>,
    ) -> Result<(), MigrationError> {
        let record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            target: target.map(str::to_owned),
            status: status.clone(),
            backend: backend.cloned(),
        };
        self.save_record(&record, || true)
    }

    /// Atomically writes a record only while its waiting caller still authorizes publication.
    /// Ordered cache writers prevent an older rename after a newer committed record.
    pub fn save_record(
        &self,
        record: &MigrationRecord,
        current: impl Fn() -> bool,
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
        let bytes = serde_json::to_vec(record).map_err(|_| MigrationError::StateIo)?;
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
            if !current() {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
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
    fn persisted_diagnostics_reject_untrusted_secret_text() {
        let mut record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            target: None,
            status: MigrationStatus::new("initial".into()),
            backend: None,
        };
        record.status.error_code = Some("postgres://secret-password@host".into());
        assert_eq!(record.validate().unwrap_err().code(), "state_corrupt");
        record.status.error_code = Some("database".into());
        record.status.sqlstate = Some("secret-password".into());
        assert_eq!(record.validate().unwrap_err().code(), "state_corrupt");
        record.status.sqlstate = Some("57014".into());
        record.status.partition = Some("secret-password".into());
        assert_eq!(record.validate().unwrap_err().code(), "state_corrupt");
        record.status.partition = Some("transaction_events_cold_20261002".into());
        assert!(record.validate().is_ok());
    }

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
