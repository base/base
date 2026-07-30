//! Offline producer conformance and crash-safe publication for T4e artifacts.

#[cfg(feature = "arm-provisioning")]
use std::{fs::DirBuilder, os::unix::fs::DirBuilderExt};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[cfg(feature = "arm-provisioning")]
use super::settled_loss::R9_CLAIM_STORE_PATH;
use alloy_primitives::{Address, B256, keccak256};
#[cfg(feature = "arm-provisioning")]
use base_mev_trader::{ClaimStoreError, VictimClaimConfig, VictimClaimStore};

use super::settled_loss::{
    FrozenP2PopulationManifestV1, INSTALL_BUNDLE_DOMAIN, MAX_POPULATION_MANIFEST_BYTES,
    MAX_PROJECTION_BYTES, P2_POPULATION_MANIFEST_PATH, SETTLED_LOSS_PROJECTION_PATH,
    SETTLED_LOSS_SCHEMA_VERSION, SourceSubmissionManifestEntryV1, T4E_INSTALL_BUNDLE_PATH,
    TerminalSettlementEntryV1, TerminalSettlementProjectionV1, proc_fd_path, read_strict_bytes,
    validate_directory_inventory, verify_canonical_signature, verify_signature_shape,
};

const INSTALL_BUNDLE_BYTES: usize = 584;
const G7_PAIR_BYTES: usize = 113;
const LIVE_PAIR_BYTES: usize = 113;
const DEPLOYMENT_PAIR_BYTES: usize = 221;
const G7_DOMAIN: &[u8] = b"base-mev/g7-closure/v1";
const LIVE_DOMAIN: &[u8] = b"base-mev/live-run/v1";
const DEPLOYMENT_DOMAIN: &[u8] = b"base-mev/deploy/v1";
const BASE_CHAIN_ID: u64 = 8453;

/// Bounded publication I/O failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationIoClass {
    /// An artifact or directory could not be opened safely.
    Open,
    /// A complete artifact write failed.
    Write,
    /// File data or metadata could not be synchronized.
    FileSync,
    /// Atomic publication failed.
    Rename,
    /// The containing directory could not be synchronized.
    DirectorySync,
    /// File or directory metadata violated the pinned policy.
    Metadata,
    /// A directory contained a stale temporary or unknown object.
    Inventory,
}

/// Closed producer-conformance failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerError {
    /// A bounded input exceeded its limit.
    Bounds,
    /// Canonical bytes could not be decoded.
    Decode,
    /// Decoded bytes were not their unique canonical encoding.
    Canonicality,
    /// A terminal reason was inconsistent with its evidence.
    Reason,
    /// A terminal formula could not be evaluated exactly.
    Formula,
    /// Checked arithmetic failed.
    Arithmetic,
    /// An owner signature was invalid.
    Signature,
    /// Artifact identity did not match an existing immutable publication.
    Identity,
    /// Entries were not in canonical order.
    Order,
    /// Population or terminal coverage was incomplete.
    Coverage,
    /// A filesystem object violated the no-link private-object policy.
    UnsafeObject,
    /// A bounded publication operation failed.
    Io(PublicationIoClass),
}

/// Canonical, owner-authenticated population bytes ready for publication.
#[derive(Debug)]
pub struct SignedPopulationManifestV1 {
    canonical: Vec<u8>,
}

impl SignedPopulationManifestV1 {
    /// Validates externally signed canonical population bytes without signing or acquiring data.
    pub fn from_canonical(canonical: Vec<u8>) -> Result<Self, ProducerError> {
        if canonical.is_empty() || canonical.len() > MAX_POPULATION_MANIFEST_BYTES {
            return Err(ProducerError::Bounds);
        }
        let decoded = FrozenP2PopulationManifestV1::decode_checked(&canonical)
            .map_err(|_| ProducerError::Signature)?;
        if decoded.encode() != canonical {
            return Err(ProducerError::Canonicality);
        }
        Ok(Self { canonical })
    }

    /// Returns the exact authenticated publication bytes.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
}

/// Non-clone proof that the exact population bytes have been durably published.
#[derive(Debug)]
pub struct PublishedPopulationManifestV1 {
    canonical: Vec<u8>,
}

impl PublishedPopulationManifestV1 {
    /// Returns the exact bytes whose immutable publication was verified.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
}

/// Canonical, owner-authenticated terminal projection ready for publication.
#[derive(Debug)]
pub struct SignedProjectionV1 {
    canonical: Vec<u8>,
}

impl SignedProjectionV1 {
    /// Validates externally signed canonical projection bytes without signing or acquiring evidence.
    pub fn from_canonical(canonical: Vec<u8>) -> Result<Self, ProducerError> {
        if canonical.is_empty() || canonical.len() > MAX_PROJECTION_BYTES {
            return Err(ProducerError::Bounds);
        }
        let decoded = TerminalSettlementProjectionV1::decode_checked(&canonical)
            .map_err(|_| ProducerError::Signature)?;
        if decoded.encode() != canonical {
            return Err(ProducerError::Canonicality);
        }
        Ok(Self { canonical })
    }

    /// Returns the exact authenticated publication bytes.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
}

/// Canonical owner-signed G7 fields and inner signature.
#[derive(Debug)]
pub struct CanonicalG7PairV1 {
    canonical: [u8; G7_PAIR_BYTES],
}

impl CanonicalG7PairV1 {
    /// Verifies and canonicalizes one externally owner-signed G7 pair.
    pub fn new(
        campaign_id: B256,
        g7_closure_epoch: u64,
        expiry_unix: u64,
        signature: [u8; 65],
    ) -> Result<Self, ProducerError> {
        if campaign_id == B256::ZERO {
            return Err(ProducerError::Identity);
        }
        let mut preimage = Vec::with_capacity(80);
        preimage.extend_from_slice(keccak256(G7_DOMAIN).as_slice());
        preimage.extend_from_slice(campaign_id.as_slice());
        preimage.extend_from_slice(&g7_closure_epoch.to_be_bytes());
        preimage.extend_from_slice(&expiry_unix.to_be_bytes());
        verify_canonical_signature(&signature, &preimage).map_err(|_| ProducerError::Signature)?;
        let mut canonical = [0u8; G7_PAIR_BYTES];
        canonical[..32].copy_from_slice(campaign_id.as_slice());
        canonical[32..40].copy_from_slice(&g7_closure_epoch.to_be_bytes());
        canonical[40..48].copy_from_slice(&expiry_unix.to_be_bytes());
        canonical[48..].copy_from_slice(&signature);
        Ok(Self { canonical })
    }
}

/// Canonical owner-signed live-run fields and inner signature.
#[derive(Debug)]
pub struct CanonicalLivePairV1 {
    canonical: [u8; LIVE_PAIR_BYTES],
}

