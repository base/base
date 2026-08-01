//! Raw-free, owner-gated, content-addressed state fixture capture DTOs.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const STATE_FIXTURE_CAPTURE_SCHEMA_V1: &str = "base-mev/state-fixture-capture/v1";
const MAX_CAPTURE_ACCOUNTS_V1: usize = 256;
const MAX_CAPTURE_STORAGE_V1: usize = 8_192;
const MAX_CAPTURE_AUDIT_READS_V1: usize = 8_192;
const MAX_CAPTURE_CODE_BYTES_V1: usize = 4 * 1024 * 1024;
const MAX_CAPTURE_ENCODED_BYTES_V1: usize = 10 * 1024 * 1024;
const CAPTURE_ENCODED_FIXED_BYTES_V1: usize = 8 * 1024;
const CAPTURE_ENCODED_ACCOUNT_BYTES_V1: usize = 512;
const CAPTURE_ENCODED_STORAGE_BYTES_V1: usize = 320;
const CAPTURE_ENCODED_AUDIT_READ_BYTES_V1: usize = 384;
const CAPTURE_ENCODED_CODE_EXPANSION_V1: usize = 2;
const CAPTURE_PROVENANCE_MASK_V1: u8 = 0b1111;

/// Explicit owner switch and reviewed canonical root for capture publication.
#[derive(Debug, PartialEq, Eq)]
pub struct StateFixtureCaptureConfigV1 {
    owner_enabled: bool,
    canonical_root: PathBuf,
}

impl StateFixtureCaptureConfigV1 {
    /// Validates an explicit owner decision against the independently reviewed canonical root.
    pub fn new(
        owner_enabled: bool,
        requested_root: PathBuf,
        reviewed_canonical_root: PathBuf,
    ) -> Result<Self, ErrorV1> {
        if !Self::lexically_safe_absolute(&requested_root)
            || !Self::lexically_safe_absolute(&reviewed_canonical_root)
        {
            return Err(ErrorV1::InvalidCanonicalRoot);
        }
        let canonical_root = fs::canonicalize(&reviewed_canonical_root).map_err(|_| ErrorV1::Io)?;
        let requested = fs::canonicalize(&requested_root).map_err(|_| ErrorV1::Io)?;
        if canonical_root != requested || !canonical_root.is_absolute() {
            return Err(ErrorV1::UnreviewedRoot);
        }
        Ok(Self { owner_enabled, canonical_root })
    }

    fn lexically_safe_absolute(path: &Path) -> bool {
        path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    }
}

/// Exact selected-route parts retained by an owned fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedPartsV1 {
    victim_tx_hash: B256,
    directed_key: B256,
    pools: [Address; 2],
    tokens: [Address; 3],
    adapters: [Address; 2],
    sender: Address,
    executor: Address,
    recipient: Address,
    adapter_code_hashes: [B256; 2],
    hop_fees: [u32; 2],
    hop_zero_for_one: [bool; 2],
    amount_in_wei: U256,
    route_digest: B256,
    header_hash: B256,
    state_digest: B256,
    access_digest: B256,
}

impl SelectedPartsV1 {
    /// Creates exact route parts bound to the captured header, state, and audited access set.
    pub fn new(
        victim_tx_hash: B256,
        directed_key: B256,
        pools: [Address; 2],
        tokens: [Address; 3],
        adapters: [Address; 2],
        sender: Address,
        executor: Address,
        recipient: Address,
        adapter_code_hashes: [B256; 2],
        hop_fees: [u32; 2],
        hop_zero_for_one: [bool; 2],
        amount_in_wei: U256,
        route_digest: B256,
        header_hash: B256,
        state_digest: B256,
        access_digest: B256,
    ) -> Result<Self, ErrorV1> {
        if victim_tx_hash.is_zero()
            || directed_key.is_zero()
            || route_digest.is_zero()
            || header_hash.is_zero()
            || state_digest.is_zero()
            || access_digest.is_zero()
            || pools.iter().any(|address| address.is_zero())
            || tokens.iter().any(|address| address.is_zero())
            || adapters.iter().any(|address| address.is_zero())
            || sender.is_zero()
            || executor.is_zero()
            || recipient.is_zero()
            || adapter_code_hashes.iter().any(B256::is_zero)
            || hop_fees.contains(&0)
            || amount_in_wei.is_zero()
        {
            return Err(ErrorV1::InvalidDigest);
        }
        Ok(Self {
            victim_tx_hash,
            directed_key,
            pools,
            tokens,
            adapters,
            sender,
            executor,
            recipient,
            adapter_code_hashes,
            hop_fees,
            hop_zero_for_one,
            amount_in_wei,
            route_digest,
            header_hash,
            state_digest,
            access_digest,
        })
    }
}

