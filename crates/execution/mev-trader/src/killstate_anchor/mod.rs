//! External monotonic anchor evidence, storage, and the sole three-store transition owner.
//!
//! Production opens an existing initialized redb leaf only. Fresh-leaf creation and
//! activation are compiled solely for tests or the non-default `p0-provisioning` feature.

use std::{
    fmt::Write as FmtWrite,
    fs::{File, OpenOptions},
    io::Write as IoWrite,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{KillReason, KillState, KillStateStore, KillStoreError, ResetAttestation};

mod external_store;
#[cfg(any(test, feature = "p0-provisioning"))]
pub use external_store::AnchorProvisioner;
use external_store::ExternalAnchorStore;
pub use external_store::{ANCHOR_DB, ANCHOR_DIR, EXPECTED_ANCHOR_IDENTITY, KILLSTATE_DIR};
pub use external_store::{PATHS_MOUNT_DOMAIN, SEED_AUTH_DOMAIN};

mod types;
pub use types::{AnchorError, AnchorStoreIdentity, Rollback};
#[cfg(any(test, feature = "p0-provisioning"))]
pub use types::{BootstrapEvidence, SeedAuthorization};

const STATE_FILE: &str = "state.json";
const LOCAL_HWM_FILE: &str = "epoch.hwm";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitTarget {
    Local,
    Engaged,
    Clear,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitPhase {
    BeforeTempWrite,
    AfterTempWrite,
    AfterTempFsync,
    BeforeRename,
    AfterRename,
}

#[cfg(test)]
thread_local! {
    static COMMIT_FAULT: std::cell::Cell<Option<(CommitTarget, CommitPhase)>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn arm_commit_fault(target: CommitTarget, phase: CommitPhase) {
    COMMIT_FAULT.with(|fault| fault.set(Some((target, phase))));
}

#[cfg(test)]
fn take_commit_fault(target: CommitTarget, phase: CommitPhase) -> bool {
    COMMIT_FAULT.with(|fault| {
        if fault.get() == Some((target, phase)) {
            fault.set(None);
            true
        } else {
            false
        }
    })
}

/// Failure to construct the pinned production three-store owner.
#[derive(Debug, Error)]
pub enum StartupError {
    /// P0-C has not compile-pinned the reviewed bootstrap identity.
    #[error("external anchor identity is not compile-pinned")]
    AnchorIdentityUnpinned,
    /// Initial continuous durable observation was not verified Clear.
    #[error("anchored kill state is not clear at startup")]
    KillStateNotClear,
    /// The pinned external store could not be admitted or verified.
    #[error("external anchor startup failed: {0}")]
    Anchor(#[from] AnchorError),
}

/// Sole production owner of kill-state load, engage, and owner-reset sequencing.
#[derive(Debug)]
pub struct AnchoredKillStateStore {
    kill_dir: PathBuf,
    external: ExternalAnchorStore,
    transition: Arc<Mutex<()>>,
}

impl AnchoredKillStateStore {
    fn from_opened(kill_dir: PathBuf, external: ExternalAnchorStore) -> Self {
        Self { kill_dir, external, transition: Arc::new(Mutex::new(())) }
    }

    #[cfg(test)]
    fn open_for_test(
        identity: AnchorStoreIdentity,
        kill_dir: &Path,
        anchor_dir: &Path,
        db_path: &Path,
    ) -> Result<Self, AnchorError> {
        let external =
            ExternalAnchorStore::open_existing_for_test(identity, kill_dir, anchor_dir, db_path)?;
        Ok(Self::from_opened(kill_dir.to_path_buf(), external))
    }

    fn record_path(&self) -> PathBuf {
        self.kill_dir.join(STATE_FILE)
    }

    fn hwm_path(&self) -> PathBuf {
        self.kill_dir.join(LOCAL_HWM_FILE)
    }

    fn read_record(&self) -> Option<PersistedRecord> {
        serde_json::from_slice(&std::fs::read(self.record_path()).ok()?).ok()
    }

    fn read_local_hwm(&self) -> Option<u64> {
        let hwm: HighWaterMark =
            serde_json::from_slice(&std::fs::read(self.hwm_path()).ok()?).ok()?;
        Some(hwm.high_water_epoch)
    }

    fn stage_temp(
        &self,
        path: &Path,
        bytes: &[u8],
        target: CommitTarget,
    ) -> Result<PathBuf, KillStoreError> {
        #[cfg(not(test))]
        let _ = target;
        let temp = temporary_sibling(path);
        let result = (|| -> std::io::Result<()> {
            #[cfg(test)]
            if take_commit_fault(target, CommitPhase::BeforeTempWrite) {
                return Err(std::io::Error::other("test fault before temp write"));
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
            file.write_all(bytes)?;
            #[cfg(test)]
            if take_commit_fault(target, CommitPhase::AfterTempWrite) {
                return Err(std::io::Error::other("test fault after temp write"));
            }
            file.sync_all()?;
            #[cfg(test)]
            if take_commit_fault(target, CommitPhase::AfterTempFsync) {
                return Err(std::io::Error::other("test fault after temp fsync"));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        Ok(temp)
    }

    fn commit_durable(
        &self,
        path: &Path,
        bytes: &[u8],
        target: CommitTarget,
    ) -> Result<(), KillStoreError> {
        let directory = File::open(&self.kill_dir).map_err(|_| KillStoreError::Io)?;
        let temp = self.stage_temp(path, bytes, target)?;
        #[cfg(test)]
        if take_commit_fault(target, CommitPhase::BeforeRename) {
            let _ = std::fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        if std::fs::rename(&temp, path).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        #[cfg(test)]
        if take_commit_fault(target, CommitPhase::AfterRename) {
            return Err(KillStoreError::Io);
        }
        directory.sync_all().map_err(|_| KillStoreError::Io)
    }

    fn commit_local_hwm(&self, epoch: u64) -> Result<(), KillStoreError> {
        let bytes = serde_json::to_vec(&HighWaterMark { high_water_epoch: epoch })
            .map_err(|_| KillStoreError::Io)?;
        self.commit_durable(&self.hwm_path(), &bytes, CommitTarget::Local)
    }

    fn commit_engaged(&self, epoch: u64, reason: KillReason) -> Result<(), KillStoreError> {
        let bytes = serde_json::to_vec(&PersistedRecord::Engaged { epoch, reason })
            .map_err(|_| KillStoreError::Io)?;
        self.commit_durable(&self.record_path(), &bytes, CommitTarget::Engaged)
    }

    fn commit_clear(
        &self,
        epoch: u64,
        attestation: &ResetAttestation,
    ) -> Result<(), KillStoreError> {
        let reset = PersistedReset::from(attestation);
        let bytes = serde_json::to_vec(&PersistedRecord::Clear { epoch, reset })
            .map_err(|_| KillStoreError::Io)?;
        let record = self.record_path();
        let temp = self.stage_temp(&record, &bytes, CommitTarget::Clear)?;
        #[cfg(test)]
        if take_commit_fault(CommitTarget::Clear, CommitPhase::BeforeRename) {
            let _ = std::fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        if std::fs::rename(&temp, &record).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        #[cfg(test)]
        if take_commit_fault(CommitTarget::Clear, CommitPhase::AfterRename) {
            // Model a directory open/sync failure after the Clear commit point.
            return Ok(());
        }
        // Clear is visible only after rename. Failure to persist that rename can only restore the
        // prior Engaged record after a crash, so post-rename directory fsync is best effort.
        if let Ok(directory) = File::open(&self.kill_dir) {
            let _ = directory.sync_all();
        }
        Ok(())
    }
}

impl KillStateStore for AnchoredKillStateStore {
    fn load(&self) -> KillState {
        let Ok(_guard) = self.transition.lock() else { return KillState::Unknown };
        let Some(record) = self.read_record() else { return KillState::Unknown };
        let Some(local) = self.read_local_hwm() else { return KillState::Unknown };
        let Ok(external) = self.external.read_hwm() else { return KillState::Unknown };
        let epoch = record.epoch();
        if epoch != local || local != external {
            return KillState::Unknown;
        }
        match record {
            PersistedRecord::Engaged { reason, .. } => KillState::Engaged { reason },
            PersistedRecord::Clear { reset, .. } => {
                let Ok(signature) = decode_signature(&reset.signature_hex) else {
                    return KillState::Unknown;
                };
                let attestation = ResetAttestation {
                    engagement_epoch: reset.engagement_epoch,
                    nonce: reset.nonce,
                    signature,
                };
                if attestation.verify_only(epoch).is_ok() {
                    KillState::Clear { verified_at: epoch }
                } else {
                    KillState::Unknown
                }
            }
        }
    }

    fn engage(&self, reason: KillReason) -> Result<(), KillStoreError> {
        let _guard = self.transition.lock().map_err(|_| KillStoreError::Io)?;
        let record_epoch = self.read_record().ok_or(KillStoreError::EpochAnchorInvalid)?.epoch();
        let local = self.read_local_hwm().ok_or(KillStoreError::EpochAnchorInvalid)?;
        let external = self.external.read_hwm().map_err(|_| KillStoreError::EpochAnchorInvalid)?;
        let next = record_epoch
            .max(local)
            .max(external)
            .checked_add(1)
            .ok_or(KillStoreError::EpochExhausted)?;

        // The only production transition order: external Immediate commit, durable local rename,
        // then durable Engaged record rename. Every prefix is fail-closed on restart.
        self.external.observe(next).map_err(|_| KillStoreError::EpochAnchorInvalid)?;
        self.commit_local_hwm(next)?;
        self.commit_engaged(next, reason)
    }

    fn owner_reset(&self, attestation: &ResetAttestation) -> Result<(), KillStoreError> {
        let _guard = self.transition.lock().map_err(|_| KillStoreError::Io)?;
        let Some(PersistedRecord::Engaged { epoch, .. }) = self.read_record() else {
            return Err(KillStoreError::NotEngaged);
        };
        let local = self.read_local_hwm().ok_or(KillStoreError::EpochAnchorInvalid)?;
        let external = self.external.read_hwm().map_err(|_| KillStoreError::EpochAnchorInvalid)?;
        if epoch != local || local != external {
            return Err(KillStoreError::EpochAnchorInvalid);
        }
        attestation.verify_only(epoch)?;
        self.commit_clear(epoch, attestation)
    }
}

/// Opens the sole pinned production kill-state owner.
///
/// P0-A intentionally refuses before touching the filesystem while the reviewed anchor identity
/// remains unpinned. Once P0-C pins it, this path performs open-existing-only admission.
pub fn open_anchored_killstate() -> Result<AnchoredKillStateStore, StartupError> {
    let expected = EXPECTED_ANCHOR_IDENTITY.ok_or(StartupError::AnchorIdentityUnpinned)?;
    let external = ExternalAnchorStore::open_existing(expected)?;
    Ok(AnchoredKillStateStore::from_opened(PathBuf::from(KILLSTATE_DIR), external))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PersistedRecord {
    Engaged { epoch: u64, reason: KillReason },
    Clear { epoch: u64, reset: PersistedReset },
}

impl PersistedRecord {
    fn epoch(&self) -> u64 {
        match self {
            Self::Engaged { epoch, .. } | Self::Clear { epoch, .. } => *epoch,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedReset {
    engagement_epoch: u64,
    nonce: u64,
    signature_hex: String,
}

impl From<&ResetAttestation> for PersistedReset {
    fn from(attestation: &ResetAttestation) -> Self {
        let mut signature_hex = String::with_capacity(130);
        for byte in attestation.signature {
            write!(&mut signature_hex, "{byte:02x}").expect("writing to String cannot fail");
        }
        debug_assert_eq!(signature_hex.len(), 130);
        Self {
            engagement_epoch: attestation.engagement_epoch,
            nonce: attestation.nonce,
            signature_hex,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HighWaterMark {
    high_water_epoch: u64,
}

fn decode_signature(hex: &str) -> Result<[u8; 65], ()> {
    if hex.len() != 130 {
        return Err(());
    }
    let mut signature = [0u8; 65];
    for (index, byte) in signature.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(signature)
}

fn temporary_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp.{}.{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    const RESET_SIGNATURE: &str = "c4e4fec07c5dc52eddee17d3c5c4ba6d0801be20fb8c688bf016df0e95d480df6679248e725806ecff4e7636921f60cdbe75bc1d69ab44d2cb15f947c8bfcf201b";

    struct Fixture {
        root: PathBuf,
        kill: PathBuf,
        anchor: PathBuf,
        db: PathBuf,
        identity: AnchorStoreIdentity,
    }

    impl Fixture {
        fn new(epoch: u64) -> Self {
            let root = std::env::temp_dir().join(format!(
                "p0a-transition-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos()
            ));
            let kill = root.join("kill");
            let anchor = root.join("anchor");
            fs::create_dir_all(&kill).expect("kill dir");
            fs::create_dir_all(&anchor).expect("anchor dir");
            fs::set_permissions(&kill, fs::Permissions::from_mode(0o700)).expect("kill mode");
            fs::set_permissions(&anchor, fs::Permissions::from_mode(0o700)).expect("anchor mode");
            let db = anchor.join("epoch-anchor.redb");
            Self::write_engaged_at(&kill, epoch);
            let paths_mount_digest =
                AnchorProvisioner::current_paths_mount_digest_for_test(&kill, &anchor, &db)
                    .expect("paths digest");
            let record_digest =
                AnchorProvisioner::current_record_digest_for_test(&kill, &anchor, &db)
                    .expect("record digest");
            let authorization =
                SeedAuthorization { expected_epoch: epoch, record_digest, paths_mount_digest };
            let evidence =
                AnchorProvisioner::bootstrap_for_test(authorization, &kill, &anchor, &db)
                    .expect("bootstrap");
            let identity = evidence.identity;
            AnchorProvisioner::activate_for_test(authorization, evidence, &kill, &anchor, &db)
                .expect("activate");
            Self { root, kill, anchor, db, identity }
        }

        fn open(&self) -> AnchoredKillStateStore {
            AnchoredKillStateStore::open_for_test(self.identity, &self.kill, &self.anchor, &self.db)
                .expect("open")
        }

        fn write_engaged_at(kill: &Path, epoch: u64) {
            fs::write(
                kill.join(STATE_FILE),
                serde_json::to_vec(&PersistedRecord::Engaged {
                    epoch,
                    reason: KillReason::DrawdownFloorBreach,
                })
                .expect("record"),
            )
            .expect("write record");
            fs::write(
                kill.join(LOCAL_HWM_FILE),
                serde_json::to_vec(&HighWaterMark { high_water_epoch: epoch }).expect("hwm"),
            )
            .expect("write hwm");
        }
    }

    struct ProvisionFixture {
        root: PathBuf,
        kill: PathBuf,
        anchor: PathBuf,
        db: PathBuf,
        authorization: SeedAuthorization,
    }

    impl ProvisionFixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "p0a-provision-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos()
            ));
            let kill = root.join("kill");
            let anchor = root.join("anchor");
            fs::create_dir_all(&kill).expect("kill dir");
            fs::create_dir_all(&anchor).expect("anchor dir");
            fs::set_permissions(&kill, fs::Permissions::from_mode(0o700)).expect("kill mode");
            fs::set_permissions(&anchor, fs::Permissions::from_mode(0o700)).expect("anchor mode");
            let db = anchor.join("epoch-anchor.redb");
            Fixture::write_engaged_at(&kill, 1);
            let paths_mount_digest =
                AnchorProvisioner::current_paths_mount_digest_for_test(&kill, &anchor, &db)
                    .expect("paths digest");
            let record_digest =
                AnchorProvisioner::current_record_digest_for_test(&kill, &anchor, &db)
                    .expect("record digest");
            let authorization =
                SeedAuthorization { expected_epoch: 1, record_digest, paths_mount_digest };
            Self { root, kill, anchor, db, authorization }
        }

        fn bootstrap(&self) -> BootstrapEvidence {
            AnchorProvisioner::bootstrap_for_test(
                self.authorization,
                &self.kill,
                &self.anchor,
                &self.db,
            )
            .expect("bootstrap")
        }
    }

    impl Drop for ProvisionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn reset(epoch: u64) -> ResetAttestation {
        let signature = decode_signature(RESET_SIGNATURE).expect("signature");
        ResetAttestation { engagement_epoch: epoch, nonce: 424242, signature }
    }
    fn assert_local_fault(phase: CommitPhase) {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Local, phase);
        assert!(matches!(store.engage(KillReason::KeyOrSignatureFailure), Err(KillStoreError::Io)));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(1));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert_eq!(store.load(), KillState::Unknown);
    }

    fn assert_record_fault(phase: CommitPhase) {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Engaged, phase);
        assert!(matches!(store.engage(KillReason::KeyOrSignatureFailure), Err(KillStoreError::Io)));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(2));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert_eq!(store.load(), KillState::Unknown);
    }

    fn assert_clear_fault(phase: CommitPhase) {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Clear, phase);
        assert!(matches!(store.owner_reset(&reset(1)), Err(KillStoreError::Io)));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
    }

    #[test]
    fn bootstrap_existing_uninitialized_and_identity_mismatch_are_rejected() {
        let fixture = ProvisionFixture::new("lifecycle");
        let evidence = fixture.bootstrap();
        assert!(matches!(
            AnchorProvisioner::bootstrap_for_test(
                fixture.authorization,
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::AlreadyExists)
        ));
        assert!(matches!(
            AnchoredKillStateStore::open_for_test(
                evidence.identity,
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::Uninitialized)
        ));
        AnchorProvisioner::activate_for_test(
            fixture.authorization,
            evidence,
            &fixture.kill,
            &fixture.anchor,
            &fixture.db,
        )
        .expect("activate");
        assert!(matches!(
            AnchoredKillStateStore::open_for_test(
                AnchorStoreIdentity::new([0x55; 32]),
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::IdentityMismatch)
        ));
    }

    #[test]
    fn stale_activation_and_evidence_mix_match_are_rejected() {
        let stale = ProvisionFixture::new("stale");
        let stale_evidence = stale.bootstrap();
        Fixture::write_engaged_at(&stale.kill, 2);
        assert!(matches!(
            AnchorProvisioner::activate_for_test(
                stale.authorization,
                stale_evidence,
                &stale.kill,
                &stale.anchor,
                &stale.db,
            ),
            Err(AnchorError::EvidenceMismatch(_))
        ));

        let left = ProvisionFixture::new("mix-left");
        let right = ProvisionFixture::new("mix-right");
        let left_evidence = left.bootstrap();
        let right_evidence = right.bootstrap();
        assert!(matches!(
            AnchorProvisioner::activate_for_test(
                left.authorization,
                right_evidence,
                &left.kill,
                &left.anchor,
                &left.db,
            ),
            Err(AnchorError::EvidenceMismatch(_) | AnchorError::IdentityMismatch)
        ));
        assert!(matches!(
            AnchorProvisioner::activate_for_test(
                right.authorization,
                left_evidence,
                &right.kill,
                &right.anchor,
                &right.db,
            ),
            Err(AnchorError::EvidenceMismatch(_) | AnchorError::IdentityMismatch)
        ));
    }

    #[test]
    fn symlink_wrong_mode_and_path_substitution_are_rejected() {
        let fixture = ProvisionFixture::new("paths");
        let substituted = fixture.anchor.join("replacement.redb");
        assert!(matches!(
            AnchorProvisioner::current_paths_mount_digest_for_test(
                &fixture.kill,
                &fixture.anchor,
                &substituted,
            ),
            Err(AnchorError::Path(_))
        ));
        fs::set_permissions(&fixture.anchor, fs::Permissions::from_mode(0o755))
            .expect("wrong mode");
        assert!(matches!(
            AnchorProvisioner::current_paths_mount_digest_for_test(
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::Path(_))
        ));
        fs::set_permissions(&fixture.anchor, fs::Permissions::from_mode(0o700))
            .expect("restore mode");
        let symlink = fixture.root.join("anchor-link");
        std::os::unix::fs::symlink(&fixture.anchor, &symlink).expect("symlink");
        let symlink_db = symlink.join("epoch-anchor.redb");
        assert!(matches!(
            AnchorProvisioner::current_paths_mount_digest_for_test(
                &fixture.kill,
                &symlink,
                &symlink_db,
            ),
            Err(AnchorError::Path(_))
        ));
    }
    #[test]
    fn anchored_engage_commits_external_then_local_then_record() {
        let fixture = Fixture::new(0);
        let store = fixture.open();
        store.engage(KillReason::KeyOrSignatureFailure).expect("engage");
        assert_eq!(store.external.read_hwm().expect("external"), 1);
        assert_eq!(store.read_local_hwm(), Some(1));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
    }

    #[test]
    fn stale_clear_direct_restore_and_single_file_rollbacks_are_unknown() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        let stale = PersistedRecord::Clear {
            epoch: 1,
            reset: PersistedReset {
                engagement_epoch: 1,
                nonce: 424242,
                signature_hex: RESET_SIGNATURE.to_owned(),
            },
        };
        store.external.observe(2).expect("advance external");
        fs::write(store.record_path(), serde_json::to_vec(&stale).expect("stale")).expect("write");
        assert_eq!(store.load(), KillState::Unknown);
        fs::write(store.hwm_path(), br#"{"high_water_epoch":2}"#).expect("local ahead");
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn external_ahead_and_behind_are_independent_unknown_branches() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.external.observe(2).expect("external ahead");
        assert_eq!(store.load(), KillState::Unknown);
        Fixture::write_engaged_at(&fixture.kill, 3);
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn epoch_max_in_record_local_or_external_changes_nothing() {
        for branch in ["record", "local", "external"] {
            let fixture = Fixture::new(1);
            let store = fixture.open();
            match branch {
                "record" => fs::write(
                    store.record_path(),
                    serde_json::to_vec(&PersistedRecord::Engaged {
                        epoch: u64::MAX,
                        reason: KillReason::DrawdownFloorBreach,
                    })
                    .expect("record"),
                )
                .expect("write record"),
                "local" => fs::write(
                    store.hwm_path(),
                    serde_json::to_vec(&HighWaterMark { high_water_epoch: u64::MAX }).expect("hwm"),
                )
                .expect("write hwm"),
                "external" => store.external.observe(u64::MAX).expect("external max"),
                _ => unreachable!(),
            }
            let before_record = fs::read(store.record_path()).expect("record before");
            let before_local = fs::read(store.hwm_path()).expect("local before");
            let before_external = store.external.read_hwm().expect("external before");
            assert!(matches!(
                store.engage(KillReason::KeyOrSignatureFailure),
                Err(KillStoreError::EpochExhausted)
            ));
            assert_eq!(fs::read(store.record_path()).expect("record after"), before_record);
            assert_eq!(fs::read(store.hwm_path()).expect("local after"), before_local);
            assert_eq!(store.external.read_hwm().expect("external after"), before_external);
        }
    }

    #[test]
    fn reset_rechecks_three_stores_before_verify_only_clear() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.external.observe(2).expect("ahead");
        assert!(matches!(store.owner_reset(&reset(1)), Err(KillStoreError::EpochAnchorInvalid)));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { .. })));
    }

    #[test]
    fn pre_external_commit_fault_is_operation_not_committed_old_old_old() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        external_store::arm_observe_fault(external_store::ObserveFault::BeforeCommit);
        assert!(matches!(
            store.engage(KillReason::KeyOrSignatureFailure),
            Err(KillStoreError::EpochAnchorInvalid)
        ));
        assert_eq!(store.external.read_hwm().expect("external"), 1);
        assert_eq!(store.read_local_hwm(), Some(1));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
    }

    #[test]
    fn post_external_commit_fault_is_new_old_old_unknown() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        external_store::arm_observe_fault(external_store::ObserveFault::AfterCommit);
        assert!(matches!(
            store.engage(KillReason::KeyOrSignatureFailure),
            Err(KillStoreError::EpochAnchorInvalid)
        ));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(1));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn local_temp_create_fault_leaves_new_old_old_unknown() {
        assert_local_fault(CommitPhase::BeforeTempWrite);
    }

    #[test]
    fn local_temp_write_fault_leaves_new_old_old_unknown() {
        assert_local_fault(CommitPhase::AfterTempWrite);
    }

    #[test]
    fn local_temp_fsync_fault_leaves_new_old_old_unknown() {
        assert_local_fault(CommitPhase::AfterTempFsync);
    }

    #[test]
    fn local_rename_fault_leaves_new_old_old_unknown() {
        assert_local_fault(CommitPhase::BeforeRename);
    }

    #[test]
    fn record_temp_create_fault_leaves_new_new_old_unknown() {
        assert_record_fault(CommitPhase::BeforeTempWrite);
    }

    #[test]
    fn record_temp_write_fault_leaves_new_new_old_unknown() {
        assert_record_fault(CommitPhase::AfterTempWrite);
    }

    #[test]
    fn record_temp_fsync_fault_leaves_new_new_old_unknown() {
        assert_record_fault(CommitPhase::AfterTempFsync);
    }

    #[test]
    fn record_rename_fault_leaves_new_new_old_unknown() {
        assert_record_fault(CommitPhase::BeforeRename);
    }
    #[test]
    fn record_pre_rename_fault_with_old_clear_is_new_new_old_clear_unknown() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.owner_reset(&reset(1)).expect("establish old Clear");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });
        arm_commit_fault(CommitTarget::Engaged, CommitPhase::BeforeRename);
        assert!(matches!(store.engage(KillReason::KeyOrSignatureFailure), Err(KillStoreError::Io)));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(2));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Clear { epoch: 1, .. })));
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn clear_temp_create_fault_remains_engaged() {
        assert_clear_fault(CommitPhase::BeforeTempWrite);
    }

    #[test]
    fn clear_temp_write_fault_remains_engaged() {
        assert_clear_fault(CommitPhase::AfterTempWrite);
    }

    #[test]
    fn clear_temp_fsync_fault_remains_engaged() {
        assert_clear_fault(CommitPhase::AfterTempFsync);
    }

    #[test]
    fn clear_rename_fault_remains_engaged() {
        assert_clear_fault(CommitPhase::BeforeRename);
    }

    #[test]
    fn post_external_and_post_local_cutpoints_are_unknown() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.external.observe(2).expect("external commit");
        assert_eq!(store.load(), KillState::Unknown, "new/old/old");
        store.commit_local_hwm(2).expect("local commit");
        assert_eq!(store.load(), KillState::Unknown, "new/new/old");
        store.commit_engaged(2, KillReason::KeyOrSignatureFailure).expect("record commit");
        assert!(matches!(store.load(), KillState::Engaged { .. }), "new/new/Engaged");
    }

    #[test]
    fn clear_post_rename_round_trip_is_authorized_clear_with_130_hex_chars() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        assert!(matches!(store.load(), KillState::Engaged { .. }));
        store.owner_reset(&reset(1)).expect("reset");
        let PersistedRecord::Clear { reset, .. } = store.read_record().expect("clear record")
        else {
            panic!("expected persisted Clear")
        };
        assert_eq!(reset.signature_hex.len(), 130);
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });
    }

    #[test]
    fn local_post_rename_fault_is_new_new_old_unknown() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Local, CommitPhase::AfterRename);
        assert!(matches!(store.engage(KillReason::KeyOrSignatureFailure), Err(KillStoreError::Io)));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(2));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 1, .. })));
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn record_post_rename_fault_is_new_new_engaged() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Engaged, CommitPhase::AfterRename);
        assert!(matches!(store.engage(KillReason::KeyOrSignatureFailure), Err(KillStoreError::Io)));
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(2));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
    }

    #[test]
    fn clear_post_rename_directory_sync_fault_succeeds_with_authorized_clear() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        arm_commit_fault(CommitTarget::Clear, CommitPhase::AfterRename);
        store.owner_reset(&reset(1)).expect("post-rename reset succeeds");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });
        // A real crash before the best-effort directory sync may conservatively restore Engaged.
    }

    #[test]
    fn concurrent_engages_share_one_transition_mutex() {
        const THREADS: usize = 4;
        let fixture = Fixture::new(1);
        let store = Arc::new(fixture.open());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let mut threads = Vec::new();
        for _ in 0..THREADS {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.engage(KillReason::KeyOrSignatureFailure)
            }));
        }
        for thread in threads {
            thread.join().expect("thread").expect("engage");
        }
        assert_eq!(store.external.read_hwm().expect("external"), 1 + THREADS as u64);
        assert_eq!(store.read_local_hwm(), Some(1 + THREADS as u64));
        assert!(matches!(
            store.read_record(),
            Some(PersistedRecord::Engaged { epoch, .. }) if epoch == 1 + THREADS as u64
        ));
    }

    #[test]
    fn concurrent_engage_vs_reset_serializes_to_engaged_epoch_two() {
        let fixture = Fixture::new(1);
        let store = Arc::new(fixture.open());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let engage_store = Arc::clone(&store);
        let engage_barrier = Arc::clone(&barrier);
        let engage = std::thread::spawn(move || {
            engage_barrier.wait();
            engage_store.engage(KillReason::KeyOrSignatureFailure)
        });
        let reset_store = Arc::clone(&store);
        let reset_barrier = Arc::clone(&barrier);
        let owner_reset = std::thread::spawn(move || {
            reset_barrier.wait();
            reset_store.owner_reset(&reset(1))
        });
        engage.join().expect("engage thread").expect("engage");
        let reset_result = owner_reset.join().expect("reset thread");
        assert!(
            reset_result.is_ok() || matches!(reset_result, Err(KillStoreError::ResetEpochMismatch))
        );
        assert_eq!(store.external.read_hwm().expect("external"), 2);
        assert_eq!(store.read_local_hwm(), Some(2));
        assert!(matches!(store.read_record(), Some(PersistedRecord::Engaged { epoch: 2, .. })));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
    }

    #[test]
    fn second_opener_is_rejected() {
        let fixture = Fixture::new(1);
        let _first = fixture.open();
        assert!(matches!(
            AnchoredKillStateStore::open_for_test(
                fixture.identity,
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::NotSingletonWriter)
        ));
    }
    #[test]
    fn hardlink_alias_second_opener_is_rejected_by_inode_lock() {
        let fixture = Fixture::new(1);
        let first = fixture.open();
        let alias_anchor = fixture.root.join("anchor-alias");
        fs::create_dir(&alias_anchor).expect("alias anchor dir");
        fs::set_permissions(&alias_anchor, fs::Permissions::from_mode(0o700))
            .expect("alias anchor mode");
        let alias_db = alias_anchor.join("epoch-anchor.redb");
        fs::hard_link(&fixture.db, &alias_db).expect("hardlink alias");
        assert!(matches!(
            AnchoredKillStateStore::open_for_test(
                fixture.identity,
                &fixture.kill,
                &alias_anchor,
                &alias_db,
            ),
            Err(AnchorError::NotSingletonWriter)
        ));
        drop(first);
    }

    #[test]
    fn open_clear_store_becomes_unknown_when_anchor_leaf_is_unlinked() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.owner_reset(&reset(1)).expect("establish Clear");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });

        fs::remove_file(&fixture.db).expect("unlink admitted anchor");
        assert_eq!(store.load(), KillState::Unknown);
        assert!(!fixture.db.exists(), "live validation must not recreate the anchor");
    }

    #[test]
    fn open_clear_store_becomes_unknown_when_anchor_leaf_is_replaced() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.owner_reset(&reset(1)).expect("establish Clear");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });
        let admitted = fs::symlink_metadata(&fixture.db).expect("admitted metadata");

        fs::remove_file(&fixture.db).expect("unlink admitted anchor");
        fs::write(&fixture.db, b"replacement inode").expect("write replacement");
        fs::set_permissions(&fixture.db, fs::Permissions::from_mode(0o600))
            .expect("replacement mode");
        let replacement = fs::symlink_metadata(&fixture.db).expect("replacement metadata");
        assert_ne!(
            (admitted.dev(), admitted.ino()),
            (replacement.dev(), replacement.ino()),
            "replacement must use another inode"
        );
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn open_clear_store_becomes_unknown_when_anchor_leaf_is_truncated_empty() {
        let fixture = Fixture::new(1);
        let store = fixture.open();
        store.owner_reset(&reset(1)).expect("establish Clear");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });

        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&fixture.db)
            .expect("truncate admitted anchor");
        assert_eq!(fs::metadata(&fixture.db).expect("truncated metadata").len(), 0);
        assert_eq!(store.load(), KillState::Unknown);
    }

    #[test]
    fn missing_and_corrupt_external_reopen_fail_closed_without_recreation() {
        let fixture = Fixture::new(1);
        fs::remove_file(&fixture.db).expect("remove external");
        assert!(matches!(
            AnchoredKillStateStore::open_for_test(
                fixture.identity,
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            ),
            Err(AnchorError::Missing)
        ));
        assert!(!fixture.db.exists(), "open-existing must not recreate a missing leaf");
        fs::write(&fixture.db, b"not redb").expect("corrupt leaf");
        fs::set_permissions(&fixture.db, fs::Permissions::from_mode(0o600)).expect("leaf mode");
        assert!(
            AnchoredKillStateStore::open_for_test(
                fixture.identity,
                &fixture.kill,
                &fixture.anchor,
                &fixture.db,
            )
            .is_err()
        );
    }

    #[test]
    fn production_factory_has_owner_reviewed_p0c_identity_pin() {
        assert_eq!(
            EXPECTED_ANCHOR_IDENTITY.map(|identity| *identity.as_bytes()),
            Some([
                0x95, 0x0a, 0xa8, 0x75, 0x18, 0x75, 0x7d, 0x3d, 0x85, 0x05, 0x37, 0x6b, 0xf2, 0xe0,
                0x18, 0xdf, 0x9b, 0x85, 0xd2, 0xe8, 0xc9, 0x1d, 0x64, 0xa1, 0x4b, 0x66, 0x42, 0x83,
                0x42, 0x1a, 0xec, 0x9f,
            ])
        );
    }
}