impl CanonicalLivePairV1 {
    /// Verifies and canonicalizes one externally owner-signed live-run pair.
    pub fn new(
        campaign_id: B256,
        window_start: u64,
        expiry_unix: u64,
        signature: [u8; 65],
    ) -> Result<Self, ProducerError> {
        if campaign_id == B256::ZERO || window_start >= expiry_unix {
            return Err(ProducerError::Identity);
        }
        let mut preimage = Vec::with_capacity(80);
        preimage.extend_from_slice(keccak256(LIVE_DOMAIN).as_slice());
        preimage.extend_from_slice(campaign_id.as_slice());
        preimage.extend_from_slice(&window_start.to_be_bytes());
        preimage.extend_from_slice(&expiry_unix.to_be_bytes());
        verify_canonical_signature(&signature, &preimage).map_err(|_| ProducerError::Signature)?;
        let mut canonical = [0u8; LIVE_PAIR_BYTES];
        canonical[..32].copy_from_slice(campaign_id.as_slice());
        canonical[32..40].copy_from_slice(&window_start.to_be_bytes());
        canonical[40..48].copy_from_slice(&expiry_unix.to_be_bytes());
        canonical[48..].copy_from_slice(&signature);
        Ok(Self { canonical })
    }
}

/// Canonical owner-signed deployment fields and inner signature.
#[derive(Debug)]
pub struct CanonicalDeploymentPairV1 {
    canonical: [u8; DEPLOYMENT_PAIR_BYTES],
}

impl CanonicalDeploymentPairV1 {
    /// Verifies and canonicalizes one externally owner-signed Base deployment pair.
    pub fn new(
        chain_id: u64,
        executor: Address,
        code_hash: B256,
        binary_digest: B256,
        deployment_digest: B256,
        r9_store_identity: B256,
        signature: [u8; 65],
    ) -> Result<Self, ProducerError> {
        if chain_id != BASE_CHAIN_ID
            || executor == Address::ZERO
            || code_hash == B256::ZERO
            || binary_digest == B256::ZERO
            || deployment_digest == B256::ZERO
            || r9_store_identity == B256::ZERO
        {
            return Err(ProducerError::Identity);
        }
        let mut preimage = Vec::with_capacity(188);
        preimage.extend_from_slice(keccak256(DEPLOYMENT_DOMAIN).as_slice());
        preimage.extend_from_slice(&chain_id.to_be_bytes());
        preimage.extend_from_slice(executor.as_slice());
        preimage.extend_from_slice(code_hash.as_slice());
        preimage.extend_from_slice(binary_digest.as_slice());
        preimage.extend_from_slice(deployment_digest.as_slice());
        preimage.extend_from_slice(r9_store_identity.as_slice());
        verify_canonical_signature(&signature, &preimage).map_err(|_| ProducerError::Signature)?;

        let mut canonical = [0u8; DEPLOYMENT_PAIR_BYTES];
        canonical[..8].copy_from_slice(&chain_id.to_be_bytes());
        canonical[8..28].copy_from_slice(executor.as_slice());
        canonical[28..60].copy_from_slice(code_hash.as_slice());
        canonical[60..92].copy_from_slice(binary_digest.as_slice());
        canonical[92..124].copy_from_slice(deployment_digest.as_slice());
        canonical[124..156].copy_from_slice(r9_store_identity.as_slice());
        canonical[156..].copy_from_slice(&signature);
        Ok(Self { canonical })
    }
}

/// Non-clone canonical install-bundle bytes awaiting the outer owner signature.
#[derive(Debug)]
pub struct UnsignedInstallBundleV1 {
    canonical_body: Vec<u8>,
}

impl UnsignedInstallBundleV1 {
    /// Returns the outer signature preimage `domain || bundle_content_hash`.
    pub fn outer_signature_preimage(&self) -> &[u8] {
        let start = self.canonical_body.len() - INSTALL_BUNDLE_DOMAIN.len() - 32;
        &self.canonical_body[start..]
    }

    /// Returns the canonical body ending with `bundle_content_hash`.
    pub fn canonical_body(&self) -> &[u8] {
        &self.canonical_body[..self.canonical_body.len() - INSTALL_BUNDLE_DOMAIN.len() - 32]
    }
}

/// Canonical mixed-generation-resistant install bundle ready for publication.
#[derive(Debug)]
pub struct SignedInstallBundleV1 {
    canonical: Vec<u8>,
}

impl SignedInstallBundleV1 {
    /// Validates all fixed fields, six signature shapes, inner owner signatures, hash, and outer owner signature.
    pub fn from_canonical(canonical: Vec<u8>) -> Result<Self, ProducerError> {
        validate_install_bundle(&canonical)?;
        Ok(Self { canonical })
    }

    /// Returns the exact authenticated publication bytes.
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
}

/// Canonical UTF-8 submission identifier bounded before hashing or allocation growth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedSubmissionIdV1 {
    bytes: Vec<u8>,
}

impl BoundedSubmissionIdV1 {
    /// Accepts exactly 1 through 128 canonical UTF-8 bytes.
    pub fn new(bytes: Vec<u8>) -> Result<Self, ProducerError> {
        if bytes.is_empty() || bytes.len() > 128 || std::str::from_utf8(&bytes).is_err() {
            return Err(ProducerError::Bounds);
        }
        Ok(Self { bytes })
    }
}

/// One immutable insert-once P2 source-ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLedgerRowV1 {
    submission_id: BoundedSubmissionIdV1,
    chain_id: u64,
    target_tx_hash: B256,
    our_backrun_tx_hash: B256,
    submit_wallclock_ms: u64,
}

impl SourceLedgerRowV1 {
    /// Constructs one explicit source row without a database or callback.
    pub const fn new(
        submission_id: BoundedSubmissionIdV1,
        chain_id: u64,
        target_tx_hash: B256,
        our_backrun_tx_hash: B256,
        submit_wallclock_ms: u64,
    ) -> Self {
        Self { submission_id, chain_id, target_tx_hash, our_backrun_tx_hash, submit_wallclock_ms }
    }
}

/// Signed snapshot-closure identity copied into the frozen population.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopulationClosureFieldsV1 {
    campaign_id: B256,
    chain_id: u64,
    source_window_start_ms: u64,
    source_window_end_ms: u64,
    source_snapshot_xmin: u64,
    source_snapshot_xmax: u64,
    source_snapshot_xip_hash: B256,
    source_snapshot_wal_lsn: u64,
}

impl PopulationClosureFieldsV1 {
    /// Constructs explicit closure fields; validation occurs during manifest preparation.
    pub const fn new(
        campaign_id: B256,
        chain_id: u64,
        source_window_start_ms: u64,
        source_window_end_ms: u64,
        source_snapshot_xmin: u64,
        source_snapshot_xmax: u64,
        source_snapshot_xip_hash: B256,
        source_snapshot_wal_lsn: u64,
    ) -> Self {
        Self {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
        }
    }
}

/// Non-clone canonical population preimage awaiting an external owner signature.
#[derive(Debug)]
pub struct UnsignedPopulationManifestV1 {
    canonical_preimage: Vec<u8>,
}