/// One exact storage input and its audit provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageV1 {
    slot: U256,
    value: U256,
    provenance_bits: u8,
    first_ordinal: u64,
    last_ordinal: u64,
    occurrences: u64,
}

impl StorageV1 {
    /// Creates one canonical storage input without collapsing a numeric zero value.
    pub fn new(
        slot: U256,
        value: U256,
        provenance_bits: u8,
        first_ordinal: u64,
        last_ordinal: u64,
        occurrences: u64,
    ) -> Result<Self, ErrorV1> {
        if provenance_bits == 0
            || provenance_bits & !CAPTURE_PROVENANCE_MASK_V1 != 0
            || occurrences == 0
            || first_ordinal > last_ordinal
            || last_ordinal.checked_sub(first_ordinal).is_none()
        {
            return Err(ErrorV1::InvalidProvenance);
        }
        Ok(Self { slot, value, provenance_bits, first_ordinal, last_ordinal, occurrences })
    }
}

/// Phase in which an owned audit observation was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditPhaseV1 {
    /// Recipient WETH balance before candidate execution.
    PreWeth,
    /// Candidate EVM execution.
    Candidate,
    /// Recipient WETH balance after candidate execution.
    PostWeth,
    /// Canonical L1 fee input fetch.
    L1Fetch,
    /// Terminal audit phase.
    Sealed,
}

/// Database operation that produced an owned audit observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditAccessKindV1 {
    /// Account lookup.
    Basic,
    /// Bytecode lookup by hash.
    CodeByHash,
    /// Storage lookup.
    Storage,
    /// Block hash lookup.
    BlockHash,
}

/// Typed result returned by one audited database operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditObservedValueV1 {
    /// The account did not exist.
    AbsentAccount,
    /// Complete account result, preserving optional hydrated code.
    Account {
        /// Account balance.
        balance: U256,
        /// Account nonce.
        nonce: u64,
        /// Account code hash.
        code_hash: B256,
        /// Hydrated code when it was present on the observed account.
        code: Option<Bytes>,
    },
    /// Returned bytecode.
    Code(Bytes),
    /// Returned storage word.
    Storage(U256),
    /// Returned block hash.
    BlockHash(B256),
}

/// One canonical phase-aware audited read and its occurrence provenance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditReadV1 {
    phase: AuditPhaseV1,
    access_kind: AuditAccessKindV1,
    address: Option<Address>,
    account_identity: Option<u32>,
    slot: Option<U256>,
    code_hash: Option<B256>,
    block_number: Option<u64>,
    observed_value: AuditObservedValueV1,
    first_ordinal: u64,
    last_ordinal: u64,
    occurrences: u64,
}

