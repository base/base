//! Public evidence and error types for the external kill-state anchor.

use thiserror::Error;

/// A CSPRNG-generated identity permanently bound to one external anchor store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnchorStoreIdentity([u8; 32]);

impl AnchorStoreIdentity {
    /// Constructs an identity from its owner-reviewed bootstrap evidence bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the byte-exact store identity.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[cfg(any(test, feature = "p0-provisioning"))]
/// Owner authorization for the state observed immediately before provisioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedAuthorization {
    /// Epoch that the observed record and local high-water mark must both carry.
    pub expected_epoch: u64,
    /// SHA-256 digest of the exact raw `state.json` bytes authorized by the owner.
    pub record_digest: [u8; 32],
    /// Byte-exact paths-and-device digest authorized by the owner.
    pub paths_mount_digest: [u8; 32],
}

#[cfg(any(test, feature = "p0-provisioning"))]
impl SeedAuthorization {
    /// Constructs a pre-bootstrap authorization. It intentionally contains no store identity.
    pub const fn new(
        expected_epoch: u64,
        record_digest: [u8; 32],
        paths_mount_digest: [u8; 32],
    ) -> Self {
        Self { expected_epoch, record_digest, paths_mount_digest }
    }
}

#[cfg(any(test, feature = "p0-provisioning"))]
/// Owner-reviewable evidence emitted by a successful fresh-leaf bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapEvidence {
    /// Identity generated and committed by bootstrap.
    pub identity: AnchorStoreIdentity,
    /// Digest of the exact [`SeedAuthorization`] tuple.
    pub seed_auth_digest: [u8; 32],
    /// SHA-256 digest of the raw kill-state record observed by bootstrap.
    pub observed_record_digest: [u8; 32],
    /// Local high-water epoch observed by bootstrap.
    pub observed_local_hwm: u64,
    /// Digest binding the three pinned paths and both observed device identifiers.
    pub paths_mount_digest: [u8; 32],
    /// `st_dev` observed for the pinned kill-state directory.
    pub kill_dir_device_id: u64,
    /// `st_dev` observed for the pinned external-anchor directory.
    pub anchor_dir_device_id: u64,
}

/// A rejected attempt to move the external high-water mark backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("external anchor rollback attempted from {current} to {attempted}")]
pub struct Rollback {
    /// Rejected epoch.
    pub attempted: u64,
    /// Durable high-water epoch that was retained.
    pub current: u64,
}

/// Fail-closed external-anchor lifecycle and persistence errors.
#[derive(Debug, Error)]
pub enum AnchorError {
    /// A pinned path, file, owner, mode, or inode invariant was violated.
    #[error("external anchor path invariant failed: {0}")]
    Path(String),
    /// The anchor leaf already exists and bootstrap therefore refused to adopt it.
    #[error("external anchor leaf already exists")]
    AlreadyExists,
    /// The production anchor leaf is absent or empty.
    #[error("external anchor store is absent or empty")]
    Missing,
    /// Another long-lived redb handle already holds the inode lock.
    #[error("external anchor store already has an opener")]
    NotSingletonWriter,
    /// A database operation failed.
    #[error("external anchor database operation failed: {0}")]
    Database(String),
    /// The fixed anchor row is absent or malformed.
    #[error("external anchor row is corrupt: {0}")]
    Corrupt(String),
    /// The store is a valid bootstrap intermediate but has not been activated.
    #[error("external anchor store is not initialized")]
    Uninitialized,
    /// The store identity does not equal the expected owner-reviewed identity.
    #[error("external anchor store identity mismatch")]
    IdentityMismatch,
    /// Provisioning evidence or its immediately re-observed local state did not match.
    #[error("external anchor provisioning evidence mismatch: {0}")]
    EvidenceMismatch(String),
    /// OS CSPRNG identity generation failed.
    #[error("external anchor identity generation failed: {0}")]
    Random(String),
    /// A monotonic observation attempted to regress the durable epoch.
    #[error(transparent)]
    Rollback(#[from] Rollback),
}
