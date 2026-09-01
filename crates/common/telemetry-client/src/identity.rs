//! The persisted node identity.

use std::{
    fmt, fs,
    io::{self, Write},
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
    ///
    /// Minting is atomic and racing callers converge, so the banner is logged by exactly the
    /// process whose ID reached the disk. See [`Self::install`] for why that matters: the
    /// telemetry ID is the fleet's primary key, and a lost race would count one operator twice.
    ///
    /// `path` is a location the caller has already resolved. Deciding there is nowhere durable to
    /// keep an identity happens where the config is assembled, as
    /// [`TelemetryConfig::id_path`](crate::TelemetryConfig::id_path) being `None`, so this
    /// function never has to guess what an unusable path means.
    pub fn load_or_create(path: &Path) -> Result<Self, TelemetryIdError> {
        if let Some(existing) = Self::read(path) {
            return Ok(existing);
        }

        let minted = Self::generate();
        let resolved = minted.install(path)?;
        if resolved != minted {
            return Ok(resolved);
        }

        warn!(
            target: "telemetry",
            path = %path.display(),
            "Base node telemetry is enabled. This node will periodically report its version, \
             chain head position, hardware, normalized config, and peer counts to Base. It never \
             reports the command line, keys, or panic messages. Disable it with \
             --telemetry.enabled=false, or run `base telemetry preview` to see the exact payload."
        );

        Ok(minted)
    }

    /// Persists this identity at `path` unless one is already there, returning whichever identity
    /// ended up on disk.
    ///
    /// Creating the file is a single atomic step: the contents are written to a temporary file in
    /// the same directory and linked into place, which fails rather than clobbers when another
    /// process got there first. A plain read-then-write lets two processes both observe absence
    /// and mint different IDs, and the loser would go on reporting under an ID no longer on disk.
    ///
    /// The link is what makes the file appear complete or not at all. `create_new` alone would
    /// also stop the second writer, but it leaves a window in which the racing reader opens a
    /// zero-length file, reads it as corrupt, and replaces the winner's ID.
    pub fn install(&self, path: &Path) -> Result<Self, TelemetryIdError> {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| TelemetryIdError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let temp = path.with_extension(format!("{}.tmp", self.0.simple()));
        if let Err(error) = Self::write_temp(&temp, self.to_string().as_bytes()) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }

        let Err(error) = fs::hard_link(&temp, path) else {
            let _ = fs::remove_file(&temp);
            return Ok(*self);
        };

        // The file already exists, so another process minted first and owns the identity for this
        // node. Adopt it: whoever wins the race must win it for every process on the box.
        if error.kind() == io::ErrorKind::AlreadyExists
            && let Some(existing) = Self::read(path)
        {
            let _ = fs::remove_file(&temp);
            return Ok(existing);
        }

        // Either the file holds no usable ID, or the filesystem has no hard links. A rename is
        // atomic in both cases, so a reader still never sees a partial file; it just cannot
        // refuse to overwrite, which is why the value is read back rather than assumed.
        fs::rename(&temp, path)
            .map_err(|source| TelemetryIdError::Write { path: path.to_path_buf(), source })?;
        Ok(Self::read(path).unwrap_or(*self))
    }

    /// Writes `contents` to `path` and flushes them to the device, so the file that gets linked
    /// into place is whole even if the machine loses power immediately afterwards.
    pub fn write_temp(path: &Path, contents: &[u8]) -> Result<(), TelemetryIdError> {
        let write_error = |source| TelemetryIdError::Write { path: path.to_path_buf(), source };
        let mut file = fs::File::create(path).map_err(write_error)?;
        file.write_all(contents).map_err(write_error)?;
        file.sync_all().map_err(write_error)
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
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

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
    fn test_racing_processes_settle_on_one_identity() {
        const RACERS: usize = 16;

        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("telemetry-id");
        let gate = Arc::new(Barrier::new(RACERS));

        let racers: Vec<_> = (0..RACERS)
            .map(|_| {
                let path = path.clone();
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    TelemetryId::load_or_create(&path).expect("a racing mint must not fail")
                })
            })
            .collect();
        let ids: Vec<_> = racers.into_iter().map(|racer| racer.join().expect("racer")).collect();

        let winner = TelemetryId::read(&path).expect("the race must leave a readable identity");
        assert!(
            ids.iter().all(|id| *id == winner),
            "every racer must report under the identity that reached the disk, got {ids:?}"
        );

        let left_behind: Vec<_> = fs::read_dir(dir.path())
            .expect("list the identity directory")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(
            left_behind,
            vec![path],
            "the losers must leave no second identity and no temp files"
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