impl AuditReadV1 {
    /// Creates one fully typed audited read after validating its kind-specific identity.
    pub fn new(
        phase: AuditPhaseV1,
        access_kind: AuditAccessKindV1,
        address: Option<Address>,
        account_identity: Option<u32>,
        slot: Option<U256>,
        code_hash: Option<B256>,
        block_number: Option<u64>,
        observed_value: AuditObservedValueV1,
        first_ordinal: u64,
        last_ordinal: u64,
        occurrences: u64,
    ) -> Result<Self, ErrorV1> {
        let identity_is_valid = match (&access_kind, &observed_value) {
            (
                AuditAccessKindV1::Basic,
                AuditObservedValueV1::AbsentAccount | AuditObservedValueV1::Account { .. },
            ) => {
                address.is_some() && slot.is_none() && code_hash.is_none() && block_number.is_none()
            }
            (AuditAccessKindV1::CodeByHash, AuditObservedValueV1::Code(_)) => {
                address.is_none()
                    && account_identity.is_none()
                    && slot.is_none()
                    && code_hash.is_some_and(|hash| !hash.is_zero())
                    && block_number.is_none()
            }
            (AuditAccessKindV1::Storage, AuditObservedValueV1::Storage(_)) => {
                address.is_some() && slot.is_some() && code_hash.is_none() && block_number.is_none()
            }
            (AuditAccessKindV1::BlockHash, AuditObservedValueV1::BlockHash(_)) => {
                address.is_none()
                    && account_identity.is_none()
                    && slot.is_none()
                    && code_hash.is_none()
                    && block_number.is_some()
            }
            _ => false,
        };
        let absent_account_has_identity =
            matches!(&observed_value, AuditObservedValueV1::AbsentAccount)
                && account_identity.is_some();
        if !identity_is_valid
            || absent_account_has_identity
            || address.is_some_and(|value| value.is_zero())
            || occurrences == 0
            || first_ordinal > last_ordinal
            || last_ordinal
                .checked_sub(first_ordinal)
                .and_then(|span| span.checked_add(1))
                .is_none_or(|span| occurrences > span)
        {
            return Err(ErrorV1::InvalidAuditRead);
        }
        Ok(Self {
            phase,
            access_kind,
            address,
            account_identity,
            slot,
            code_hash,
            block_number,
            observed_value,
            first_ordinal,
            last_ordinal,
            occurrences,
        })
    }
}

/// One self-contained account input, including exact code and storage values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountV1 {
    address: Address,
    exists: bool,
    balance: U256,
    nonce: u64,
    code_hash: B256,
    code: Bytes,
    storage: Vec<StorageV1>,
    provenance_bits: u8,
}

impl AccountV1 {
    /// Creates a bounded, internally consistent account capture.
    pub fn new(
        address: Address,
        exists: bool,
        balance: U256,
        nonce: u64,
        code_hash: B256,
        code: Bytes,
        storage: Vec<StorageV1>,
        provenance_bits: u8,
    ) -> Result<Self, ErrorV1> {
        let canonical_code_hash = keccak256(code.as_ref());
        if address.is_zero()
            || provenance_bits == 0
            || provenance_bits & !CAPTURE_PROVENANCE_MASK_V1 != 0
            || storage.len() > MAX_CAPTURE_STORAGE_V1
            || code.len() > MAX_CAPTURE_CODE_BYTES_V1
            || code_hash != canonical_code_hash
            || (!exists
                && (!balance.is_zero()
                    || nonce != 0
                    || !code.is_empty()
                    || code_hash != keccak256([])
                    || !storage.is_empty()))
        {
            return Err(ErrorV1::InvalidAccount);
        }
        let mut prior = None;
        for entry in &storage {
            if prior.is_some_and(|slot| slot >= entry.slot) {
                return Err(ErrorV1::NonCanonicalOrdering);
            }
            prior = Some(entry.slot);
        }
        Ok(Self { address, exists, balance, nonce, code_hash, code, storage, provenance_bits })
    }
}

