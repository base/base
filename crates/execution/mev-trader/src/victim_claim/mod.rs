//! R9 victim at-most-once claim store (Stage 0, keyless, red-line-safe).
//!
//! A durable, node-local redb store that guarantees a given victim transaction
//! is claimed **at most once, globally** across restarts and concurrent
//! attempts. It is the proof source for downstream backrun arming: a
//! [`VictimClaim`] is minted only on a successful durable commit and cannot be
//! forged. This module is purely a local claim ledger — there is **no key
//! material, transaction egress, network, or funds** surface anywhere in it
//! (enforced by a keyless-token test).
//!
//! # Singleton writer
//!
//! redb takes an exclusive advisory `flock(LOCK_EX)` on the database file's
//! **inode** for the lifetime of an open [`redb::Database`]
//! (`redb::backends::FileBackend`). Because `flock` conflicts are keyed on the
//! inode, this refuses a second opener across processes **and across hardlink
//! aliases** of the same file (a path-named sidecar lock would not). We surface
//! that as [`ClaimStoreError::NotSingletonWriter`] fail-closed, and additionally
//! require a non-symlinked, private (0700) containing directory so a hostile
//! alias cannot be planted next to the store in the first place.
//!
//! # Store identity
//!
//! The store writes a one-time CSPRNG `creation_nonce` at bootstrap
//! ([`StoreIdentity`]). [`VictimClaimStore::open_existing`] refuses to open a
//! store whose nonce does not match the expected (owner-attested) identity,
//! and never auto-recreates a lost or empty store — this distinguishes an
//! attested zero-claim store from a fresh empty one.
//!
//! # Rollback boundary
//!
//! Path + nonce detect concurrent writers and torn writes, **not** a restore of
//! a normal past snapshot. For the single-node/single-operator P2 scope,
//! snapshot/backup-restore/volume-rollback of the claim store is an operational
//! red-line (owner-approved), not a guarantee enforced here. HA / multi-instance
//! expansion would require an external synchronous monotonic co-anchor.

mod store;
mod types;

use std::path::{Path, PathBuf};

use alloy_primitives::B256;

pub use types::{CampaignId, ClaimResult, ClaimStoreError, StoreIdentity, VictimClaim};

/// Configuration for a [`VictimClaimStore`].
#[derive(Debug, Clone)]
pub struct VictimClaimConfig {
    /// Absolute path of the dedicated redb claim database file (kept separate
    /// from the canonical node MDBX store). Its containing directory must be a
    /// non-symlinked, private (0700) directory; the singleton-writer lock is
    /// taken on this file's inode.
    pub db_path: PathBuf,
}

/// A long-lived handle to the durable victim claim store.
///
/// Holds the redb database — whose open [`redb::Database`] keeps an exclusive
/// `flock(LOCK_EX)` on the file's inode for its lifetime (the singleton-writer
/// lock) — and the store identity. Only a handle obtained via
/// [`bootstrap`](Self::bootstrap) or [`open_existing`](Self::open_existing) can
/// mint a [`VictimClaim`] — and only on the commit-succeeded path.
#[derive(Debug)]
pub struct VictimClaimStore {
    db: redb::Database,
    identity: StoreIdentity,
}