impl UnsignedPopulationManifestV1 {
    /// Returns the only bytes the external owner may sign.
    pub fn canonical_preimage(&self) -> &[u8] {
        &self.canonical_preimage
    }
}

/// Non-clone canonical projection body awaiting an external owner signature.
#[derive(Debug)]
pub struct UnsignedProjectionV1 {
    canonical_body: Vec<u8>,
    signature_preimage: Vec<u8>,
}

impl UnsignedProjectionV1 {
    /// Returns the exact bytes the external owner may sign.
    pub fn signature_preimage(&self) -> &[u8] {
        &self.signature_preimage
    }

    /// Returns the canonical projection body ending with its content hash.
    pub fn canonical_body(&self) -> &[u8] {
        &self.canonical_body
    }
}

/// Explicit authenticated closure fields copied into one terminal projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionClosureFieldsV1 {
    campaign_id: B256,
    chain_id: u64,
    source_window_start_ms: u64,
    source_window_end_ms: u64,
    source_snapshot_xmin: u64,
    source_snapshot_xmax: u64,
    source_snapshot_xip_hash: B256,
    source_snapshot_wal_lsn: u64,
    projection_sequence: u64,
    source_manifest_hash: B256,
    population_closure_signature: [u8; 65],
    finalized_block_number: u64,
    finalized_block_hash: B256,
    previous_content_hash: B256,
}

impl ProjectionClosureFieldsV1 {
    /// Constructs explicit projection closure fields; preparation validates the complete wire.
    #[expect(clippy::too_many_arguments, reason = "the canonical projection header is explicit")]
    pub const fn new(
        campaign_id: B256,
        chain_id: u64,
        source_window_start_ms: u64,
        source_window_end_ms: u64,
        source_snapshot_xmin: u64,
        source_snapshot_xmax: u64,
        source_snapshot_xip_hash: B256,
        source_snapshot_wal_lsn: u64,
        projection_sequence: u64,
        source_manifest_hash: B256,
        population_closure_signature: [u8; 65],
        finalized_block_number: u64,
        finalized_block_hash: B256,
        previous_content_hash: B256,
    ) -> Self {
        Self {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
            projection_sequence,
            source_manifest_hash,
            population_closure_signature,
            finalized_block_number,
            finalized_block_hash,
            previous_content_hash,
        }
    }
}

/// Offline-only artifact validator and publisher with no signer, database, RPC, or network surface.
#[derive(Debug, Clone, Copy)]
pub struct ProducerConformance;

impl ProducerConformance {
    /// Creates and validates the four private compile-pinned artifact directories.
    #[cfg(feature = "arm-provisioning")]
    pub fn prepare_directories() -> Result<(), ProducerError> {
        for artifact in [
            T4E_INSTALL_BUNDLE_PATH,
            SETTLED_LOSS_PROJECTION_PATH,
            P2_POPULATION_MANIFEST_PATH,
            R9_CLAIM_STORE_PATH,
        ] {
            let directory = Path::new(artifact).parent().ok_or(ProducerError::UnsafeObject)?;
            prepare_private_directory(directory)?;
        }
        Ok(())
    }

    /// Creates the identity-bearing R9 claim store or adopts its existing identity idempotently.
    #[cfg(feature = "arm-provisioning")]
    pub fn provision_claim_store() -> Result<[u8; 32], ClaimStoreError> {
        Self::prepare_directories().map_err(|error| {
            ClaimStoreError::Io(format!("T4e directory preparation failed: {error:?}"))
        })?;
        let store = VictimClaimStore::bootstrap(&VictimClaimConfig {
            db_path: R9_CLAIM_STORE_PATH.into(),
        })?;
        Ok(*store.store_identity().as_bytes())
    }

    /// Validates and immutably publishes an externally owner-signed population file.
    #[cfg(feature = "arm-provisioning")]
    pub fn publish_population_file(path: &Path) -> Result<(), ProducerError> {
        Self::prepare_directories()?;
        let canonical = read_bounded(path, MAX_POPULATION_MANIFEST_BYTES)?;
        let manifest = SignedPopulationManifestV1::from_canonical(canonical)?;
        Self::publish_population(manifest).map(|_| ())
    }

    /// Validates and atomically publishes an externally owner-signed projection file.
    #[cfg(feature = "arm-provisioning")]
    pub fn publish_projection_file(path: &Path) -> Result<(), ProducerError> {
        Self::prepare_directories()?;
        let canonical = read_bounded(path, MAX_PROJECTION_BYTES)?;
        let projection = SignedProjectionV1::from_canonical(canonical)?;
        Self::publish_projection(projection)
    }

    /// Validates and atomically publishes an externally owner-signed install-bundle file.
    #[cfg(feature = "arm-provisioning")]
    pub fn publish_install_bundle_file(path: &Path) -> Result<(), ProducerError> {
        Self::prepare_directories()?;
        let canonical = read_bounded(path, INSTALL_BUNDLE_BYTES)?;
        let bundle = SignedInstallBundleV1::from_canonical(canonical)?;
        Self::publish_install_bundle(bundle)
    }

    /// Builds the fixed-order install bundle body and outer-signature preimage.
    pub fn prepare_install_bundle(
        generation: u64,
        g7: CanonicalG7PairV1,
        live: CanonicalLivePairV1,
        deployment: CanonicalDeploymentPairV1,
    ) -> Result<UnsignedInstallBundleV1, ProducerError> {
        if generation == 0 || g7.canonical[..32] != live.canonical[..32] {
            return Err(ProducerError::Identity);
        }
        let mut hash_preimage = Vec::with_capacity(136);
        hash_preimage.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
        hash_preimage.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        hash_preimage.extend_from_slice(&generation.to_be_bytes());
        hash_preimage.extend_from_slice(keccak256(g7.canonical).as_slice());
        hash_preimage.extend_from_slice(keccak256(live.canonical).as_slice());
        hash_preimage.extend_from_slice(keccak256(deployment.canonical).as_slice());
        let content_hash = keccak256(hash_preimage);

        let mut canonical_body = Vec::with_capacity(INSTALL_BUNDLE_BYTES - 65 + 62);
        canonical_body.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
        canonical_body.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        canonical_body.extend_from_slice(&generation.to_be_bytes());
        canonical_body.extend_from_slice(&g7.canonical);
        canonical_body.extend_from_slice(&live.canonical);
        canonical_body.extend_from_slice(&deployment.canonical);
        canonical_body.extend_from_slice(content_hash.as_slice());
        canonical_body.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
        canonical_body.extend_from_slice(content_hash.as_slice());
        Ok(UnsignedInstallBundleV1 { canonical_body })
    }

    /// Attaches and verifies the install bundle's externally supplied outer owner signature.
    pub fn attach_install_bundle_signature(
        unsigned: UnsignedInstallBundleV1,
        signature: [u8; 65],
    ) -> Result<SignedInstallBundleV1, ProducerError> {
        verify_canonical_signature(&signature, unsigned.outer_signature_preimage())
            .map_err(|_| ProducerError::Signature)?;
        let canonical_length = unsigned.canonical_body.len() - INSTALL_BUNDLE_DOMAIN.len() - 32;
        let mut canonical = unsigned.canonical_body;
        canonical.truncate(canonical_length);
        canonical.extend_from_slice(&signature);
        SignedInstallBundleV1::from_canonical(canonical)
    }

