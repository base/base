//! Fixed-schema redb storage and provisioning lifecycle for the external anchor.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use redb::{Durability, ReadableTable, ReadableTableMetadata, TableDefinition};
#[cfg(any(test, feature = "p0-provisioning"))]
use sha2::{Digest, Sha256};

use super::types::{AnchorError, AnchorStoreIdentity, Rollback};
#[cfg(any(test, feature = "p0-provisioning"))]
use super::types::{BootstrapEvidence, SeedAuthorization};

/// Pinned directory containing the existing kill-state record and local HWM.
pub const KILLSTATE_DIR: &str = "/home/ubuntu/.config/mev-killstate";
/// Pinned directory on the owner-provisioned external anchor volume.
pub const ANCHOR_DIR: &str = "/home/ubuntu/.config/mev-killstate-anchor";
/// Pinned regular-file redb leaf containing the external anchor row.
pub const ANCHOR_DB: &str = "/home/ubuntu/.config/mev-killstate-anchor/epoch-anchor.redb";
/// Byte-exact domain for the paths-and-device evidence digest.
pub const PATHS_MOUNT_DOMAIN: &[u8; 28] = b"base-mev:p0a:paths-mount:v1\0";
/// Byte-exact domain for the pre-bootstrap seed authorization digest.
pub const SEED_AUTH_DOMAIN: &[u8; 35] = b"base-mev:p0a:seed-authorization:v1\0";
/// P0-A intentionally ships without an owner-pinned identity and must not start production.
pub const EXPECTED_ANCHOR_IDENTITY: Option<AnchorStoreIdentity> = None;

#[cfg(any(test, feature = "p0-provisioning"))]
const PATHS_MOUNT_INPUT_LEN: usize = 181;
#[cfg(any(test, feature = "p0-provisioning"))]
const SEED_AUTH_INPUT_LEN: usize = 107;
const ANCHOR_ROW_LEN: usize = 74;
const ANCHOR_FORMAT_VERSION: u8 = 1;
const ANCHOR_KEY: &str = "anchor";
const ANCHOR_TABLE: TableDefinition<'static, &str, &[u8]> =
    TableDefinition::new("killstate_anchor");
#[cfg(any(test, feature = "p0-provisioning"))]
const STATE_FILE: &str = "state.json";
#[cfg(any(test, feature = "p0-provisioning"))]
const LOCAL_HWM_FILE: &str = "epoch.hwm";