impl VictimClaimStore {
    /// Provisions (creates if absent) the claim store and returns a handle.
    ///
    /// **Provisioning-only surface**: gated behind the `r9-provisioning` cargo
    /// feature (plus `cfg(test)` so the test suite can build fixtures) and
    /// therefore **not compiled into the default live-node build** — a running
    /// node cannot mint a fresh store. It creates the database and, on first
    /// bootstrap, writes a fresh CSPRNG creation nonce; on an existing store it
    /// adopts the existing identity idempotently. Opening the database takes the
    /// inode lock; a concurrent opener is refused
    /// [`ClaimStoreError::NotSingletonWriter`].
    #[cfg(any(test, feature = "r9-provisioning"))]
    pub fn bootstrap(config: &VictimClaimConfig) -> Result<Self, ClaimStoreError> {
        if let Some(parent) = config.db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        }
        verify_store_path(&config.db_path)?;
        let db = redb::Database::create(&config.db_path).map_err(map_open_error)?;
        let identity = store::bootstrap_metadata(&db)?;
        Ok(Self { db, identity })
    }

    /// Opens an already-provisioned store, verifying its identity.
    ///
    /// Uses `redb::Database::open` only (never creates). Fails
    /// [`ClaimStoreError::StoreIdentityMismatch`] if the store file is missing,
    /// empty, or its creation nonce does not equal `expected` — a lost or
    /// freshly-created store is never silently adopted. Opening the database
    /// takes redb's exclusive inode lock, so a second opener (any process, or
    /// any hardlink alias of the same inode) is refused
    /// [`ClaimStoreError::NotSingletonWriter`].
    pub fn open_existing(
        config: &VictimClaimConfig,
        expected: StoreIdentity,
    ) -> Result<Self, ClaimStoreError> {
        verify_store_path(&config.db_path)?;
        match std::fs::metadata(&config.db_path) {
            // Auto-recreation of a lost or empty store is forbidden once active.
            Err(_) => return Err(ClaimStoreError::StoreIdentityMismatch),
            Ok(meta) if meta.len() == 0 => return Err(ClaimStoreError::StoreIdentityMismatch),
            Ok(_) => {}
        }
        let db = redb::Database::open(&config.db_path).map_err(map_open_error)?;
        store::verify_metadata(&db, expected)?;
        Ok(Self { db, identity: expected })
    }

    /// Attempts to claim `victim_tx_hash` on `chain_id` at most once globally.
    ///
    /// Returns [`ClaimResult::Claimed`] with an unforgeable proof only after a
    /// successful durable commit; [`ClaimResult::AlreadyClaimed`] if the victim
    /// was already claimed by any campaign. Any [`ClaimStoreError`] means no
    /// proof exists and inclusion submission must not proceed.
    pub fn try_claim(
        &self,
        chain_id: u64,
        victim_tx_hash: B256,
        campaign_id: CampaignId,
    ) -> Result<ClaimResult, ClaimStoreError> {
        store::try_claim_txn(&self.db, self.identity, chain_id, victim_tx_hash, campaign_id)
    }

    /// The identity of this store (its creation nonce).
    pub const fn store_identity(&self) -> StoreIdentity {
        self.identity
    }
}