    /// Builds and validates the sole canonical terminal-projection body and signature preimage.
    pub fn prepare_terminal_projection(
        source_entries: Vec<SourceSubmissionManifestEntryV1>,
        terminal_entries: Vec<TerminalSettlementEntryV1>,
        closure: ProjectionClosureFieldsV1,
    ) -> Result<UnsignedProjectionV1, ProducerError> {
        let (canonical_body, signature_preimage) =
            TerminalSettlementProjectionV1::prepare_unsigned(
                closure.campaign_id,
                closure.chain_id,
                closure.source_window_start_ms,
                closure.source_window_end_ms,
                closure.source_snapshot_xmin,
                closure.source_snapshot_xmax,
                closure.source_snapshot_xip_hash,
                closure.source_snapshot_wal_lsn,
                closure.projection_sequence,
                closure.source_manifest_hash,
                closure.population_closure_signature,
                closure.finalized_block_number,
                closure.finalized_block_hash,
                closure.previous_content_hash,
                source_entries,
                terminal_entries,
            )
            .map_err(|error| match error {
                super::settled_loss::SettledLossUnavailableReason::ManifestMismatch => {
                    ProducerError::Coverage
                }
                super::settled_loss::SettledLossUnavailableReason::AuthenticationFailed => {
                    ProducerError::Signature
                }
                _ => ProducerError::Formula,
            })?;
        Ok(UnsignedProjectionV1 { canonical_body, signature_preimage })
    }

    /// Attaches and verifies an externally supplied projection owner signature.
    pub fn attach_projection_signature(
        unsigned: UnsignedProjectionV1,
        signature: [u8; 65],
    ) -> Result<SignedProjectionV1, ProducerError> {
        verify_canonical_signature(&signature, &unsigned.signature_preimage)
            .map_err(|_| ProducerError::Signature)?;
        let mut canonical = unsigned.canonical_body;
        canonical.extend_from_slice(&signature);
        SignedProjectionV1::from_canonical(canonical)
    }

    /// Rehydrates an exact population preimage from a request directory and attaches its signature.
    pub fn attach_population_signature_bytes(
        canonical_preimage: Vec<u8>,
        signature: [u8; 65],
    ) -> Result<SignedPopulationManifestV1, ProducerError> {
        if canonical_preimage.is_empty()
            || canonical_preimage.len() > MAX_POPULATION_MANIFEST_BYTES - 65
        {
            return Err(ProducerError::Bounds);
        }
        verify_canonical_signature(&signature, &canonical_preimage)
            .map_err(|_| ProducerError::Signature)?;
        let mut canonical = canonical_preimage;
        canonical.extend_from_slice(&signature);
        SignedPopulationManifestV1::from_canonical(canonical)
    }

    /// Rehydrates an exact projection request and attaches its externally supplied signature.
    pub fn attach_projection_signature_bytes(
        canonical_body: Vec<u8>,
        signature_preimage: Vec<u8>,
        signature: [u8; 65],
    ) -> Result<SignedProjectionV1, ProducerError> {
        if canonical_body.len() < 32 || canonical_body.len() > MAX_PROJECTION_BYTES - 65 {
            return Err(ProducerError::Bounds);
        }
        let content_hash = &canonical_body[canonical_body.len() - 32..];
        if signature_preimage.len() != super::settled_loss::SETTLED_LOSS_DOMAIN.len() + 32
            || !signature_preimage.starts_with(super::settled_loss::SETTLED_LOSS_DOMAIN)
            || &signature_preimage[super::settled_loss::SETTLED_LOSS_DOMAIN.len()..] != content_hash
        {
            return Err(ProducerError::Canonicality);
        }
        verify_canonical_signature(&signature, &signature_preimage)
            .map_err(|_| ProducerError::Signature)?;
        let mut canonical = canonical_body;
        canonical.extend_from_slice(&signature);
        SignedProjectionV1::from_canonical(canonical)
    }