/// Fully owned, sealed fixture input; it contains no provider or transaction-byte borrow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputV1 {
    schema_version: &'static str,
    chain_id: u64,
    block_number: u64,
    block_hash: B256,
    parent_hash: B256,
    timestamp: u64,
    gas_limit: u64,
    coinbase: Address,
    base_fee_per_gas: u64,
    prev_randao: B256,
    excess_blob_gas: Option<u64>,
    recipient_weth_address: Address,
    recipient_weth_recipient: Address,
    recipient_weth_slot: U256,
    recipient_weth_pre: U256,
    recipient_weth_post: U256,
    canonical_l1_digest: B256,
    canonical_l1_fee_wei: U256,
    header_identity_digest: B256,
    selected: SelectedPartsV1,
    accounts: Vec<AccountV1>,
    audit_reads: Vec<AuditReadV1>,
    audit_digest: B256,
    economics_evidence_digest: B256,
}

impl InputV1 {
    /// Seals a self-contained, canonically ordered capture DTO after candidate execution and L1 work.
    pub fn seal(
        chain_id: u64,
        block_number: u64,
        block_hash: B256,
        parent_hash: B256,
        timestamp: u64,
        gas_limit: u64,
        coinbase: Address,
        base_fee_per_gas: u64,
        prev_randao: B256,
        excess_blob_gas: Option<u64>,
        recipient_weth_address: Address,
        recipient_weth_recipient: Address,
        recipient_weth_slot: U256,
        recipient_weth_pre: U256,
        recipient_weth_post: U256,
        canonical_l1_digest: B256,
        canonical_l1_fee_wei: U256,
        header_identity_digest: B256,
        selected: SelectedPartsV1,
        accounts: Vec<AccountV1>,
        audit_reads: Vec<AuditReadV1>,
        audit_digest: B256,
        economics_evidence_digest: B256,
    ) -> Result<Self, ErrorV1> {
        if chain_id == 0
            || block_hash.is_zero()
            || parent_hash.is_zero()
            || prev_randao.is_zero()
            || gas_limit == 0
            || coinbase.is_zero()
            || recipient_weth_address.is_zero()
            || recipient_weth_recipient != selected.recipient
            || header_identity_digest.is_zero()
            || audit_digest.is_zero()
            || recipient_weth_post.checked_sub(recipient_weth_pre).is_none()
            || canonical_l1_digest.is_zero()
            || economics_evidence_digest.is_zero()
            || accounts.is_empty()
            || accounts.len() > MAX_CAPTURE_ACCOUNTS_V1
            || audit_reads.is_empty()
            || audit_reads.len() > MAX_CAPTURE_AUDIT_READS_V1
        {
            return Err(ErrorV1::InvalidInput);
        }
        let mut prior = None;
        let mut aggregate_storage = 0usize;
        let mut aggregate_code_bytes = 0usize;
        let mut conservative_encoded_bytes = CAPTURE_ENCODED_FIXED_BYTES_V1;
        conservative_encoded_bytes = conservative_encoded_bytes
            .checked_add(
                audit_reads
                    .len()
                    .checked_mul(CAPTURE_ENCODED_AUDIT_READ_BYTES_V1)
                    .ok_or(ErrorV1::InvalidInput)?,
            )
            .ok_or(ErrorV1::InvalidInput)?;
        let mut prior_audit_read = None;
        let mut aggregate_audit_occurrences = 0u64;
        let mut pre_weth_reads = 0usize;
        let mut post_weth_reads = 0usize;
        let mut candidate_reads = 0usize;
        let mut l1_fetch_reads = 0usize;
        for read in &audit_reads {
            match read.phase {
                AuditPhaseV1::PreWeth => {
                    pre_weth_reads += 1;
                    if read.access_kind != AuditAccessKindV1::Storage
                        || read.address != Some(recipient_weth_address)
                        || read.slot != Some(recipient_weth_slot)
                        || read.observed_value != AuditObservedValueV1::Storage(recipient_weth_pre)
                        || read.first_ordinal != read.last_ordinal
                        || read.occurrences != 1
                    {
                        return Err(ErrorV1::InvalidInput);
                    }
                }
                AuditPhaseV1::Candidate => candidate_reads += 1,
                AuditPhaseV1::PostWeth => {
                    post_weth_reads += 1;
                    if read.access_kind != AuditAccessKindV1::Storage
                        || read.address != Some(recipient_weth_address)
                        || read.slot != Some(recipient_weth_slot)
                        || read.observed_value != AuditObservedValueV1::Storage(recipient_weth_post)
                        || read.first_ordinal != read.last_ordinal
                        || read.occurrences != 1
                    {
                        return Err(ErrorV1::InvalidInput);
                    }
                }
                AuditPhaseV1::L1Fetch => l1_fetch_reads += 1,
                AuditPhaseV1::Sealed => return Err(ErrorV1::InvalidInput),
            }
            if prior_audit_read.is_some_and(|prior: &AuditReadV1| prior >= read) {
                return Err(ErrorV1::NonCanonicalOrdering);
            }
            aggregate_audit_occurrences = aggregate_audit_occurrences
                .checked_add(read.occurrences)
                .ok_or(ErrorV1::InvalidInput)?;
            if aggregate_audit_occurrences
                > u64::try_from(MAX_CAPTURE_AUDIT_READS_V1).map_err(|_| ErrorV1::InvalidInput)?
            {
                return Err(ErrorV1::InvalidInput);
            }
            let observed_code_bytes = match &read.observed_value {
                AuditObservedValueV1::Account { code: Some(code), .. }
                | AuditObservedValueV1::Code(code) => code.len(),
                _ => 0,
            };
            aggregate_code_bytes = aggregate_code_bytes
                .checked_add(observed_code_bytes)
                .ok_or(ErrorV1::InvalidInput)?;
            conservative_encoded_bytes = conservative_encoded_bytes
                .checked_add(
                    observed_code_bytes
                        .checked_mul(CAPTURE_ENCODED_CODE_EXPANSION_V1)
                        .ok_or(ErrorV1::InvalidInput)?,
                )
                .ok_or(ErrorV1::InvalidInput)?;
            prior_audit_read = Some(read);
        }
        if pre_weth_reads != 1
            || post_weth_reads != 1
            || candidate_reads == 0
            || l1_fetch_reads == 0
        {
            return Err(ErrorV1::InvalidInput);
        }
        if aggregate_code_bytes > MAX_CAPTURE_CODE_BYTES_V1
            || conservative_encoded_bytes > MAX_CAPTURE_ENCODED_BYTES_V1
        {
            return Err(ErrorV1::InvalidInput);
        }
        for account in &accounts {
            aggregate_storage = aggregate_storage
                .checked_add(account.storage.len())
                .ok_or(ErrorV1::InvalidInput)?;
            aggregate_code_bytes = aggregate_code_bytes
                .checked_add(account.code.len())
                .ok_or(ErrorV1::InvalidInput)?;
            conservative_encoded_bytes = conservative_encoded_bytes
                .checked_add(CAPTURE_ENCODED_ACCOUNT_BYTES_V1)
                .and_then(|bytes| {
                    account
                        .storage
                        .len()
                        .checked_mul(CAPTURE_ENCODED_STORAGE_BYTES_V1)
                        .and_then(|storage| bytes.checked_add(storage))
                })
                .and_then(|bytes| {
                    account
                        .code
                        .len()
                        .checked_mul(CAPTURE_ENCODED_CODE_EXPANSION_V1)
                        .and_then(|code| bytes.checked_add(code))
                })
                .ok_or(ErrorV1::InvalidInput)?;
            if aggregate_storage > MAX_CAPTURE_STORAGE_V1
                || aggregate_code_bytes > MAX_CAPTURE_CODE_BYTES_V1
                || conservative_encoded_bytes > MAX_CAPTURE_ENCODED_BYTES_V1
            {
                return Err(ErrorV1::InvalidInput);
            }
            if prior.is_some_and(|address| address >= account.address) {
                return Err(ErrorV1::NonCanonicalOrdering);
            }
            prior = Some(account.address);
        }
        Ok(Self {
            schema_version: STATE_FIXTURE_CAPTURE_SCHEMA_V1,
            chain_id,
            block_number,
            block_hash,
            parent_hash,
            timestamp,
            gas_limit,
            coinbase,
            base_fee_per_gas,
            prev_randao,
            excess_blob_gas,
            recipient_weth_address,
            recipient_weth_recipient,
            recipient_weth_slot,
            recipient_weth_pre,
            recipient_weth_post,
            canonical_l1_digest,
            canonical_l1_fee_wei,
            header_identity_digest,
            selected,
            accounts,
            audit_reads,
            audit_digest,
            economics_evidence_digest,
        })
    }