/// Long-lived production handle holding redb's exclusive inode lock.
#[derive(Debug)]
pub(super) struct ExternalAnchorStore {
    db: redb::Database,
    db_path: PathBuf,
    identity: AnchorStoreIdentity,
    leaf_identity: LeafIdentity,
}
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObserveFault {
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
thread_local! {
    static OBSERVE_FAULT: std::cell::Cell<Option<ObserveFault>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(super) fn arm_observe_fault(fault: ObserveFault) {
    OBSERVE_FAULT.with(|armed| armed.set(Some(fault)));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeafIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct OpenedDatabase {
    db: redb::Database,
    leaf_identity: LeafIdentity,
}
impl ExternalAnchorStore {
    /// Opens only the existing initialized store and verifies its owner-pinned identity.
    pub(super) fn open_existing(expected: AnchorStoreIdentity) -> Result<Self, AnchorError> {
        open_existing_at(expected, validate_paths()?)
    }

    #[cfg(test)]
    pub(super) fn open_existing_for_test(
        expected: AnchorStoreIdentity,
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<Self, AnchorError> {
        open_existing_at(expected, validate_paths_at(kill_dir, anchor_dir, db_path)?)
    }

    /// Reads the initialized external high-water epoch, failing closed on malformed state.
    pub(super) fn read_hwm(&self) -> Result<u64, AnchorError> {
        self.verify_live_leaf()?;
        let row = read_row(&self.db)?;
        self.verify_live_leaf()?;
        if row.identity != self.identity {
            return Err(AnchorError::IdentityMismatch);
        }
        if !row.initialized {
            return Err(AnchorError::Uninitialized);
        }
        Ok(row.hwm)
    }

    /// Durably advances the external epoch, rejecting any regression.
    pub(super) fn observe(&self, epoch: u64) -> Result<(), AnchorError> {
        self.verify_live_leaf()?;
        #[cfg(test)]
        if OBSERVE_FAULT.with(|armed| {
            if armed.get() == Some(ObserveFault::BeforeCommit) {
                armed.set(None);
                true
            } else {
                false
            }
        }) {
            return Err(AnchorError::Database("test fault before external commit".to_string()));
        }
        let mut write = self.db.begin_write().map_err(database_error)?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write.open_table(ANCHOR_TABLE).map_err(database_error)?;
            if table.len().map_err(database_error)? != 1 {
                return Err(AnchorError::Corrupt(
                    "anchor table does not contain exactly one row".to_string(),
                ));
            }
            let bytes = table
                .get(ANCHOR_KEY)
                .map_err(database_error)?
                .ok_or_else(|| AnchorError::Corrupt("anchor row is absent".to_string()))?
                .value()
                .to_vec();
            let row = AnchorRow::decode(&bytes)?;
            if row.identity != self.identity {
                return Err(AnchorError::IdentityMismatch);
            }
            if !row.initialized {
                return Err(AnchorError::Uninitialized);
            }
            if epoch < row.hwm {
                return Err(Rollback { attempted: epoch, current: row.hwm }.into());
            }
            let updated = AnchorRow { hwm: epoch, ..row }.encode();
            table.insert(ANCHOR_KEY, updated.as_slice()).map_err(database_error)?;
        }
        write.commit().map_err(database_error)?;
        self.verify_live_leaf()?;
        #[cfg(test)]
        if OBSERVE_FAULT.with(|armed| {
            if armed.get() == Some(ObserveFault::AfterCommit) {
                armed.set(None);
                true
            } else {
                false
            }
        }) {
            return Err(AnchorError::Database("test fault after external commit".to_string()));
        }
        Ok(())
    }

    fn verify_live_leaf(&self) -> Result<(), AnchorError> {
        let metadata = std::fs::symlink_metadata(&self.db_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AnchorError::Missing
            } else {
                AnchorError::Path(error.to_string())
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(AnchorError::Path("anchor leaf path is not a regular file".to_string()));
        }
        if metadata.len() == 0 {
            return Err(AnchorError::Missing);
        }
        #[cfg(unix)]
        {
            let process_uid = process_uid()?;
            validate_unix_owner_mode(
                metadata.uid(),
                process_uid,
                metadata.permissions().mode(),
                0o600,
                "anchor leaf owner is not the process UID",
                "anchor leaf mode is not 0600",
            )?;
            let current = LeafIdentity { device: metadata.dev(), inode: metadata.ino() };
            if current != self.leaf_identity {
                return Err(AnchorError::Path(
                    "anchor leaf inode changed after admission".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Narrow provisioning-only façade; production builds contain no create or activation API.
#[cfg(any(test, feature = "p0-provisioning"))]
#[derive(Debug, Clone, Copy)]
pub struct AnchorProvisioner;

#[cfg(any(test, feature = "p0-provisioning"))]
impl AnchorProvisioner {
    /// Creates the pinned leaf exactly once with `initialized=false` and returns review evidence.
    pub fn bootstrap(authorization: SeedAuthorization) -> Result<BootstrapEvidence, AnchorError> {
        bootstrap_at(authorization, validate_paths()?)
    }

    #[cfg(test)]
    pub(super) fn bootstrap_for_test(
        authorization: SeedAuthorization,
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<BootstrapEvidence, AnchorError> {
        bootstrap_at(authorization, validate_paths_at(kill_dir, anchor_dir, db_path)?)
    }

    /// Activates the matching bootstrap intermediate after immediate record/local revalidation.
    pub fn activate(
        authorization: SeedAuthorization,
        evidence: BootstrapEvidence,
    ) -> Result<(), AnchorError> {
        activate_at(authorization, evidence, validate_paths()?)
    }

    #[cfg(test)]
    pub(super) fn activate_for_test(
        authorization: SeedAuthorization,
        evidence: BootstrapEvidence,
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<(), AnchorError> {
        activate_at(authorization, evidence, validate_paths_at(kill_dir, anchor_dir, db_path)?)
    }

    /// Computes the current byte-exact paths/device digest for pre-bootstrap owner review.
    pub fn current_paths_mount_digest() -> Result<[u8; 32], AnchorError> {
        paths_mount_digest(&validate_paths()?)
    }

    #[cfg(test)]
    pub(super) fn current_paths_mount_digest_for_test(
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<[u8; 32], AnchorError> {
        paths_mount_digest(&validate_paths_at(kill_dir, anchor_dir, db_path)?)
    }

    /// Computes SHA-256 over the exact current raw pinned kill-state record bytes.
    pub fn current_record_digest() -> Result<[u8; 32], AnchorError> {
        current_record_digest_at(&validate_paths()?)
    }

    #[cfg(test)]
    pub(super) fn current_record_digest_for_test(
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<[u8; 32], AnchorError> {
        current_record_digest_at(&validate_paths_at(kill_dir, anchor_dir, db_path)?)
    }
}

fn open_existing_at(
    expected: AnchorStoreIdentity,
    paths: ValidatedPaths,
) -> Result<ExternalAnchorStore, AnchorError> {
    let opened = open_database_file(&paths.db_path, false)?;
    let row = read_row(&opened.db)?;
    if row.identity != expected {
        return Err(AnchorError::IdentityMismatch);
    }
    if !row.initialized {
        return Err(AnchorError::Uninitialized);
    }
    Ok(ExternalAnchorStore {
        db: opened.db,
        db_path: paths.db_path,
        identity: row.identity,
        leaf_identity: opened.leaf_identity,
    })
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn bootstrap_at(
    authorization: SeedAuthorization,
    paths: ValidatedPaths,
) -> Result<BootstrapEvidence, AnchorError> {
    let observed = observe_local_state(&paths, authorization.expected_epoch)?;
    let paths_mount_digest = paths_mount_digest(&paths)?;
    if observed.record_digest != authorization.record_digest {
        return Err(AnchorError::EvidenceMismatch(
            "observed record digest differs from authorization".to_string(),
        ));
    }
    if paths_mount_digest != authorization.paths_mount_digest {
        return Err(AnchorError::EvidenceMismatch(
            "paths-and-device digest differs from authorization".to_string(),
        ));
    }
    let seed_auth_digest = authorization.digest();
    let file = create_fresh_leaf(&paths.db_path)?;
    let backend = redb::backends::FileBackend::new(file).map_err(database_error)?;
    let db = redb::Database::builder().create_with_backend(backend).map_err(database_error)?;
    let mut write = db.begin_write().map_err(database_error)?;
    write.set_durability(Durability::Immediate);
    let mut identity_bytes = [0u8; 32];
    getrandom::fill(&mut identity_bytes).map_err(|error| AnchorError::Random(error.to_string()))?;
    let identity = AnchorStoreIdentity::new(identity_bytes);
    {
        let mut table = write.open_table(ANCHOR_TABLE).map_err(database_error)?;
        if table.get(ANCHOR_KEY).map_err(database_error)?.is_some() {
            return Err(AnchorError::AlreadyExists);
        }
        let row = AnchorRow { identity, initialized: false, hwm: 0, seed_auth_digest }.encode();
        table.insert(ANCHOR_KEY, row.as_slice()).map_err(database_error)?;
    }
    write.commit().map_err(database_error)?;
    Ok(BootstrapEvidence {
        identity,
        seed_auth_digest,
        observed_record_digest: observed.record_digest,
        observed_local_hwm: observed.local_hwm,
        paths_mount_digest,
        kill_dir_device_id: paths.kill_dir_device_id,
        anchor_dir_device_id: paths.anchor_dir_device_id,
    })
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn activate_at(
    authorization: SeedAuthorization,
    evidence: BootstrapEvidence,
    paths: ValidatedPaths,
) -> Result<(), AnchorError> {
    let current_paths_digest = paths_mount_digest(&paths)?;
    let seed_auth_digest = authorization.digest();
    if seed_auth_digest != evidence.seed_auth_digest
        || authorization.paths_mount_digest != evidence.paths_mount_digest
        || current_paths_digest != evidence.paths_mount_digest
        || paths.kill_dir_device_id != evidence.kill_dir_device_id
        || paths.anchor_dir_device_id != evidence.anchor_dir_device_id
    {
        return Err(AnchorError::EvidenceMismatch(
            "bootstrap authorization or paths evidence differs".to_string(),
        ));
    }

    let opened = open_database_file(&paths.db_path, true)?;
    let db = opened.db;
    let mut write = db.begin_write().map_err(database_error)?;
    write.set_durability(Durability::Immediate);

    // This observation deliberately occurs after admission to the anchor write transaction
    // and immediately before the single activation-row commit.
    let observed = observe_local_state(&paths, authorization.expected_epoch)?;
    if observed.record_digest != authorization.record_digest
        || observed.record_digest != evidence.observed_record_digest
        || observed.local_hwm != evidence.observed_local_hwm
    {
        return Err(AnchorError::EvidenceMismatch(
            "record or local HWM changed before activation".to_string(),
        ));
    }

    {
        let mut table = write.open_table(ANCHOR_TABLE).map_err(database_error)?;
        if table.len().map_err(database_error)? != 1 {
            return Err(AnchorError::Corrupt(
                "anchor table does not contain exactly one row".to_string(),
            ));
        }
        let bytes = table
            .get(ANCHOR_KEY)
            .map_err(database_error)?
            .ok_or_else(|| AnchorError::Corrupt("anchor row is absent".to_string()))?
            .value()
            .to_vec();
        let row = AnchorRow::decode(&bytes)?;
        if row.initialized {
            return Err(AnchorError::EvidenceMismatch(
                "anchor store is already activated".to_string(),
            ));
        }
        if row.identity != evidence.identity {
            return Err(AnchorError::IdentityMismatch);
        }
        if row.seed_auth_digest != seed_auth_digest {
            return Err(AnchorError::EvidenceMismatch(
                "stored seed authorization digest differs".to_string(),
            ));
        }
        let activated =
            AnchorRow { initialized: true, hwm: authorization.expected_epoch, ..row }.encode();
        table.insert(ANCHOR_KEY, activated.as_slice()).map_err(database_error)?;
    }
    write.commit().map_err(database_error)
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn current_record_digest_at(paths: &ValidatedPaths) -> Result<[u8; 32], AnchorError> {
    let bytes = std::fs::read(paths.kill_dir.join(STATE_FILE))
        .map_err(|error| AnchorError::Path(error.to_string()))?;
    Ok(sha256(&bytes))
}

#[cfg(any(test, feature = "p0-provisioning"))]
impl SeedAuthorization {
    /// Computes `SHA-256(domain || epoch_LE || record_digest || paths_mount_digest)` (107 bytes).
    pub fn digest(&self) -> [u8; 32] {
        let mut input = Vec::with_capacity(SEED_AUTH_INPUT_LEN);
        input.extend_from_slice(SEED_AUTH_DOMAIN);
        input.extend_from_slice(&self.expected_epoch.to_le_bytes());
        input.extend_from_slice(&self.record_digest);
        input.extend_from_slice(&self.paths_mount_digest);
        debug_assert_eq!(input.len(), SEED_AUTH_INPUT_LEN);
        sha256(&input)
    }
}

#[derive(Debug, Clone, Copy)]
struct AnchorRow {
    identity: AnchorStoreIdentity,
    initialized: bool,
    hwm: u64,
    seed_auth_digest: [u8; 32],
}

impl AnchorRow {
    fn encode(self) -> [u8; ANCHOR_ROW_LEN] {
        let mut bytes = [0u8; ANCHOR_ROW_LEN];
        bytes[0] = ANCHOR_FORMAT_VERSION;
        bytes[1..33].copy_from_slice(self.identity.as_bytes());
        bytes[33] = u8::from(self.initialized);
        bytes[34..42].copy_from_slice(&self.hwm.to_le_bytes());
        bytes[42..74].copy_from_slice(&self.seed_auth_digest);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, AnchorError> {
        if bytes.len() != ANCHOR_ROW_LEN {
            return Err(AnchorError::Corrupt("anchor row length is not 74 bytes".to_string()));
        }
        if bytes[0] != ANCHOR_FORMAT_VERSION {
            return Err(AnchorError::Corrupt("anchor row version is not 1".to_string()));
        }
        let initialized = match bytes[33] {
            0 => false,
            1 => true,
            _ => return Err(AnchorError::Corrupt("initialized flag is not 0 or 1".to_string())),
        };
        let mut identity = [0u8; 32];
        identity.copy_from_slice(&bytes[1..33]);
        let mut hwm = [0u8; 8];
        hwm.copy_from_slice(&bytes[34..42]);
        let hwm = u64::from_le_bytes(hwm);
        if !initialized && hwm != 0 {
            return Err(AnchorError::Corrupt("uninitialized anchor has a nonzero HWM".to_string()));
        }
        let mut seed_auth_digest = [0u8; 32];
        seed_auth_digest.copy_from_slice(&bytes[42..74]);
        Ok(Self {
            identity: AnchorStoreIdentity::new(identity),
            initialized,
            hwm,
            seed_auth_digest,
        })
    }
}

#[derive(Debug)]
struct ValidatedPaths {
    #[cfg(any(test, feature = "p0-provisioning"))]
    kill_dir: PathBuf,
    db_path: PathBuf,
    #[cfg(any(test, feature = "p0-provisioning"))]
    kill_dir_device_id: u64,
    #[cfg(any(test, feature = "p0-provisioning"))]
    anchor_dir_device_id: u64,
}

#[cfg(any(test, feature = "p0-provisioning"))]
#[derive(Debug)]
struct LocalObservation {
    record_digest: [u8; 32],
    local_hwm: u64,
}

fn validate_paths() -> Result<ValidatedPaths, AnchorError> {
    validate_paths_at(Path::new(KILLSTATE_DIR), Path::new(ANCHOR_DIR), Path::new(ANCHOR_DB))
}

fn validate_paths_at(
    kill_dir_path: &Path,
    anchor_dir_path: &Path,
    db_path: &Path,
) -> Result<ValidatedPaths, AnchorError> {
    let kill_dir = validate_directory(kill_dir_path)?;
    let anchor_dir = validate_directory(anchor_dir_path)?;
    if db_path != anchor_dir.join("epoch-anchor.redb") {
        return Err(AnchorError::Path(
            "anchor leaf is not the fixed leaf of the validated anchor directory".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        #[cfg(any(test, feature = "p0-provisioning"))]
        let kill_metadata =
            std::fs::metadata(&kill_dir).map_err(|error| AnchorError::Path(error.to_string()))?;
        #[cfg(any(test, feature = "p0-provisioning"))]
        let anchor_metadata =
            std::fs::metadata(&anchor_dir).map_err(|error| AnchorError::Path(error.to_string()))?;
        #[cfg(not(any(test, feature = "p0-provisioning")))]
        let _ = kill_dir;
        Ok(ValidatedPaths {
            #[cfg(any(test, feature = "p0-provisioning"))]
            kill_dir,
            db_path: db_path.to_path_buf(),
            #[cfg(any(test, feature = "p0-provisioning"))]
            kill_dir_device_id: kill_metadata.dev(),
            #[cfg(any(test, feature = "p0-provisioning"))]
            anchor_dir_device_id: anchor_metadata.dev(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (kill_dir, anchor_dir, db_path);
        Err(AnchorError::Path("external anchor requires Unix inode semantics".to_string()))
    }
}

fn validate_directory(path: &Path) -> Result<PathBuf, AnchorError> {
    if !path.is_absolute() {
        return Err(AnchorError::Path("pinned directory is not absolute".to_string()));
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|error| AnchorError::Path(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(AnchorError::Path("pinned directory has a symlink ancestor".to_string()));
        }
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|error| AnchorError::Path(error.to_string()))?;
    if canonical != path {
        return Err(AnchorError::Path("pinned directory canonical path differs".to_string()));
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| AnchorError::Path(error.to_string()))?;
    if !metadata.file_type().is_dir() {
        return Err(AnchorError::Path("pinned path is not a directory".to_string()));
    }
    #[cfg(unix)]
    {
        let process_uid = process_uid()?;
        validate_unix_owner_mode(
            metadata.uid(),
            process_uid,
            metadata.permissions().mode(),
            0o700,
            "pinned directory owner is not the process UID",
            "pinned directory mode is not 0700",
        )?;
    }
    Ok(canonical)
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn create_fresh_leaf(path: &Path) -> Result<File, AnchorError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AnchorError::AlreadyExists
        } else {
            AnchorError::Path(error.to_string())
        }
    })?;
    validate_open_leaf(path, &file)?;
    Ok(file)
}

fn open_database_file(
    path: &Path,
    allow_uninitialized: bool,
) -> Result<OpenedDatabase, AnchorError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(false).truncate(false);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x20000); // O_NOFOLLOW
    let admission_file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AnchorError::Missing
        } else {
            AnchorError::Path(error.to_string())
        }
    })?;
    let leaf_identity = validate_open_leaf(path, &admission_file)?;
    if admission_file.metadata().map_err(|error| AnchorError::Path(error.to_string()))?.len() == 0 {
        return Err(AnchorError::Missing);
    }

    // Production opening must retain redb's open-existing semantics. The admission descriptor
    // prevents an unlinked inode from disappearing while redb acquires its long-lived lock; the
    // second comparison rejects a leaf substitution around that open.
    let db = redb::Database::open(path).map_err(database_open_error)?;
    if validate_open_leaf(path, &admission_file)? != leaf_identity {
        return Err(AnchorError::Path("anchor leaf inode changed during open".to_string()));
    }
    if !allow_uninitialized && !read_row(&db)?.initialized {
        return Err(AnchorError::Uninitialized);
    }
    Ok(OpenedDatabase { db, leaf_identity })
}

fn validate_open_leaf(path: &Path, file: &File) -> Result<LeafIdentity, AnchorError> {
    let descriptor = file.metadata().map_err(|error| AnchorError::Path(error.to_string()))?;
    if !descriptor.file_type().is_file() {
        return Err(AnchorError::Path("anchor leaf is not a regular file".to_string()));
    }
    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|error| AnchorError::Path(error.to_string()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
        return Err(AnchorError::Path("anchor leaf path is not a regular file".to_string()));
    }
    #[cfg(unix)]
    {
        let identity = LeafIdentity { device: descriptor.dev(), inode: descriptor.ino() };
        if identity != (LeafIdentity { device: path_metadata.dev(), inode: path_metadata.ino() }) {
            return Err(AnchorError::Path("anchor leaf inode changed during open".to_string()));
        }
        let process_uid = process_uid()?;
        validate_unix_owner_mode(
            descriptor.uid(),
            process_uid,
            descriptor.permissions().mode(),
            0o600,
            "anchor leaf owner is not the process UID",
            "anchor leaf mode is not 0600",
        )?;
        validate_unix_owner_mode(
            path_metadata.uid(),
            process_uid,
            path_metadata.permissions().mode(),
            0o600,
            "anchor leaf owner is not the process UID",
            "anchor leaf mode is not 0600",
        )?;
        Ok(identity)
    }
    #[cfg(not(unix))]
    {
        let _ = (path_metadata, descriptor);
        Err(AnchorError::Path("external anchor requires Unix inode semantics".to_string()))
    }
}

#[cfg(unix)]
fn process_uid() -> Result<u32, AnchorError> {
    Ok(std::fs::metadata("/proc/self").map_err(|error| AnchorError::Path(error.to_string()))?.uid())
}

#[cfg(unix)]
fn validate_unix_owner_mode(
    actual_uid: u32,
    expected_uid: u32,
    actual_mode: u32,
    expected_mode: u32,
    owner_error: &'static str,
    mode_error: &'static str,
) -> Result<(), AnchorError> {
    if actual_uid != expected_uid {
        return Err(AnchorError::Path(owner_error.to_string()));
    }
    if actual_mode & 0o777 != expected_mode {
        return Err(AnchorError::Path(mode_error.to_string()));
    }
    Ok(())
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn observe_local_state(
    paths: &ValidatedPaths,
    expected_epoch: u64,
) -> Result<LocalObservation, AnchorError> {
    let record = std::fs::read(paths.kill_dir.join(STATE_FILE))
        .map_err(|error| AnchorError::EvidenceMismatch(error.to_string()))?;
    let record_value: serde_json::Value = serde_json::from_slice(&record)
        .map_err(|error| AnchorError::EvidenceMismatch(error.to_string()))?;
    let record_epoch =
        record_value.get("epoch").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            AnchorError::EvidenceMismatch("record epoch is absent or invalid".to_string())
        })?;
    let state = record_value.get("state").and_then(serde_json::Value::as_str).ok_or_else(|| {
        AnchorError::EvidenceMismatch("record state is absent or invalid".to_string())
    })?;
    if state != "engaged" && state != "clear" {
        return Err(AnchorError::EvidenceMismatch("record state is unknown".to_string()));
    }

    let local_bytes = std::fs::read(paths.kill_dir.join(LOCAL_HWM_FILE))
        .map_err(|error| AnchorError::EvidenceMismatch(error.to_string()))?;
    let local_value: serde_json::Value = serde_json::from_slice(&local_bytes)
        .map_err(|error| AnchorError::EvidenceMismatch(error.to_string()))?;
    let local_hwm =
        local_value.get("high_water_epoch").and_then(serde_json::Value::as_u64).ok_or_else(
            || AnchorError::EvidenceMismatch("local HWM is absent or invalid".to_string()),
        )?;
    if record_epoch != expected_epoch || local_hwm != expected_epoch {
        return Err(AnchorError::EvidenceMismatch(
            "record epoch and local HWM do not equal the authorized epoch".to_string(),
        ));
    }
    Ok(LocalObservation { record_digest: sha256(&record), local_hwm })
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn paths_mount_digest(paths: &ValidatedPaths) -> Result<[u8; 32], AnchorError> {
    let mut input = Vec::with_capacity(PATHS_MOUNT_INPUT_LEN);
    input.extend_from_slice(PATHS_MOUNT_DOMAIN);
    input.extend_from_slice(KILLSTATE_DIR.as_bytes());
    input.push(0);
    input.extend_from_slice(ANCHOR_DIR.as_bytes());
    input.push(0);
    input.extend_from_slice(ANCHOR_DB.as_bytes());
    input.push(0);
    input.extend_from_slice(&paths.kill_dir_device_id.to_le_bytes());
    input.extend_from_slice(&paths.anchor_dir_device_id.to_le_bytes());
    if input.len() != PATHS_MOUNT_INPUT_LEN {
        return Err(AnchorError::Corrupt(
            "paths-and-device digest input is not 181 bytes".to_string(),
        ));
    }
    Ok(sha256(&input))
}

fn read_row(db: &redb::Database) -> Result<AnchorRow, AnchorError> {
    let read = db.begin_read().map_err(database_error)?;
    let table =
        read.open_table(ANCHOR_TABLE).map_err(|error| AnchorError::Corrupt(error.to_string()))?;
    if table.len().map_err(database_error)? != 1 {
        return Err(AnchorError::Corrupt(
            "anchor table does not contain exactly one row".to_string(),
        ));
    }
    let bytes = table
        .get(ANCHOR_KEY)
        .map_err(database_error)?
        .ok_or_else(|| AnchorError::Corrupt("anchor row is absent".to_string()))?
        .value()
        .to_vec();
    AnchorRow::decode(&bytes)
}

fn database_error(error: impl std::fmt::Display) -> AnchorError {
    AnchorError::Database(error.to_string())
}

fn database_open_error(error: redb::DatabaseError) -> AnchorError {
    match error {
        redb::DatabaseError::DatabaseAlreadyOpen => AnchorError::NotSingletonWriter,
        other => AnchorError::Database(other.to_string()),
    }
}

#[cfg(any(test, feature = "p0-provisioning"))]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_uid_is_rejected_by_exact_owner_check() {
        assert!(matches!(
            validate_unix_owner_mode(
                1000,
                1001,
                0o600,
                0o600,
                "anchor leaf owner is not the process UID",
                "anchor leaf mode is not 0600",
            ),
            Err(AnchorError::Path(_))
        ));
    }
}