    /// Rehydrates an exact install request and attaches its externally supplied outer signature.
    pub fn attach_install_bundle_signature_bytes(
        canonical_body: Vec<u8>,
        signature_preimage: Vec<u8>,
        signature: [u8; 65],
    ) -> Result<SignedInstallBundleV1, ProducerError> {
        if canonical_body.len() != INSTALL_BUNDLE_BYTES - 65
            || signature_preimage.len() != INSTALL_BUNDLE_DOMAIN.len() + 32
            || !signature_preimage.starts_with(INSTALL_BUNDLE_DOMAIN)
            || &canonical_body[canonical_body.len() - 32..]
                != &signature_preimage[INSTALL_BUNDLE_DOMAIN.len()..]
        {
            return Err(ProducerError::Canonicality);
        }
        verify_canonical_signature(&signature, &signature_preimage)
            .map_err(|_| ProducerError::Signature)?;
        let mut canonical = canonical_body;
        canonical.extend_from_slice(&signature);
        SignedInstallBundleV1::from_canonical(canonical)
    }
    /// Deterministically derives the complete frozen population from ordered bounded source rows.
    pub fn prepare_frozen_manifest(
        rows: Vec<SourceLedgerRowV1>,
        closure: PopulationClosureFieldsV1,
    ) -> Result<UnsignedPopulationManifestV1, ProducerError> {
        if rows.is_empty() || rows.len() > super::settled_loss::MAX_TERMINAL_ENTRIES {
            return Err(ProducerError::Bounds);
        }
        if closure.campaign_id == B256::ZERO
            || closure.chain_id != BASE_CHAIN_ID
            || closure.source_window_start_ms >= closure.source_window_end_ms
            || closure.source_snapshot_xmin > closure.source_snapshot_xmax
        {
            return Err(ProducerError::Identity);
        }

        let mut encoded_entries = Vec::with_capacity(
            rows.len()
                .checked_mul(super::settled_loss::SOURCE_ENTRY_BYTES)
                .ok_or(ProducerError::Arithmetic)?,
        );
        let mut previous_order: Option<(u64, &[u8])> = None;
        for (sequence, row) in rows.iter().enumerate() {
            if row.chain_id != closure.chain_id
                || row.submit_wallclock_ms < closure.source_window_start_ms
                || row.submit_wallclock_ms >= closure.source_window_end_ms
            {
                return Err(ProducerError::Identity);
            }
            let order = (row.submit_wallclock_ms, row.submission_id.bytes.as_slice());
            if previous_order.is_some_and(|previous| previous >= order) {
                return Err(ProducerError::Order);
            }
            previous_order = Some(order);

            let mut id_preimage = Vec::with_capacity(36 + row.submission_id.bytes.len());
            id_preimage.extend_from_slice(b"base-mev/p2-submission-id/v1");
            id_preimage.extend_from_slice(
                &u32::try_from(row.submission_id.bytes.len())
                    .map_err(|_| ProducerError::Bounds)?
                    .to_be_bytes(),
            );
            id_preimage.extend_from_slice(&row.submission_id.bytes);
            let source_submission_id = keccak256(id_preimage);
            let mut correlation_preimage = Vec::with_capacity(128);
            correlation_preimage.extend_from_slice(b"base-mev/p2-correlation/v1");
            correlation_preimage.extend_from_slice(source_submission_id.as_slice());
            correlation_preimage.extend_from_slice(row.target_tx_hash.as_slice());
            correlation_preimage.extend_from_slice(row.our_backrun_tx_hash.as_slice());
            let correlation_key = keccak256(correlation_preimage);

            encoded_entries.extend_from_slice(
                &u64::try_from(sequence).map_err(|_| ProducerError::Bounds)?.to_be_bytes(),
            );
            encoded_entries.extend_from_slice(source_submission_id.as_slice());
            encoded_entries.extend_from_slice(row.target_tx_hash.as_slice());
            encoded_entries.extend_from_slice(correlation_key.as_slice());
            encoded_entries.extend_from_slice(row.our_backrun_tx_hash.as_slice());
            encoded_entries.push(1);
            encoded_entries.extend_from_slice(row.our_backrun_tx_hash.as_slice());
        }

        let submission_count = u64::try_from(rows.len()).map_err(|_| ProducerError::Bounds)?;
        let source_manifest_hash = keccak256(&encoded_entries);
        let mut canonical_preimage = Vec::with_capacity(191 + encoded_entries.len());
        canonical_preimage.extend_from_slice(super::settled_loss::POPULATION_CLOSURE_DOMAIN);
        canonical_preimage.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        canonical_preimage.extend_from_slice(closure.campaign_id.as_slice());
        canonical_preimage.extend_from_slice(&closure.chain_id.to_be_bytes());
        canonical_preimage.extend_from_slice(&closure.source_window_start_ms.to_be_bytes());
        canonical_preimage.extend_from_slice(&closure.source_window_end_ms.to_be_bytes());
        canonical_preimage.extend_from_slice(&closure.source_snapshot_xmin.to_be_bytes());
        canonical_preimage.extend_from_slice(&closure.source_snapshot_xmax.to_be_bytes());
        canonical_preimage.extend_from_slice(closure.source_snapshot_xip_hash.as_slice());
        canonical_preimage.extend_from_slice(&closure.source_snapshot_wal_lsn.to_be_bytes());
        canonical_preimage.extend_from_slice(&submission_count.to_be_bytes());
        canonical_preimage.extend_from_slice(source_manifest_hash.as_slice());
        canonical_preimage.extend_from_slice(
            &u32::try_from(rows.len()).map_err(|_| ProducerError::Bounds)?.to_be_bytes(),
        );
        canonical_preimage.extend_from_slice(&encoded_entries);
        Ok(UnsignedPopulationManifestV1 { canonical_preimage })
    }

    /// Attaches and verifies an externally supplied owner signature.
    pub fn attach_population_signature(
        unsigned: UnsignedPopulationManifestV1,
        signature: [u8; 65],
    ) -> Result<SignedPopulationManifestV1, ProducerError> {
        verify_canonical_signature(&signature, &unsigned.canonical_preimage)
            .map_err(|_| ProducerError::Signature)?;
        let mut canonical = unsigned.canonical_preimage;
        canonical.extend_from_slice(&signature);
        SignedPopulationManifestV1::from_canonical(canonical)
    }
    /// Immutably publishes a population manifest, accepting only exact-byte idempotence.
    pub fn publish_population(
        manifest: SignedPopulationManifestV1,
    ) -> Result<PublishedPopulationManifestV1, ProducerError> {
        publish_population_at(Path::new(P2_POPULATION_MANIFEST_PATH), manifest.canonical)
    }

    /// Reloads and authenticates the immutable population publication for restart/resume.
    pub fn load_published_population() -> Result<PublishedPopulationManifestV1, ProducerError> {
        let directory = Path::new("/home/ubuntu/.local/state/base-mev/p2-population-v1");
        validate_directory_inventory(directory, &["manifest.bin", "manifest.bin.open"])
            .map_err(map_consumer_io)?;
        let canonical = read_strict_bytes(
            Path::new(P2_POPULATION_MANIFEST_PATH),
            1,
            MAX_POPULATION_MANIFEST_BYTES,
        )
        .map_err(map_consumer_io)?;
        validate_directory_inventory(directory, &["manifest.bin", "manifest.bin.open"])
            .map_err(map_consumer_io)?;
        let manifest = SignedPopulationManifestV1::from_canonical(canonical)?;
        Ok(PublishedPopulationManifestV1 { canonical: manifest.canonical })
    }

    /// Atomically replaces the current projection with one authenticated successor.
    pub fn publish_projection(projection: SignedProjectionV1) -> Result<(), ProducerError> {
        publish_replace_at(
            Path::new(SETTLED_LOSS_PROJECTION_PATH),
            &projection.canonical,
            &["accepted-head"],
        )
    }

    /// Atomically replaces the current install bundle with one authenticated generation.
    pub fn publish_install_bundle(bundle: SignedInstallBundleV1) -> Result<(), ProducerError> {
        publish_replace_at(Path::new(T4E_INSTALL_BUNDLE_PATH), &bundle.canonical, &[])
    }
}