    /// Returns the byte-stability binding to immutable economics evidence.
    pub const fn economics_evidence_digest(&self) -> B256 {
        self.economics_evidence_digest
    }
}

/// Capture writer failure class without leaking filesystem paths or arbitrary provider strings.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorV1 {
    /// The configured root was not canonical and absolute.
    #[error("capture root is not canonical and absolute")]
    InvalidCanonicalRoot,
    /// The requested root did not equal the reviewed canonical root.
    #[error("capture root was not reviewed")]
    UnreviewedRoot,
    /// A mandatory digest used the forbidden zero sentinel.
    #[error("capture contains an invalid digest")]
    InvalidDigest,
    /// Audit provenance was internally inconsistent.
    #[error("capture provenance is invalid")]
    InvalidProvenance,
    /// A phase-aware audit read had an invalid identity, result, or occurrence range.
    #[error("capture audit read is invalid")]
    InvalidAuditRead,
    /// Account evidence exceeded bounds or was inconsistent.
    #[error("capture account is invalid")]
    InvalidAccount,
    /// The sealed root input was incomplete or exceeded bounds.
    #[error("capture input is invalid")]
    InvalidInput,
    /// Accounts, storage, or audit reads were not strictly canonically ordered.
    #[error("capture input is not canonically ordered")]
    NonCanonicalOrdering,
    /// Serialization failed before any file was created.
    #[error("capture serialization failed")]
    Serialization,
    /// A content-addressed file already existed; overwrite is forbidden.
    #[error("capture content already exists")]
    AlreadyExists,
    /// A filesystem operation failed.
    #[error("capture I/O failed")]
    Io,
    /// A failed publication could not remove its same-directory temporary file.
    #[error("capture cleanup failed")]
    CleanupFailed,
    /// The complete content-addressed file exists but directory durability is uncertain.
    #[error("capture directory durability is uncertain")]
    DurabilityUncertain,
}

