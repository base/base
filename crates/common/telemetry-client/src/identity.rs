//! The persisted node identity.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use tracing::warn;
use uuid::Uuid;

/// Failures encountered while loading or minting a node identity.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryIdError {
    /// The parent directory of the identity file could not be created.
    #[error("failed to create telemetry id directory {}: {source}", path.display())]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
    /// The identity file could not be written.
    #[error("failed to write telemetry id to {}: {source}", path.display())]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: io::Error,
    },
}

/// A node's telemetry identity: a random v4 UUID.
///
/// A UUID rather than a bare hex string because every store the reports land in has a native UUID
/// type. Identities minted before the switch were 32 undashed hex characters, which is exactly the
/// simple UUID form, so an existing `telemetry-id` file still parses and the node keeps its
/// identity across the upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TelemetryId(Uuid);

impl TelemetryId {
    /// Mints a fresh random identity.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Loads the identity at `path`, minting and persisting one if it is absent or unreadable.
    ///
    /// Minting logs a first-run disclosure banner stating what the node sends and how to turn
    /// it off. That banner lives here rather than at the call site so it cannot be forgotten:
    /// the first mint is exactly the moment an operator becomes a reporter.
    ///
    /// A file that exists but does not hold a well-formed ID is replaced rather than treated as
    /// fatal. A truncated write from a crash should not stop a node from starting.
    pub fn load_or_create(path: &Path) -> Result<Self, TelemetryIdError> {
        if let Some(existing) = Self::read(path) {
            return Ok(existing);
        }

        let id = Self::generate();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| TelemetryIdError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(path, id.to_string())
            .map_err(|source| TelemetryIdError::Write { path: path.to_path_buf(), source })?;

        warn!(
            target: "telemetry",
            path = %path.display(),
            "Base node telemetry is enabled. This node will periodically report its version, \
             chain head position, hardware, normalized config, and peer counts to Base. It never \
             reports the command line, keys, or panic messages. Disable it with \
             --telemetry.enabled=false, or run `base telemetry preview` to see the exact payload."
        );

        Ok(id)
    }

    /// Returns the identity as a `UUID`.
    pub const fn uuid(&self) -> Uuid {
        self.0
    }

    /// Reads a well-formed identity from `path`, or `None` if there is not one there.
    ///
    /// Unlike [`Self::load_or_create`] this never writes, so `base telemetry preview` can show
    /// the real identity of a node that already reports without making a node that does not
    /// report identifiable.
    pub fn read(path: &Path) -> Option<Self> {
        let contents = fs::read_to_string(path).ok()?;
        let Ok(id) = Uuid::parse_str(contents.trim()) else {
            warn!(
                target: "telemetry",
                path = %path.display(),
                "telemetry id file is malformed; minting a replacement"
            );
            return None;
        };
        Some(Self(id))
    }
}

impl fmt::Display for TelemetryId {
    /// Renders the hyphenated form, which is what gets persisted and reported.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_generated_ids_are_random_v4_uuids() {
        let first = TelemetryId::generate();
        let second = TelemetryId::generate();

        assert_eq!(first.uuid().get_version_num(), 4);
        assert_ne!(first, second, "two mints must not collide");
    }

    #[test]
    fn test_id_survives_a_reload() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("nested").join("telemetry-id");

        let minted = TelemetryId::load_or_create(&path).expect("mint should succeed");
        let reloaded = TelemetryId::load_or_create(&path).expect("reload should succeed");

        assert_eq!(minted, reloaded, "a restart must preserve the identity");
    }

    #[test]
    fn test_malformed_file_is_replaced_rather_than_fatal() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("telemetry-id");
        fs::write(&path, "not-a-telemetry-id").expect("seed the file");

        let id = TelemetryId::load_or_create(&path).expect("a bad file must not stop startup");
        assert_eq!(
            TelemetryId::load_or_create(&path).expect("reload"),
            id,
            "the replacement must be persisted, not re-minted every start"
        );
    }

    #[test]
    fn test_an_id_minted_before_the_uuid_switch_is_preserved() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("telemetry-id");
        fs::write(&path, "0123456789ABCDEF0123456789ABCDEF\n").expect("seed the file");

        let id = TelemetryId::load_or_create(&path).expect("load");
        assert_eq!(
            id.to_string(),
            "01234567-89ab-cdef-0123-456789abcdef",
            "an upgrade must not change a node's identity"
        );
    }
}