fn validate_install_bundle(bytes: &[u8]) -> Result<(), ProducerError> {
    if bytes.len() != INSTALL_BUNDLE_BYTES
        || bytes[..INSTALL_BUNDLE_DOMAIN.len()] != INSTALL_BUNDLE_DOMAIN[..]
        || u16::from_be_bytes(bytes[30..32].try_into().map_err(|_| ProducerError::Decode)?)
            != SETTLED_LOSS_SCHEMA_VERSION
        || u64::from_be_bytes(bytes[32..40].try_into().map_err(|_| ProducerError::Decode)?) == 0
    {
        return Err(ProducerError::Canonicality);
    }

    let g7_start = 40;
    let live_start = g7_start + G7_PAIR_BYTES;
    let deployment_start = live_start + LIVE_PAIR_BYTES;
    let content_hash_start = deployment_start + DEPLOYMENT_PAIR_BYTES;
    let outer_signature_start = content_hash_start + 32;

    let g7_signature: &[u8; 65] =
        bytes[g7_start + 48..live_start].try_into().map_err(|_| ProducerError::Decode)?;
    let live_signature: &[u8; 65] =
        bytes[live_start + 48..deployment_start].try_into().map_err(|_| ProducerError::Decode)?;
    let deployment_signature: &[u8; 65] = bytes[deployment_start + 156..content_hash_start]
        .try_into()
        .map_err(|_| ProducerError::Decode)?;
    let outer_signature: &[u8; 65] =
        bytes[outer_signature_start..].try_into().map_err(|_| ProducerError::Decode)?;
    for signature in [g7_signature, live_signature, deployment_signature, outer_signature] {
        verify_signature_shape(signature).map_err(|_| ProducerError::Signature)?;
    }
    if bytes[g7_start..g7_start + 32] != bytes[live_start..live_start + 32]
        || bytes[g7_start..g7_start + 32].iter().all(|byte| *byte == 0)
        || u64::from_be_bytes(
            bytes[deployment_start..deployment_start + 8]
                .try_into()
                .map_err(|_| ProducerError::Decode)?,
        ) != BASE_CHAIN_ID
    {
        return Err(ProducerError::Identity);
    }

    let mut g7_preimage = Vec::with_capacity(80);
    g7_preimage.extend_from_slice(keccak256(G7_DOMAIN).as_slice());
    g7_preimage.extend_from_slice(&bytes[g7_start..g7_start + 48]);
    verify_canonical_signature(g7_signature, &g7_preimage).map_err(|_| ProducerError::Signature)?;

    let mut live_preimage = Vec::with_capacity(80);
    live_preimage.extend_from_slice(keccak256(LIVE_DOMAIN).as_slice());
    live_preimage.extend_from_slice(&bytes[live_start..live_start + 48]);
    verify_canonical_signature(live_signature, &live_preimage)
        .map_err(|_| ProducerError::Signature)?;

    let mut deployment_preimage = Vec::with_capacity(188);
    deployment_preimage.extend_from_slice(keccak256(DEPLOYMENT_DOMAIN).as_slice());
    deployment_preimage.extend_from_slice(&bytes[deployment_start..deployment_start + 156]);
    verify_canonical_signature(deployment_signature, &deployment_preimage)
        .map_err(|_| ProducerError::Signature)?;

    let mut hash_preimage = Vec::with_capacity(40 + 96);
    hash_preimage.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
    hash_preimage.extend_from_slice(&bytes[30..40]);
    hash_preimage.extend_from_slice(keccak256(&bytes[g7_start..live_start]).as_slice());
    hash_preimage.extend_from_slice(keccak256(&bytes[live_start..deployment_start]).as_slice());
    hash_preimage
        .extend_from_slice(keccak256(&bytes[deployment_start..content_hash_start]).as_slice());
    let content_hash = keccak256(hash_preimage);
    if content_hash.as_slice() != &bytes[content_hash_start..outer_signature_start] {
        return Err(ProducerError::Canonicality);
    }
    let mut outer_preimage = Vec::with_capacity(INSTALL_BUNDLE_DOMAIN.len() + 32);
    outer_preimage.extend_from_slice(INSTALL_BUNDLE_DOMAIN);
    outer_preimage.extend_from_slice(content_hash.as_slice());
    verify_canonical_signature(outer_signature, &outer_preimage)
        .map_err(|_| ProducerError::Signature)
}

fn publish_population_at(
    final_path: &Path,
    canonical: Vec<u8>,
) -> Result<PublishedPopulationManifestV1, ProducerError> {
    let parent = checked_parent(final_path)?;
    let final_name =
        final_path.file_name().and_then(|name| name.to_str()).ok_or(ProducerError::UnsafeObject)?;
    inventory(&parent, final_name, &[])?;
    let directory_path = proc_fd_path(&parent);
    let final_handle_path = directory_path.join(final_name);
    let temp = directory_path.join(format!("{final_name}.open"));
    match read_bounded(&final_handle_path, MAX_POPULATION_MANIFEST_BYTES) {
        Ok(existing) if existing == canonical => {
            return Ok(PublishedPopulationManifestV1 { canonical });
        }
        Ok(_) => return Err(ProducerError::Identity),
        Err(ProducerError::Io(PublicationIoClass::Open)) => {}
        Err(error) => return Err(error),
    }

    let mut file = open_new(&temp)?;
    file.write_all(&canonical).map_err(|_| ProducerError::Io(PublicationIoClass::Write))?;
    file.sync_all().map_err(|_| ProducerError::Io(PublicationIoClass::FileSync))?;
    validate_final(&file, canonical.len())?;
    match std::fs::hard_link(&temp, &final_handle_path) {
        Ok(()) => {}
        Err(_) => {
            let existing = read_bounded(&final_handle_path, MAX_POPULATION_MANIFEST_BYTES)?;
            if existing != canonical {
                return Err(ProducerError::Identity);
            }
        }
    }
    std::fs::remove_file(&temp).map_err(|_| ProducerError::Io(PublicationIoClass::Rename))?;
    sync_directory(&parent)?;
    let existing = read_bounded(&final_handle_path, MAX_POPULATION_MANIFEST_BYTES)?;
    if existing != canonical {
        return Err(ProducerError::Identity);
    }
    Ok(PublishedPopulationManifestV1 { canonical })
}

fn publish_replace_at(
    final_path: &Path,
    canonical: &[u8],
    allowed_existing: &[&str],
) -> Result<(), ProducerError> {
    let parent = checked_parent(final_path)?;
    let final_name =
        final_path.file_name().and_then(|name| name.to_str()).ok_or(ProducerError::UnsafeObject)?;
    inventory(&parent, final_name, allowed_existing)?;
    let directory_path = proc_fd_path(&parent);
    let final_handle_path = directory_path.join(final_name);
    let temp = directory_path.join(format!("{final_name}.open"));
    let mut file = open_new(&temp)?;
    file.write_all(canonical).map_err(|_| ProducerError::Io(PublicationIoClass::Write))?;
    file.sync_all().map_err(|_| ProducerError::Io(PublicationIoClass::FileSync))?;
    validate_final(&file, canonical.len())?;
    std::fs::rename(&temp, &final_handle_path)
        .map_err(|_| ProducerError::Io(PublicationIoClass::Rename))?;
    sync_directory(&parent)?;
    let existing = read_bounded(&final_handle_path, canonical.len())?;
    if existing != canonical {
        return Err(ProducerError::Identity);
    }
    Ok(())
}

#[cfg(not(test))]
fn checked_parent(path: &Path) -> Result<File, ProducerError> {
    let parent = path.parent().ok_or(ProducerError::UnsafeObject)?;
    super::settled_loss::open_directory_no_follow(parent).map_err(map_consumer_io)
}

#[cfg(feature = "arm-provisioning")]
fn prepare_private_directory(path: &Path) -> Result<(), ProducerError> {
    if !path.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| ProducerError::Io(PublicationIoClass::Open))?;
    }
    super::settled_loss::open_directory_no_follow(path).map_err(map_consumer_io)?;
    Ok(())
}

#[cfg(test)]
fn checked_parent(path: &Path) -> Result<File, ProducerError> {
    let parent = path.parent().ok_or(ProducerError::UnsafeObject)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|_| ProducerError::Io(PublicationIoClass::Open))?;
    let metadata = file.metadata().map_err(|_| ProducerError::Io(PublicationIoClass::Metadata))?;
    if metadata.uid() != effective_uid()? || metadata.mode() & 0o777 != 0o700 {
        return Err(ProducerError::UnsafeObject);
    }
    Ok(file)
}