/// Terminal outcome of the optional capture writer only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OutcomeV1 {
    /// Owner capture was explicitly disabled and no I/O was attempted.
    Disabled,
    /// A new content-addressed file was durably created.
    Written,
    /// Capture failed without changing economics evidence.
    Failed(ErrorV1),
}

/// Exactly-once capture writer counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountersV1 {
    writer_attempted: u64,
    files_created: u64,
    writer_failed: u64,
}

impl CountersV1 {
    const fn disabled() -> Self {
        Self { writer_attempted: 0, files_created: 0, writer_failed: 0 }
    }

    const fn written() -> Self {
        Self { writer_attempted: 1, files_created: 1, writer_failed: 0 }
    }

    const fn failed() -> Self {
        Self { writer_attempted: 1, files_created: 0, writer_failed: 1 }
    }

    /// Returns the number of writer calls.
    pub const fn writer_attempted(&self) -> u64 {
        self.writer_attempted
    }

    /// Returns the number of newly created files.
    pub const fn files_created(&self) -> u64 {
        self.files_created
    }
}

/// Complete capture disposition, including content identity and isolated counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptV1 {
    outcome: OutcomeV1,
    content_digest: Option<B256>,
    relative_name: Option<String>,
    bytes_written: Option<u64>,
    economics_evidence_digest: B256,
    counters: CountersV1,
}