/// Enforces the store path/directory policy (canonical-path integrity):
/// the path must be absolute; no ancestor may be a symlink; the containing
/// directory must be a real directory with private (0700) permissions; and the
/// database file, if it exists, must be a regular file (not a symlink). A 0700
/// containing directory (owner-only) is what makes the inode lock and the
/// symlink checks TOCTOU-safe on a single-operator host.
fn verify_store_path(db_path: &Path) -> Result<(), ClaimStoreError> {
    if !db_path.is_absolute() {
        return Err(ClaimStoreError::Io("claim store path must be absolute".to_string()));
    }
    let parent = db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| ClaimStoreError::Io("claim store path has no parent".to_string()))?;

    // Reject a symlink anywhere in the parent chain.
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            return Err(ClaimStoreError::Io(
                "claim store path contains a symlink component".to_string(),
            ));
        }
    }

    // Containing directory: must be a real directory, and (on unix) private.
    let dir_meta =
        std::fs::symlink_metadata(parent).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    if !dir_meta.file_type().is_dir() {
        return Err(ClaimStoreError::Io("claim store parent is not a directory".to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if dir_meta.permissions().mode() & 0o077 != 0 {
            return Err(ClaimStoreError::Io(
                "claim store directory must be private (0700)".to_string(),
            ));
        }
    }

    // The database file itself, if present, must be a regular file.
    if let Ok(meta) = std::fs::symlink_metadata(db_path)
        && !meta.file_type().is_file()
    {
        return Err(ClaimStoreError::Io("claim store path must be a regular file".to_string()));
    }
    Ok(())
}

/// Maps a redb open/create failure. redb takes an exclusive `flock` on the
/// database inode when opening; a conflict means another writer already holds
/// the store (including via a hardlink alias, since `flock` is inode-keyed), so
/// it maps fail-closed to [`ClaimStoreError::NotSingletonWriter`]. All other
/// failures are I/O.
fn map_open_error(error: redb::DatabaseError) -> ClaimStoreError {
    match error {
        redb::DatabaseError::DatabaseAlreadyOpen => ClaimStoreError::NotSingletonWriter,
        other => ClaimStoreError::Io(other.to_string()),
    }
}

#[cfg(test)]
impl VictimClaimStore {
    /// Assembles a store handle from pre-built parts (commit-failure injection).
    fn from_parts(db: redb::Database, identity: StoreIdentity) -> Self {
        Self { db, identity }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use alloy_primitives::{B256, hex};

    use super::{ClaimResult, ClaimStoreError};
    use super::{VictimClaimConfig, VictimClaimStore, store};
    use crate::victim_claim::CampaignId;

    const CHAIN_ID: u64 = 8453;

    // ---- child-process test protocol ------------------------------------
    const CHILD_ROLE_ENV: &str = "R9_CHILD_ROLE";
    const CHILD_DB_ENV: &str = "R9_CHILD_DB";
    const CHILD_IDENTITY_ENV: &str = "R9_CHILD_IDENTITY";
    const CHILD_VICTIM_ENV: &str = "R9_CHILD_VICTIM";
    const CHILD_TEST_PATH: &str = "victim_claim::tests::r9_child_entrypoint";
    const EXIT_NOT_SINGLETON: i32 = 91;
    const EXIT_UNEXPECTED: i32 = 92;

    fn config_at(dir: &std::path::Path) -> VictimClaimConfig {
        VictimClaimConfig { db_path: dir.join("claims.redb") }
    }

    fn victim(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn campaign(byte: u8) -> CampaignId {
        CampaignId::new([byte; 32])
    }

    /// Unique, self-cleaning temp directory (std-only, mirroring the
    /// `safety.rs` test precedent; no `tempfile` dependency).
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempStore {
        path: std::path::PathBuf,
    }

    impl TempStore {
        fn new() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir()
                .join(format!("r9-victim-claim-{}-{seq}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            // The store requires a private (0700) containing directory.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod 0700");
            }
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn concurrent_attempts_yield_exactly_one_claim() {
        let dir = TempStore::new();
        let store = VictimClaimStore::bootstrap(&config_at(dir.path())).unwrap();
        let target = victim(0xAB);
        let camp = campaign(0x07);

        let threads = 16usize;
        let claimed = AtomicUsize::new(0);
        let already = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| match store.try_claim(CHAIN_ID, target, camp).unwrap() {
                    ClaimResult::Claimed(proof) => {
                        assert_eq!(proof.victim_tx_hash(), target);
                        assert_eq!(proof.chain_id(), CHAIN_ID);
                        assert_eq!(proof.store_identity(), store.store_identity());
                        claimed.fetch_add(1, Ordering::SeqCst);
                    }
                    ClaimResult::AlreadyClaimed => {
                        already.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(claimed.load(Ordering::SeqCst), 1);
        assert_eq!(already.load(Ordering::SeqCst), threads - 1);
    }

    #[test]
    fn claim_persists_across_reopen() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        let target = victim(0x11);

        let identity = {
            let store = VictimClaimStore::bootstrap(&config).unwrap();
            assert!(matches!(
                store.try_claim(CHAIN_ID, target, campaign(0x01)).unwrap(),
                ClaimResult::Claimed(_)
            ));
            store.store_identity()
        };

        let reopened = VictimClaimStore::open_existing(&config, identity).unwrap();
        assert!(matches!(
            reopened.try_claim(CHAIN_ID, target, campaign(0x02)).unwrap(),
            ClaimResult::AlreadyClaimed
        ));
    }

    #[test]
    fn same_victim_different_campaign_is_rejected() {
        let dir = TempStore::new();
        let store = VictimClaimStore::bootstrap(&config_at(dir.path())).unwrap();
        let target = victim(0x22);

        assert!(matches!(
            store.try_claim(CHAIN_ID, target, campaign(0xAA)).unwrap(),
            ClaimResult::Claimed(_)
        ));
        assert!(matches!(
            store.try_claim(CHAIN_ID, target, campaign(0xBB)).unwrap(),
            ClaimResult::AlreadyClaimed
        ));
    }

    #[test]
    fn different_victims_claim_independently() {
        let dir = TempStore::new();
        let store = VictimClaimStore::bootstrap(&config_at(dir.path())).unwrap();
        assert!(matches!(
            store.try_claim(CHAIN_ID, victim(0x01), campaign(0x01)).unwrap(),
            ClaimResult::Claimed(_)
        ));
        assert!(matches!(
            store.try_claim(CHAIN_ID, victim(0x02), campaign(0x01)).unwrap(),
            ClaimResult::Claimed(_)
        ));
    }

    #[test]
    fn missing_or_mismatched_store_is_identity_mismatch() {
        let dir = TempStore::new();
        let config = config_at(dir.path());

        let zero = super::StoreIdentity::new([0u8; 32]);

        // Absent store: never auto-created.
        let err = VictimClaimStore::open_existing(&config, zero).unwrap_err();
        assert!(matches!(err, ClaimStoreError::StoreIdentityMismatch));

        // Existing store, wrong expected identity.
        let real = VictimClaimStore::bootstrap(&config).unwrap().store_identity();
        assert_ne!(real, zero);
        let err = VictimClaimStore::open_existing(&config, zero).unwrap_err();
        assert!(matches!(err, ClaimStoreError::StoreIdentityMismatch));
    }

    #[test]
    fn second_process_opener_is_refused() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        // Parent bootstraps and HOLDS the store (and its lock) for the duration.
        let store = VictimClaimStore::bootstrap(&config).unwrap();
        let identity = store.store_identity();

        let status = spawn_child(
            "second_opener",
            &[
                (CHILD_DB_ENV, config.db_path.to_str().unwrap().to_string()),
                (CHILD_IDENTITY_ENV, hex::encode(identity.as_bytes())),
            ],
        )
        .wait()
        .unwrap();

        // Keep the parent store (and lock) alive until the child has exited.
        drop(store);
        assert_eq!(
            status.code(),
            Some(EXIT_NOT_SINGLETON),
            "child opener should be refused with NotSingletonWriter"
        );
    }

    #[test]
    fn crash_before_commit_exposes_no_claim() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        let target = victim(0x5A);

        let mut child = spawn_child(
            "crash_cut",
            &[
                (CHILD_DB_ENV, config.db_path.to_str().unwrap().to_string()),
                (CHILD_VICTIM_ENV, hex::encode(target.as_slice())),
            ],
        );

        // Wait for the child to signal it has an uncommitted write open.
        let ready = config.db_path.with_extension("ready");
        let mut waited_ms = 0u64;
        while !ready.exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            waited_ms += 20;
            assert!(waited_ms < 30_000, "child never reached the pre-commit barrier");
        }

        // Hard-kill the child mid-transaction (SIGKILL on unix), simulating a
        // crash before commit.
        child.kill().unwrap();
        child.wait().unwrap();

        // The uncommitted partial write must not be visible after the crash.
        let claimed = store::test_probe_claimed(&config.db_path, CHAIN_ID, target).unwrap();
        assert!(!claimed, "an uncommitted (crashed) write must not expose a claim");
    }

    #[derive(Clone, Copy)]
    enum FailPhase {
        Write,
        Sync,
    }

    fn assert_commit_failure_issues_no_claim(phase: FailPhase) {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        let target = victim(0x7C);
        let camp = campaign(0x33);

        // Build a store over a fault-injecting backend. The inode lock is taken
        // on the same real file the backend writes to.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&config.db_path)
            .unwrap();
        let (backend, switches) =
            FailingBackend::new(redb::backends::FileBackend::new(file).unwrap());
        let db = redb::Database::builder().create_with_backend(backend).unwrap();
        // Provision metadata while both phases still succeed.
        let identity = store::bootstrap_metadata(&db).unwrap();
        let store = VictimClaimStore::from_parts(db, identity);

        // Arm the fault at the chosen phase and attempt a claim.
        match phase {
            FailPhase::Write => switches.write.store(true, Ordering::SeqCst),
            FailPhase::Sync => switches.sync.store(true, Ordering::SeqCst),
        }
        // The core invariant for BOTH phases: the failure surfaces as
        // `CommitFailed` and NO proof (`Claimed`) is issued — so the consumer
        // latches and never submits.
        let err = store.try_claim(CHAIN_ID, target, camp).unwrap_err();
        assert!(matches!(err, ClaimStoreError::CommitFailed(_)), "got {err:?}");

        drop(store);
        match phase {
            // A write-phase failure never let the bytes reach the file, so the
            // record is provably absent on reopen.
            FailPhase::Write => {
                let claimed = store::test_probe_claimed(&config.db_path, CHAIN_ID, target).unwrap();
                assert!(!claimed, "a failed write must not durably record a claim");
            }
            // A sync-phase failure means the bytes may already have landed
            // (durability is unknown) — the guarantee is only that no proof was
            // issued above. If the record did persist, a later attempt would see
            // it as AlreadyClaimed (still fail-closed, still at-most-once).
            FailPhase::Sync => {}
        }
    }

    #[test]
    fn commit_failure_at_write_phase_issues_no_claim() {
        assert_commit_failure_issues_no_claim(FailPhase::Write);
    }

    #[test]
    fn commit_failure_at_sync_phase_issues_no_claim() {
        assert_commit_failure_issues_no_claim(FailPhase::Sync);
    }

    #[test]
    fn unknown_claim_record_version_is_corruption() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        let target = victim(0x63);

        // Provision, then drop the store (releasing the inode lock).
        let identity = VictimClaimStore::bootstrap(&config).unwrap().store_identity();
        // Inject a claim record with an unsupported version byte directly.
        {
            let db = redb::Database::open(&config.db_path).unwrap();
            store::test_put_raw_claim(&db, CHAIN_ID, target, 0x02).unwrap();
        }

        let store = VictimClaimStore::open_existing(&config, identity).unwrap();
        let err = store.try_claim(CHAIN_ID, target, campaign(0x01)).unwrap_err();
        assert!(matches!(err, ClaimStoreError::Corruption(_)), "got {err:?}");
    }

    #[test]
    fn unknown_metadata_version_is_corruption() {
        let dir = TempStore::new();
        let config = config_at(dir.path());

        // Create a database whose metadata carries an unsupported version byte.
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&config.db_path)
                .unwrap();
            let db = redb::Database::builder()
                .create_with_backend(redb::backends::FileBackend::new(file).unwrap())
                .unwrap();
            store::test_put_raw_meta(&db, 0x02, [0x11; 32]).unwrap();
        }

        let err = VictimClaimStore::open_existing(&config, super::StoreIdentity::new([0x11; 32]))
            .unwrap_err();
        assert!(matches!(err, ClaimStoreError::Corruption(_)), "got {err:?}");
    }

    #[test]
    fn empty_database_file_is_identity_mismatch() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        // A zero-length file must never be adopted as a valid store.
        std::fs::File::create(&config.db_path).unwrap();
        let err = VictimClaimStore::open_existing(&config, super::StoreIdentity::new([0u8; 32]))
            .unwrap_err();
        assert!(matches!(err, ClaimStoreError::StoreIdentityMismatch), "got {err:?}");
    }

    #[test]
    fn hardlink_alias_second_opener_is_refused() {
        let dir = TempStore::new();
        let config = config_at(dir.path());
        // Provision and HOLD the store (its inode lock) for the duration.
        let store = VictimClaimStore::bootstrap(&config).unwrap();
        let identity = store.store_identity();

        // A hardlink alias points at the SAME inode under a different name.
        let alias = dir.path().join("alias.redb");
        std::fs::hard_link(&config.db_path, &alias).unwrap();
        let alias_config = VictimClaimConfig { db_path: alias };

        // Opening via the alias must be refused — flock conflicts are keyed on
        // the shared inode, not the path name.
        let err = VictimClaimStore::open_existing(&alias_config, identity).unwrap_err();
        assert!(matches!(err, ClaimStoreError::NotSingletonWriter), "got {err:?}");
        drop(store);
    }

    #[test]
    fn module_is_keyless() {
        // Reject any key-material / submission / egress / network token in the
        // module. Tokens are split via `concat!` so this test source does not
        // itself contain them verbatim (avoiding self-matches when mod.rs is
        // scanned).
        let forbidden = [
            concat!("re", "qwest"),
            concat!("eth_", "send", "RawTransaction"),
            concat!("eth_", "send", "Bundle"),
            concat!("send", "Bundle"),
            concat!("Private", "Key"),
            concat!("private", "_key"),
            concat!("Secret", "Key"),
            concat!("Sign", "er"),
            concat!("sign_", "transaction"),
            concat!("broad", "cast"),
            concat!("secp", "256k1"),
        ];
        let dir = env!("CARGO_MANIFEST_DIR");
        for file in ["mod.rs", "store.rs", "types.rs"] {
            let path = format!("{dir}/src/victim_claim/{file}");
            let source = std::fs::read_to_string(&path).unwrap().to_lowercase();
            for token in forbidden {
                assert!(
                    !source.contains(&token.to_lowercase()),
                    "keyless violation: `{token}` found in {file}"
                );
            }
        }
    }

    // ---- child-process machinery ----------------------------------------

    fn spawn_child(role: &str, envs: &[(&str, String)]) -> std::process::Child {
        let exe = std::env::current_exe().unwrap();
        let mut cmd = std::process::Command::new(exe);
        cmd.args([CHILD_TEST_PATH, "--exact", "--test-threads=1", "--nocapture"]);
        cmd.env(CHILD_ROLE_ENV, role);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.spawn().unwrap()
    }

    fn decode_b256(value: &str) -> B256 {
        B256::from_slice(&hex::decode(value).unwrap())
    }

    fn decode_identity(value: &str) -> super::StoreIdentity {
        let bytes = hex::decode(value).unwrap();
        let mut nonce = [0u8; 32];
        nonce.copy_from_slice(&bytes);
        super::StoreIdentity::new(nonce)
    }

    /// Multiplexed child-process entrypoint. A no-op pass when `R9_CHILD_ROLE`
    /// is unset (i.e. during a normal parent test run); otherwise it performs
    /// the requested role and exits with a distinguishing code / parks.
    #[test]
    fn r9_child_entrypoint() {
        let Ok(role) = std::env::var(CHILD_ROLE_ENV) else {
            return;
        };
        let config = VictimClaimConfig {
            db_path: std::path::PathBuf::from(std::env::var(CHILD_DB_ENV).unwrap()),
        };
        match role.as_str() {
            "second_opener" => {
                let expected = decode_identity(&std::env::var(CHILD_IDENTITY_ENV).unwrap());
                match VictimClaimStore::open_existing(&config, expected) {
                    Err(ClaimStoreError::NotSingletonWriter) => {
                        std::process::exit(EXIT_NOT_SINGLETON)
                    }
                    other => {
                        eprintln!("unexpected second-opener outcome: {other:?}");
                        std::process::exit(EXIT_UNEXPECTED)
                    }
                }
            }
            "crash_cut" => {
                let target = decode_b256(&std::env::var(CHILD_VICTIM_ENV).unwrap());
                store::test_insert_no_commit_and_park(
                    &config.db_path,
                    CHAIN_ID,
                    target,
                    campaign(0x99),
                );
            }
            _ => std::process::exit(EXIT_UNEXPECTED),
        }
    }

    /// A [`redb::StorageBackend`] wrapper that injects a fault at a chosen phase.
    /// `fail_write` / `fail_sync` are flipped on by the test after provisioning
    /// succeeds, so a commit fails specifically at the write or the sync phase.
    #[derive(Debug)]
    struct FailingBackend {
        inner: redb::backends::FileBackend,
        fail_write: Arc<AtomicBool>,
        fail_sync: Arc<AtomicBool>,
    }

    /// Shared handles to a [`FailingBackend`]'s phase-fault switches.
    struct FaultSwitches {
        write: Arc<AtomicBool>,
        sync: Arc<AtomicBool>,
    }

    impl FailingBackend {
        fn new(inner: redb::backends::FileBackend) -> (Self, FaultSwitches) {
            let fail_write = Arc::new(AtomicBool::new(false));
            let fail_sync = Arc::new(AtomicBool::new(false));
            let switches =
                FaultSwitches { write: Arc::clone(&fail_write), sync: Arc::clone(&fail_sync) };
            (Self { inner, fail_write, fail_sync }, switches)
        }
    }

    impl redb::StorageBackend for FailingBackend {
        fn len(&self) -> Result<u64, std::io::Error> {
            self.inner.len()
        }

        fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, std::io::Error> {
            self.inner.read(offset, len)
        }

        fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
            self.inner.set_len(len)
        }

        fn sync_data(&self, eventual: bool) -> Result<(), std::io::Error> {
            if self.fail_sync.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("injected sync fault"));
            }
            self.inner.sync_data(eventual)
        }

        fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("injected write fault"));
            }
            self.inner.write(offset, data)
        }
    }
}