fn inventory(
    parent: &File,
    final_name: &str,
    allowed_existing: &[&str],
) -> Result<(), ProducerError> {
    let temp_name = format!("{final_name}.open");
    for entry in std::fs::read_dir(proc_fd_path(parent))
        .map_err(|_| ProducerError::Io(PublicationIoClass::Inventory))?
    {
        let entry = entry.map_err(|_| ProducerError::Io(PublicationIoClass::Inventory))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ProducerError::Io(PublicationIoClass::Inventory))?;
        if name == temp_name || (name != final_name && !allowed_existing.contains(&name)) {
            return Err(ProducerError::Io(PublicationIoClass::Inventory));
        }
    }
    Ok(())
}

fn open_new(path: &Path) -> Result<File, ProducerError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ProducerError::Io(PublicationIoClass::Open))
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>, ProducerError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ProducerError::Io(PublicationIoClass::Open))?;
    let metadata = file.metadata().map_err(|_| ProducerError::Io(PublicationIoClass::Metadata))?;
    let length = usize::try_from(metadata.len()).map_err(|_| ProducerError::Bounds)?;
    if length == 0 || length > maximum {
        return Err(ProducerError::Bounds);
    }
    validate_final(&file, length)?;
    let mut canonical = vec![0u8; length];
    file.read_exact(&mut canonical).map_err(|_| ProducerError::Io(PublicationIoClass::Open))?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing).map_err(|_| ProducerError::Io(PublicationIoClass::Open))? != 0 {
        return Err(ProducerError::Bounds);
    }
    Ok(canonical)
}

fn validate_final(file: &File, expected_length: usize) -> Result<(), ProducerError> {
    let metadata = file.metadata().map_err(|_| ProducerError::Io(PublicationIoClass::Metadata))?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()?
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != expected_length as u64
    {
        return Err(ProducerError::UnsafeObject);
    }
    Ok(())
}

fn sync_directory(parent: &File) -> Result<(), ProducerError> {
    parent.sync_all().map_err(|_| ProducerError::Io(PublicationIoClass::DirectorySync))
}

fn map_consumer_io(reason: super::settled_loss::SettledLossUnavailableReason) -> ProducerError {
    match reason {
        super::settled_loss::SettledLossUnavailableReason::Missing => {
            ProducerError::Io(PublicationIoClass::Open)
        }
        super::settled_loss::SettledLossUnavailableReason::Incomplete
        | super::settled_loss::SettledLossUnavailableReason::Unresolved(_)
        | super::settled_loss::SettledLossUnavailableReason::Stale
        | super::settled_loss::SettledLossUnavailableReason::ManifestMismatch
        | super::settled_loss::SettledLossUnavailableReason::FinalityUnavailable
        | super::settled_loss::SettledLossUnavailableReason::CanonicalMismatch(_)
        | super::settled_loss::SettledLossUnavailableReason::Malformed
        | super::settled_loss::SettledLossUnavailableReason::AuthenticationFailed
        | super::settled_loss::SettledLossUnavailableReason::Rollback
        | super::settled_loss::SettledLossUnavailableReason::Io => ProducerError::UnsafeObject,
    }
}