impl ReceiptV1 {
    /// Returns the capture-only outcome.
    pub const fn outcome(&self) -> OutcomeV1 {
        self.outcome
    }

    /// Returns capture-only counters.
    pub const fn counters(&self) -> CountersV1 {
        self.counters
    }

    /// Returns the unchanged economics evidence binding.
    pub const fn economics_evidence_digest(&self) -> B256 {
        self.economics_evidence_digest
    }
}

/// Consuming, exactly-once, create-new content-addressed capture writer.
#[derive(Debug)]
pub struct WriterV1 {
    config: StateFixtureCaptureConfigV1,
}

impl WriterV1 {
    /// Creates a writer token. The token is intentionally neither cloneable nor reusable.
    pub const fn new(config: StateFixtureCaptureConfigV1) -> Self {
        Self { config }
    }

    /// Consumes the writer and attempts at most one create-new publication.
    pub fn write(self, input: InputV1) -> ReceiptV1 {
        let economics_evidence_digest = input.economics_evidence_digest;
        if !self.config.owner_enabled {
            return ReceiptV1 {
                outcome: OutcomeV1::Disabled,
                content_digest: None,
                relative_name: None,
                bytes_written: None,
                economics_evidence_digest,
                counters: CountersV1::disabled(),
            };
        }
        let encoded = match serde_json::to_vec(&input) {
            Ok(encoded) => encoded,
            Err(_) => return Self::failure(economics_evidence_digest, ErrorV1::Serialization),
        };
        let digest = B256::from_slice(Sha256::digest(&encoded).as_slice());
        let relative_name = format!("{}.json", digest);
        let temporary_name = format!(".{}.tmp", digest);
        let path = self.config.canonical_root.join(&relative_name);
        let temporary_path = self.config.canonical_root.join(&temporary_name);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temporary_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Self::failure(economics_evidence_digest, ErrorV1::AlreadyExists);
            }
            Err(_) => return Self::failure(economics_evidence_digest, ErrorV1::Io),
        };
        if file.write_all(&encoded).and_then(|()| file.sync_all()).is_err() {
            drop(file);
            return match fs::remove_file(&temporary_path) {
                Ok(()) => Self::failure(economics_evidence_digest, ErrorV1::Io),
                Err(_) => Self::failure_with_identity(
                    economics_evidence_digest,
                    ErrorV1::CleanupFailed,
                    digest,
                    temporary_name,
                    encoded.len(),
                ),
            };
        }
        drop(file);
        if let Err(error) = fs::hard_link(&temporary_path, &path) {
            return match fs::remove_file(&temporary_path) {
                Ok(()) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Self::failure(economics_evidence_digest, ErrorV1::AlreadyExists)
                }
                Ok(()) => Self::failure(economics_evidence_digest, ErrorV1::Io),
                Err(_) => Self::failure_with_identity(
                    economics_evidence_digest,
                    ErrorV1::CleanupFailed,
                    digest,
                    temporary_name,
                    encoded.len(),
                ),
            };
        }
        if fs::remove_file(&temporary_path).is_err() {
            return Self::failure_with_identity(
                economics_evidence_digest,
                ErrorV1::CleanupFailed,
                digest,
                relative_name,
                encoded.len(),
            );
        }
        if File::open(&self.config.canonical_root).and_then(|root| root.sync_all()).is_err() {
            return Self::failure_with_identity(
                economics_evidence_digest,
                ErrorV1::DurabilityUncertain,
                digest,
                relative_name,
                encoded.len(),
            );
        }
        ReceiptV1 {
            outcome: OutcomeV1::Written,
            content_digest: Some(digest),
            relative_name: Some(relative_name),
            bytes_written: u64::try_from(encoded.len()).ok(),
            economics_evidence_digest,
            counters: CountersV1::written(),
        }
    }

    fn failure(economics_evidence_digest: B256, error: ErrorV1) -> ReceiptV1 {
        ReceiptV1 {
            outcome: OutcomeV1::Failed(error),
            content_digest: None,
            relative_name: None,
            bytes_written: None,
            economics_evidence_digest,
            counters: CountersV1::failed(),
        }
    }

    fn failure_with_identity(
        economics_evidence_digest: B256,
        error: ErrorV1,
        content_digest: B256,
        relative_name: String,
        bytes_written: usize,
    ) -> ReceiptV1 {
        ReceiptV1 {
            outcome: OutcomeV1::Failed(error),
            content_digest: Some(content_digest),
            relative_name: Some(relative_name),
            bytes_written: u64::try_from(bytes_written).ok(),
            economics_evidence_digest,
            counters: CountersV1::failed(),
        }
    }
}