fn effective_uid() -> Result<u32, ProducerError> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|_| ProducerError::Io(PublicationIoClass::Metadata))?;
    status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .and_then(|line| line.split_ascii_whitespace().nth(2))
        .and_then(|value| value.parse().ok())
        .ok_or(ProducerError::Io(PublicationIoClass::Metadata))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn private_directory() -> std::path::PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
        let path =
            std::env::temp_dir().join(format!("t4e-producer-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create private directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("private mode");
        path
    }

    fn closure() -> PopulationClosureFieldsV1 {
        PopulationClosureFieldsV1::new(
            B256::repeat_byte(0x11),
            BASE_CHAIN_ID,
            100,
            200,
            7,
            9,
            B256::repeat_byte(0x22),
            12,
        )
    }

    fn row(id: &[u8], submitted_at: u64) -> SourceLedgerRowV1 {
        SourceLedgerRowV1::new(
            BoundedSubmissionIdV1::new(id.to_vec()).expect("bounded id"),
            BASE_CHAIN_ID,
            B256::repeat_byte(0x33),
            B256::repeat_byte(0x44),
            submitted_at,
        )
    }

    fn canonical_pairs(
        live_campaign: B256,
    ) -> (CanonicalG7PairV1, CanonicalLivePairV1, CanonicalDeploymentPairV1) {
        let campaign = B256::repeat_byte(0x11);
        let mut g7 = [0u8; G7_PAIR_BYTES];
        g7[..32].copy_from_slice(campaign.as_slice());
        let mut live = [0u8; LIVE_PAIR_BYTES];
        live[..32].copy_from_slice(live_campaign.as_slice());
        let mut deployment = [0u8; DEPLOYMENT_PAIR_BYTES];
        deployment[..8].copy_from_slice(&BASE_CHAIN_ID.to_be_bytes());
        (
            CanonicalG7PairV1 { canonical: g7 },
            CanonicalLivePairV1 { canonical: live },
            CanonicalDeploymentPairV1 { canonical: deployment },
        )
    }

    #[test]
    fn install_bundle_staging_is_fixed_order_and_mixed_generation_resistant() {
        let campaign = B256::repeat_byte(0x11);
        let (g7, live, deployment) = canonical_pairs(campaign);
        let unsigned = ProducerConformance::prepare_install_bundle(9, g7, live, deployment)
            .expect("prepare bundle");
        assert_eq!(unsigned.canonical_body().len(), INSTALL_BUNDLE_BYTES - 65);
        assert_eq!(unsigned.outer_signature_preimage().len(), INSTALL_BUNDLE_DOMAIN.len() + 32);
        assert_eq!(&unsigned.canonical_body()[..30], INSTALL_BUNDLE_DOMAIN);
        assert_eq!(&unsigned.canonical_body()[32..40], &9u64.to_be_bytes());
        assert_eq!(
            &unsigned.canonical_body()[487..519],
            &unsigned.outer_signature_preimage()[30..]
        );

        let (g7, live, deployment) = canonical_pairs(B256::repeat_byte(0x22));
        assert!(matches!(
            ProducerConformance::prepare_install_bundle(9, g7, live, deployment),
            Err(ProducerError::Identity)
        ));
        let (g7, live, deployment) = canonical_pairs(campaign);
        assert!(matches!(
            ProducerConformance::prepare_install_bundle(0, g7, live, deployment),
            Err(ProducerError::Identity)
        ));
    }
    #[test]
    fn frozen_population_derives_exact_canonical_identities() {
        let unsigned = ProducerConformance::prepare_frozen_manifest(
            vec![row(b"submission-a", 120)],
            closure(),
        )
        .expect("prepare manifest");
        let bytes = unsigned.canonical_preimage();
        assert_eq!(bytes.len(), 191 + super::super::settled_loss::SOURCE_ENTRY_BYTES);
        assert_eq!(&bytes[..33], super::super::settled_loss::POPULATION_CLOSURE_DOMAIN);
        assert_eq!(&bytes[147..155], &1u64.to_be_bytes());
        assert_eq!(&bytes[187..191], &1u32.to_be_bytes());
        assert_eq!(&bytes[191..199], &0u64.to_be_bytes());

        let mut id_preimage = Vec::new();
        id_preimage.extend_from_slice(b"base-mev/p2-submission-id/v1");
        id_preimage.extend_from_slice(&12u32.to_be_bytes());
        id_preimage.extend_from_slice(b"submission-a");
        assert_eq!(&bytes[199..231], keccak256(id_preimage).as_slice());
        assert_eq!(bytes[327], 1);
        assert_eq!(&bytes[328..360], B256::repeat_byte(0x44).as_slice());
        assert_eq!(&bytes[155..187], keccak256(&bytes[191..]).as_slice());
    }

    #[test]
    fn frozen_population_rejects_unbounded_unordered_and_out_of_window_rows() {
        assert_eq!(BoundedSubmissionIdV1::new(Vec::new()), Err(ProducerError::Bounds));
        assert_eq!(BoundedSubmissionIdV1::new(vec![0xff]), Err(ProducerError::Bounds));
        assert!(matches!(
            ProducerConformance::prepare_frozen_manifest(
                vec![row(b"later", 120), row(b"earlier", 110)],
                closure(),
            ),
            Err(ProducerError::Order)
        ));
        assert!(matches!(
            ProducerConformance::prepare_frozen_manifest(vec![row(b"outside", 200)], closure()),
            Err(ProducerError::Identity)
        ));
    }

    #[test]
    fn terminal_projection_uses_single_canonical_encoder_and_external_signature() {
        use super::super::{
            settled_loss::{SETTLED_LOSS_DOMAIN, TerminalKindV1, UnresolvedReasonV1},
            testkit::{eip191_sign, owner_key},
        };
        use alloy_primitives::U256;

        let source = SourceSubmissionManifestEntryV1::new(
            0,
            B256::repeat_byte(0x10),
            B256::repeat_byte(0x20),
            B256::repeat_byte(0x30),
            B256::repeat_byte(0x40),
            B256::repeat_byte(0x40),
        );
        let mut source_bytes = Vec::new();
        source_bytes.extend_from_slice(&0u64.to_be_bytes());
        source_bytes.extend_from_slice(B256::repeat_byte(0x10).as_slice());
        source_bytes.extend_from_slice(B256::repeat_byte(0x20).as_slice());
        source_bytes.extend_from_slice(B256::repeat_byte(0x30).as_slice());
        source_bytes.extend_from_slice(B256::repeat_byte(0x40).as_slice());
        source_bytes.push(1);
        source_bytes.extend_from_slice(B256::repeat_byte(0x40).as_slice());
        let terminal = TerminalSettlementEntryV1::new(
            0,
            B256::repeat_byte(0x10),
            B256::repeat_byte(0x30),
            B256::repeat_byte(0x40),
            B256::repeat_byte(0x40),
            TerminalKindV1::Successful,
            UnresolvedReasonV1::None,
            1_001,
            B256::repeat_byte(0x50),
            U256::from(6),
            U256::from(1),
            U256::from(2),
            U256::from(30),
            U256::ZERO,
            U256::from(39),
            U256::ZERO,
        );
        let fields = ProjectionClosureFieldsV1::new(
            B256::repeat_byte(0x11),
            BASE_CHAIN_ID,
            100,
            200,
            7,
            9,
            B256::repeat_byte(0x22),
            12,
            1,
            keccak256(source_bytes),
            [0x22; 65],
            1_100,
            B256::repeat_byte(0x60),
            B256::ZERO,
        );
        let unsigned = ProducerConformance::prepare_terminal_projection(
            vec![source],
            vec![terminal],
            fields,
        )
        .expect("prepare projection");
        assert_eq!(unsigned.signature_preimage().len(), SETTLED_LOSS_DOMAIN.len() + 32);
        assert_eq!(&unsigned.signature_preimage()[..SETTLED_LOSS_DOMAIN.len()], SETTLED_LOSS_DOMAIN);
        assert_eq!(
            &unsigned.canonical_body()[unsigned.canonical_body().len() - 32..],
            &unsigned.signature_preimage()[SETTLED_LOSS_DOMAIN.len()..],
        );

        let signature = eip191_sign(unsigned.signature_preimage(), &owner_key());
        let signed = ProducerConformance::attach_projection_signature(unsigned, signature)
            .expect("attach projection signature");
        assert_eq!(
            signed.canonical_bytes().len(),
            664 + super::super::settled_loss::SOURCE_ENTRY_BYTES
                + super::super::settled_loss::TERMINAL_ENTRY_BYTES,
        );
    }
    #[test]
    fn population_publication_is_no_replace_and_exactly_idempotent() {
        let directory = private_directory();
        let path = directory.join("manifest.bin");
        let first = vec![7u8; 32];
        let published = publish_population_at(&path, first.clone()).expect("first publication");
        assert_eq!(published.canonical_bytes(), first);
        publish_population_at(&path, first).expect("idempotent publication");
        assert!(matches!(
            publish_population_at(&path, vec![8u8; 32]),
            Err(ProducerError::Identity)
        ));
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn successor_publication_replaces_only_after_complete_file_sync() {
        let directory = private_directory();
        let path = directory.join("projection.bin");
        publish_replace_at(&path, &[1u8; 32], &[]).expect("first projection");
        fs::write(directory.join("accepted-head"), b"anchor").expect("accepted head");
        publish_replace_at(&path, &[2u8; 32], &["accepted-head"]).expect("successor projection");
        assert_eq!(fs::read(&path).expect("published bytes"), vec![2u8; 32]);
        assert!(!directory.join("projection.bin.open").exists());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn final_symlink_is_never_followed_or_replaced() {
        let directory = private_directory();
        let target_directory = private_directory();
        let target = target_directory.join("target");
        fs::write(&target, b"target").expect("target");
        let path = directory.join("manifest.bin");
        symlink(&target, &path).expect("final symlink");
        assert_eq!(
            publish_population_at(&path, vec![7u8; 32]).map(|_| ()),
            Err(ProducerError::Io(PublicationIoClass::Open))
        );
        assert_eq!(fs::read(&target).expect("unchanged target"), b"target");
        fs::remove_dir_all(directory).expect("cleanup");
        fs::remove_dir_all(target_directory).expect("target cleanup");
    }
    #[test]
    fn stale_open_and_unknown_inventory_close_publication() {
        let directory = private_directory();
        let path = directory.join("projection.bin");
        fs::write(directory.join("projection.bin.open"), b"stale").expect("stale temp");
        assert_eq!(
            publish_replace_at(&path, &[1u8; 32], &[]),
            Err(ProducerError::Io(PublicationIoClass::Inventory))
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