/// Publication outcome after the production economics finalizer has run.
#[derive(Debug, PartialEq, Eq)]
pub enum PublicationOutcomeV1<Finalized, FinalizeError, BuilderError> {
    /// Economics finalization rejected publication before capture construction or I/O.
    FinalizeRejected(FinalizeError),
    /// Execution-derived capture construction failed before I/O.
    BuilderFailed {
        /// Finalized economics retained for the normal handoff path.
        finalized: Finalized,
        /// Capture-only builder failure.
        error: BuilderError,
    },
    /// The optional writer ran and reported its capture-only disposition.
    WriterOutcome {
        /// Finalized economics retained for the normal handoff path.
        finalized: Finalized,
        /// Capture-only writer receipt.
        receipt: ReceiptV1,
    },
}

impl<Finalized, FinalizeError, BuilderError>
    PublicationOutcomeV1<Finalized, FinalizeError, BuilderError>
{
    /// Returns how many times the execution-derived builder was invoked.
    pub const fn builder_attempted(&self) -> u64 {
        match self {
            Self::FinalizeRejected(_) => 0,
            Self::BuilderFailed { .. } | Self::WriterOutcome { .. } => 1,
        }
    }

    /// Returns how many times the optional writer was invoked.
    pub const fn writer_attempted(&self) -> u64 {
        match self {
            Self::FinalizeRejected(_) | Self::BuilderFailed { .. } => 0,
            Self::WriterOutcome { receipt, .. } => receipt.counters.writer_attempted,
        }
    }

    /// Returns how many new capture files the writer created.
    pub const fn files_created(&self) -> u64 {
        match self {
            Self::FinalizeRejected(_) | Self::BuilderFailed { .. } => 0,
            Self::WriterOutcome { receipt, .. } => receipt.counters.files_created,
        }
    }

    /// Recovers finalized economics for the normal handoff path.
    pub fn into_finalized(self) -> Result<Finalized, FinalizeError> {
        match self {
            Self::FinalizeRejected(error) => Err(error),
            Self::BuilderFailed { finalized, .. } | Self::WriterOutcome { finalized, .. } => {
                Ok(finalized)
            }
        }
    }
}

/// Production ordering gate between economics finalization and optional capture publication.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublicationGateV1;

impl PublicationGateV1 {
    /// Consumes a finalize result, builder, and writer in strict finalize-build-write order.
    pub fn publish<Finalized, FinalizeError, BuilderError, Builder>(
        finalized: Result<Finalized, FinalizeError>,
        builder: Builder,
        writer: WriterV1,
    ) -> PublicationOutcomeV1<Finalized, FinalizeError, BuilderError>
    where
        Builder: FnOnce(&Finalized) -> Result<InputV1, BuilderError>,
    {
        let finalized = match finalized {
            Ok(finalized) => finalized,
            Err(error) => return PublicationOutcomeV1::FinalizeRejected(error),
        };
        let input = match builder(&finalized) {
            Ok(input) => input,
            Err(error) => return PublicationOutcomeV1::BuilderFailed { finalized, error },
        };
        PublicationOutcomeV1::WriterOutcome { finalized, receipt: writer.write(input) }
    }
}
