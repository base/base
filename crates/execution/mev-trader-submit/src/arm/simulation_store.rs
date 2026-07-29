//! Bounded, local-only persistence for inert simulation attempts.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use alloy_primitives::{B256, U256, keccak256};
use rand_08::{RngCore, rngs::OsRng};
use serde_json::{Value, json};

use super::{SimulationAttempt, SimulationRecord};
use crate::PriorityEconomicsReceipt;

/// Compile-pinned local simulation ledger directory.
pub const SIMULATION_LEDGER_PATH: &str = "/home/ubuntu/.local/state/base-mev/simulation-v1";
/// Maximum durable records in one ledger epoch.
pub const SIMULATION_RECORD_CAPACITY: u64 = 262_144;
/// Maximum canonical JSON bytes in one durable record.
pub const SIMULATION_RECORD_MAX_BYTES: usize = 16 * 1024;

const EPOCH_FILE: &str = "epoch";
const LEASE_FILE: &str = ".lease";
const HEAD_FILE: &str = "head";
const HEAD_OPEN_FILE: &str = ".head.open";
const VERSION: u64 = 1;

/// One random identity separating owner-rotated ledger epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationLedgerEpoch(B256);

impl SimulationLedgerEpoch {
    /// Returns epoch bytes.
    pub const fn value(self) -> B256 {
        self.0
    }
}

/// Typed failure while opening a ledger.
#[derive(Debug)]
pub enum SimulationStoreOpenError {
    /// Filesystem operation failed.
    Io(io::ErrorKind),
    /// Another process or handle owns the pinned ledger.
    AlreadyOpen,
    /// The lease database could not be opened because it is corrupt or otherwise unusable.
    Lease(redb::DatabaseError),
    /// Existing ledger structure is not classifiable as valid V1 data.
    InvalidExistingLedger(SimulationLedgerInvalid),
}

/// Closed structural classes for an existing ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationLedgerInvalid {
    /// Epoch metadata is absent or malformed.
    Epoch,
    /// Unknown directory entry is present.
    UnknownEntry,
    /// An unpublished record remains.
    StaleOpen,
    /// Published sequences are not contiguous.
    Sequence,
    /// A record exceeds the encoded bound.
    Oversize,
    /// Record JSON/schema is malformed or unknown.
    Schema,
    /// Prior-record hash does not match.
    HashChain,
    /// Published entry is not a single-link regular file.
    FileType,
}

/// Typed persistence terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationPersistError {
    /// Ledger reached its fixed campaign bound before irreversible candidate work.
    Full {
        /// Current epoch.
        epoch: SimulationLedgerEpoch,
        /// Rejected next sequence.
        next_sequence: u64,
        /// Fixed capacity.
        capacity: u64,
    },
    /// Simulation record lacked the checked economics receipt.
    MissingEconomics,
    /// Canonical encoding exceeded its fixed bound.
    Oversize,
    /// Simulation record lacked bounded T4e identity/route evidence.
    MissingIdentityEvidence,
    /// Durable publication failed at a classified operation.
    WriteFailed {
        /// Current epoch.
        epoch: SimulationLedgerEpoch,
        /// Sequence whose outcome is not durable.
        next_sequence: u64,
        /// Failed operation.
        operation: SimulationStoreOperation,
        /// Stable I/O class.
        kind: io::ErrorKind,
    },
}

/// Bounded persistence operations used in operator-visible status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationStoreOperation {
    /// Create unpublished file.
    Create,
    /// Write canonical bytes.
    Write,
    /// Synchronize record file.
    SyncFile,
    /// Publish without replacement.
    Publish,
    /// Remove unpublished name after publish.
    RemoveOpen,
    /// Synchronize directory metadata.
    SyncDirectory,
    /// Synchronize and publish the ledger head anchor.
    UpdateHead,
}

/// Durable publication receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationPersisted {
    /// Ledger epoch.
    pub epoch: SimulationLedgerEpoch,
    /// Published sequence.
    pub sequence: u64,
    /// Stable candidate correlation key.
    pub correlation_key: B256,
}

#[derive(Debug)]
struct SimulationDurableInput {
    correlation_key: B256,
    attempt: SimulationAttempt,
    campaign_id: B256,
    victim_tx_hash: B256,
    plan_digest: B256,
    signed_tx_hash: B256,
    economics: PriorityEconomicsReceipt,
    executor: alloy_primitives::Address,
    parent_hash: B256,
    block_number: u64,
    sender: alloy_primitives::Address,
    nonce: u64,
    chain_id: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    valid_until_block: u64,
    hop_protocols: [u8; 2],
    hop_adapters: [alloy_primitives::Address; 2],
    hop_runtime_hashes: [B256; 2],
    deployment_code_hash: B256,
    deployment_digest: B256,
    binary_digest: B256,
    r9_store_identity: B256,
    proof_valid_until_block: u64,
    request_channel_count: u8,
}

impl SimulationDurableInput {
    fn from_record(record: &SimulationRecord) -> Result<Self, SimulationPersistError> {
        let economics = record.economics().ok_or(SimulationPersistError::MissingEconomics)?;
        let evidence =
            record.simulation_evidence().ok_or(SimulationPersistError::MissingIdentityEvidence)?;
        Ok(Self {
            correlation_key: record.correlation_key().value(),
            attempt: record.attempt(),
            campaign_id: B256::from(*record.campaign_id().as_bytes()),
            victim_tx_hash: record.victim_tx_hash(),
            plan_digest: record.plan_digest(),
            signed_tx_hash: record.inclusion_receipt_hash(),
            economics,
            executor: record.executor(),
            parent_hash: evidence.parent_hash,
            block_number: evidence.block_number,
            sender: evidence.sender,
            nonce: evidence.nonce,
            chain_id: evidence.chain_id,
            gas_limit: evidence.gas_limit,
            max_fee_per_gas: evidence.max_fee_per_gas,
            max_priority_fee_per_gas: evidence.max_priority_fee_per_gas,
            valid_until_block: evidence.valid_until_block,
            hop_protocols: evidence.hop_protocols,
            hop_adapters: evidence.hop_adapters,
            hop_runtime_hashes: evidence.hop_runtime_hashes,
            deployment_code_hash: record.deployment_code_hash(),
            deployment_digest: record.deployment_digest(),
            binary_digest: record.binary_digest(),
            r9_store_identity: record.r9_store_identity(),
            proof_valid_until_block: record.proof_valid_until_block(),
            request_channel_count: u8::try_from(record.requests().len())
                .map_err(|_| SimulationPersistError::Oversize)?,
        })
    }
}

/// Exclusive writer for one append-only bounded simulation epoch.
#[derive(Debug)]
pub struct SimulationStore {
    directory: PathBuf,
    directory_handle: File,
    _lease: redb::Database,
    epoch: SimulationLedgerEpoch,
    next_sequence: u64,
    prior_hash: B256,
}

impl SimulationStore {
    /// Opens only the compile-pinned production directory.
    pub fn open() -> Result<Self, SimulationStoreOpenError> {
        Self::open_directory(Path::new(SIMULATION_LEDGER_PATH))
    }

    #[cfg(test)]
    pub(crate) fn open_at(path: &Path) -> Result<Self, SimulationStoreOpenError> {
        Self::open_directory(path)
    }

    fn open_directory(directory: &Path) -> Result<Self, SimulationStoreOpenError> {
        let initialized = match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(SimulationStoreOpenError::InvalidExistingLedger(
                        SimulationLedgerInvalid::FileType,
                    ));
                }
                false
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(directory)
                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
                true
            }
            Err(error) => return Err(SimulationStoreOpenError::Io(error.kind())),
        };

        let is_empty = fs::read_dir(directory)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
            .next()
            .transpose()
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
            .is_none();
        let state = if initialized || is_empty {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            Self::initialize(directory)?
        } else {
            if !directory.join(EPOCH_FILE).exists() {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::Epoch,
                ));
            }
            Self::inspect(directory)?
        };

        let directory_handle =
            File::open(directory).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        let lease_path = directory.join(LEASE_FILE);
        let lease = redb::Database::create(&lease_path).map_err(|error| match error {
            redb::DatabaseError::DatabaseAlreadyOpen => SimulationStoreOpenError::AlreadyOpen,
            other => SimulationStoreOpenError::Lease(other),
        })?;
        fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;

        Ok(Self {
            directory: directory.to_path_buf(),
            directory_handle,
            _lease: lease,
            epoch: state.0,
            next_sequence: state.1,
            prior_hash: state.2,
        })
    }

    fn initialize(
        directory: &Path,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        let mut bytes = [0_u8; 32];
        while bytes.iter().all(|byte| *byte == 0) {
            OsRng.fill_bytes(&mut bytes);
        }
        let epoch = SimulationLedgerEpoch(B256::from(bytes));
        let epoch_path = directory.join(EPOCH_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&epoch_path)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        Self::initialize_head(directory)?;
        File::open(directory)
            .and_then(|handle| handle.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        Ok((epoch, 0, B256::ZERO))
    }

    fn inspect(
        directory: &Path,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        let bytes = fs::read(directory.join(EPOCH_FILE))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Epoch)
        })?;
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Epoch,
            ));
        }
        let epoch = SimulationLedgerEpoch(B256::from(bytes));

        let mut sequences = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
        {
            let entry = entry.map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::UnknownEntry,
            ))?;
            let file_type =
                entry.file_type().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let metadata =
                entry.metadata().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            if !file_type.is_file() || metadata.nlink() != 1 {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::FileType,
                ));
            }
            if name == EPOCH_FILE || name == HEAD_FILE || name == LEASE_FILE {
                continue;
            }
            if name.ends_with(".open") {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::StaleOpen,
                ));
            }
            let sequence = name
                .strip_suffix(".record")
                .filter(|digits| {
                    digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
                .and_then(|digits| digits.parse::<u64>().ok())
                .ok_or(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::UnknownEntry,
                ))?;
            sequences.push(sequence);
        }
        sequences.sort_unstable();

        let mut prior_hash = B256::ZERO;
        for (expected, sequence) in sequences.iter().copied().enumerate() {
            let expected = u64::try_from(expected).map_err(|_| {
                SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Sequence)
            })?;
            if sequence != expected {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::Sequence,
                ));
            }
            let bytes = fs::read(directory.join(Self::record_name(sequence)))
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            if bytes.len() > SIMULATION_RECORD_MAX_BYTES {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::Oversize,
                ));
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema)
            })?;
            if serde_json::to_vec(&value).ok().as_deref() != Some(bytes.as_slice()) {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::Schema,
                ));
            }
            Self::validate_existing(&value, epoch, sequence, prior_hash)?;
            prior_hash = keccak256(&bytes);
        }
        let next_sequence = u64::try_from(sequences.len()).map_err(|_| {
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Sequence)
        })?;
        if next_sequence > SIMULATION_RECORD_CAPACITY {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Sequence,
            ));
        }
        Self::validate_head(directory, next_sequence, prior_hash)?;
        Ok((epoch, next_sequence, prior_hash))
    }

    fn validate_head(
        directory: &Path,
        next_sequence: u64,
        prior_hash: B256,
    ) -> Result<(), SimulationStoreOpenError> {
        let bytes = fs::read(directory.join(HEAD_FILE)).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::HashChain)
            } else {
                SimulationStoreOpenError::Io(error.kind())
            }
        })?;
        if bytes.len() != 40 {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain,
            ));
        }
        let observed_sequence = u64::from_be_bytes(bytes[..8].try_into().map_err(|_| {
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::HashChain)
        })?);
        if observed_sequence != next_sequence || bytes[8..] != prior_hash[..] {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain,
            ));
        }
        Ok(())
    }

    fn initialize_head(directory: &Path) -> Result<(), SimulationStoreOpenError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(HEAD_FILE))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        file.write_all(&Self::head_bytes(0, B256::ZERO))
            .and_then(|()| file.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))
    }
    fn validate_existing(
        value: &Value,
        epoch: SimulationLedgerEpoch,
        sequence: u64,
        prior_hash: B256,
    ) -> Result<(), SimulationStoreOpenError> {
        let invalid =
            || SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema);
        let object = value.as_object().ok_or_else(invalid)?;
        let top_keys = [
            "attempt",
            "blockIdentity",
            "campaignId",
            "correlationKey",
            "economics",
            "execution",
            "expectedInclusionHash",
            "ledgerEpoch",
            "planDigest",
            "priorRecordHash",
            "proofIdentity",
            "requestChannelCount",
            "route",
            "sequence",
            "signedTxHash",
            "version",
            "victimTxHash",
        ];
        if !Self::has_exact_keys(object, &top_keys)
            || object.get("version").and_then(Value::as_u64) != Some(VERSION)
            || object.get("sequence").and_then(Value::as_u64) != Some(sequence)
            || !Self::hash_field(object, "ledgerEpoch", false)
            || object.get("ledgerEpoch").and_then(Value::as_str)
                != Some(Self::hex(epoch.value()).as_str())
        {
            return Err(invalid());
        }
        if !Self::hash_field(object, "priorRecordHash", true) {
            return Err(invalid());
        }
        if object.get("priorRecordHash").and_then(Value::as_str)
            != Some(Self::hex(prior_hash).as_str())
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain,
            ));
        }

        for field in [
            "campaignId",
            "correlationKey",
            "expectedInclusionHash",
            "planDigest",
            "signedTxHash",
            "victimTxHash",
        ] {
            if !Self::hash_field(object, field, false) {
                return Err(invalid());
            }
        }
        if object.get("signedTxHash") != object.get("expectedInclusionHash") {
            return Err(invalid());
        }

        let attempt = object.get("attempt").and_then(Value::as_str).ok_or_else(invalid)?;
        let channel_count =
            object.get("requestChannelCount").and_then(Value::as_u64).ok_or_else(invalid)?;
        if !matches!((attempt, channel_count), ("initial", 2) | ("attribution-retry", 1)) {
            return Err(invalid());
        }

        let block = Self::exact_object(object, "blockIdentity", &["number", "parentHash"])?;
        let block_number = block
            .get("number")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(invalid)?;
        if !Self::hash_field(block, "parentHash", false) {
            return Err(invalid());
        }

        let execution = Self::exact_object(
            object,
            "execution",
            &[
                "chainId",
                "executor",
                "gasLimit",
                "maxFeePerGasWei",
                "maxPriorityFeePerGasWei",
                "nonce",
                "sender",
                "validUntilBlock",
            ],
        )?;
        let gas_limit = execution
            .get("gasLimit")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(invalid)?;
        let valid_until = execution
            .get("validUntilBlock")
            .and_then(Value::as_u64)
            .filter(|value| *value > block_number)
            .ok_or_else(invalid)?;
        if execution.get("chainId").and_then(Value::as_u64) != Some(super::CHAIN_ID_BASE)
            || execution.get("nonce").and_then(Value::as_u64).is_none()
            || !Self::address_field(execution, "executor", true)
            || !Self::address_field(execution, "sender", false)
        {
            return Err(invalid());
        }
        let max_fee = Self::decimal_field(execution, "maxFeePerGasWei")?;
        let max_priority = Self::decimal_field(execution, "maxPriorityFeePerGasWei")?;
        if max_fee.is_zero() || max_priority.is_zero() || max_priority > max_fee {
            return Err(invalid());
        }

        let proof = Self::exact_object(
            object,
            "proofIdentity",
            &[
                "binaryDigest",
                "deploymentCodeHash",
                "deploymentDigest",
                "r9StoreIdentity",
                "validUntilBlock",
            ],
        )?;
        if proof.get("validUntilBlock").and_then(Value::as_u64) != Some(valid_until)
            || ["binaryDigest", "deploymentCodeHash", "deploymentDigest", "r9StoreIdentity"]
                .iter()
                .any(|field| !Self::hash_field(proof, field, false))
        {
            return Err(invalid());
        }

        let route = object.get("route").and_then(Value::as_array).ok_or_else(invalid)?;
        if route.len() != 2 {
            return Err(invalid());
        }
        for hop in route {
            let hop = hop.as_object().ok_or_else(invalid)?;
            if !Self::has_exact_keys(hop, &["adapter", "protocol", "runtimeHash"])
                || !Self::address_field(hop, "adapter", false)
                || !Self::hash_field(hop, "runtimeHash", false)
                || !matches!(
                    hop.get("protocol").and_then(Value::as_str),
                    Some("uniswap-v2" | "aerodrome-volatile" | "aerodrome-stable" | "uniswap-v3")
                )
            {
                return Err(invalid());
            }
        }

        let economics = Self::exact_object(
            object,
            "economics",
            &[
                "authorityBlock",
                "baseFeePerGasWei",
                "executionGasEstimate",
                "expectedEvWei",
                "grossProfitWei",
                "kickbackWei",
                "l1DataFeeWei",
                "l2ExecutionFeeWei",
                "retainedValueWei",
                "totalCostWei",
                "victimMaxFeePerGasWei",
                "victimPriorityFeePerGasWei",
            ],
        )?;
        if economics.get("authorityBlock").and_then(Value::as_u64) != Some(block_number) {
            return Err(invalid());
        }
        let gross = Self::positive_decimal(economics, "grossProfitWei")?;
        let kickback = Self::positive_decimal(economics, "kickbackWei")?;
        let retained = Self::positive_decimal(economics, "retainedValueWei")?;
        let gas = Self::positive_decimal(economics, "executionGasEstimate")?;
        let l1_fee = Self::positive_decimal(economics, "l1DataFeeWei")?;
        let l2_fee = Self::positive_decimal(economics, "l2ExecutionFeeWei")?;
        let total = Self::positive_decimal(economics, "totalCostWei")?;
        let expected = Self::positive_decimal(economics, "expectedEvWei")?;
        let base_fee = Self::positive_decimal(economics, "baseFeePerGasWei")?;
        let victim_priority = Self::positive_decimal(economics, "victimPriorityFeePerGasWei")?;
        let victim_max = Self::positive_decimal(economics, "victimMaxFeePerGasWei")?;
        let expected_kickback = (gross / U256::from(4)) * U256::from(3)
            + ((gross % U256::from(4)) * U256::from(3) + U256::from(3)) / U256::from(4);
        let effective_fee = base_fee.checked_add(victim_priority).ok_or_else(invalid)?;
        if kickback != expected_kickback
            || gross.checked_sub(kickback) != Some(retained)
            || gas.checked_mul(effective_fee) != Some(l2_fee)
            || l2_fee.checked_add(l1_fee) != Some(total)
            || retained.checked_sub(total) != Some(expected)
            || victim_max != max_fee
            || victim_priority != max_priority
            || victim_max < effective_fee
            || U256::from(gas_limit) < gas
        {
            return Err(invalid());
        }

        let campaign = Self::parse_hash(object, "campaignId").ok_or_else(invalid)?;
        let victim = Self::parse_hash(object, "victimTxHash").ok_or_else(invalid)?;
        let plan = Self::parse_hash(object, "planDigest").ok_or_else(invalid)?;
        let signed = Self::parse_hash(object, "signedTxHash").ok_or_else(invalid)?;
        let mut correlation_input = Vec::with_capacity(32 * 4 + 30);
        correlation_input.extend_from_slice(b"base-mev/simulation-correlation/v1");
        correlation_input.extend_from_slice(campaign.as_slice());
        correlation_input.extend_from_slice(victim.as_slice());
        correlation_input.extend_from_slice(plan.as_slice());
        correlation_input.extend_from_slice(signed.as_slice());
        if object.get("correlationKey").and_then(Value::as_str)
            != Some(Self::hex(keccak256(correlation_input)).as_str())
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn exact_object<'a>(
        object: &'a serde_json::Map<String, Value>,
        field: &str,
        expected: &[&str],
    ) -> Result<&'a serde_json::Map<String, Value>, SimulationStoreOpenError> {
        let object = object.get(field).and_then(Value::as_object).ok_or(
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema),
        )?;
        if !Self::has_exact_keys(object, expected) {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Schema,
            ));
        }
        Ok(object)
    }

    fn fixed_hex(value: &str, digits: usize, allow_zero: bool) -> bool {
        value.len() == digits + 2
            && value.starts_with("0x")
            && value[2..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && (allow_zero || value[2..].bytes().any(|byte| byte != b'0'))
    }

    fn hash_field(object: &serde_json::Map<String, Value>, field: &str, allow_zero: bool) -> bool {
        object
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| Self::fixed_hex(value, 64, allow_zero))
    }

    fn address_field(
        object: &serde_json::Map<String, Value>,
        field: &str,
        allow_zero: bool,
    ) -> bool {
        object
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| Self::fixed_hex(value, 40, allow_zero))
    }

    fn parse_hash(object: &serde_json::Map<String, Value>, field: &str) -> Option<B256> {
        object.get(field).and_then(Value::as_str)?.parse().ok()
    }

    fn decimal_field(
        object: &serde_json::Map<String, Value>,
        field: &str,
    ) -> Result<U256, SimulationStoreOpenError> {
        let value = object.get(field).and_then(Value::as_str).ok_or(
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema),
        )?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Schema,
            ));
        }
        value.parse().map_err(|_| {
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema)
        })
    }

    fn positive_decimal(
        object: &serde_json::Map<String, Value>,
        field: &str,
    ) -> Result<U256, SimulationStoreOpenError> {
        Self::decimal_field(object, field).and_then(|value| {
            if value.is_zero() {
                Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::Schema,
                ))
            } else {
                Ok(value)
            }
        })
    }

    fn has_exact_keys(object: &serde_json::Map<String, Value>, expected: &[&str]) -> bool {
        object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
    }

    /// Returns whether capacity remains before irreversible claim/signing work.
    pub const fn has_capacity(&self) -> bool {
        self.next_sequence < SIMULATION_RECORD_CAPACITY
    }

    /// Fails before irreversible claim/signing work when the epoch is full.
    pub const fn ensure_capacity(&self) -> Result<(), SimulationPersistError> {
        if self.has_capacity() {
            Ok(())
        } else {
            Err(SimulationPersistError::Full {
                epoch: self.epoch,
                next_sequence: self.next_sequence,
                capacity: SIMULATION_RECORD_CAPACITY,
            })
        }
    }

    /// Appends and fsyncs one inert simulation attempt.
    pub fn append(
        &mut self,
        record: &SimulationRecord,
    ) -> Result<SimulationPersisted, SimulationPersistError> {
        if !self.has_capacity() {
            return Err(SimulationPersistError::Full {
                epoch: self.epoch,
                next_sequence: self.next_sequence,
                capacity: SIMULATION_RECORD_CAPACITY,
            });
        }
        let input = SimulationDurableInput::from_record(record)?;
        let bytes = self.encode(&input)?;
        let sequence = self.next_sequence;
        let open_path = self.directory.join(Self::open_name(sequence));
        let record_path = self.directory.join(Self::record_name(sequence));
        let mut file =
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(&open_path).map_err(
                |error| self.write_error(sequence, SimulationStoreOperation::Create, error),
            )?;
        file.write_all(&bytes)
            .map_err(|error| self.write_error(sequence, SimulationStoreOperation::Write, error))?;
        file.sync_all().map_err(|error| {
            self.write_error(sequence, SimulationStoreOperation::SyncFile, error)
        })?;
        fs::hard_link(&open_path, &record_path).map_err(|error| {
            self.write_error(sequence, SimulationStoreOperation::Publish, error)
        })?;
        fs::remove_file(&open_path).map_err(|error| {
            self.write_error(sequence, SimulationStoreOperation::RemoveOpen, error)
        })?;
        self.directory_handle.sync_all().map_err(|error| {
            self.write_error(sequence, SimulationStoreOperation::SyncDirectory, error)
        })?;
        let record_hash = keccak256(&bytes);
        self.publish_head(sequence + 1, record_hash, sequence)?;
        self.prior_hash = record_hash;
        self.next_sequence += 1;
        Ok(SimulationPersisted {
            epoch: self.epoch,
            sequence,
            correlation_key: input.correlation_key,
        })
    }

    fn publish_head(
        &self,
        next_sequence: u64,
        record_hash: B256,
        failed_sequence: u64,
    ) -> Result<(), SimulationPersistError> {
        let open_path = self.directory.join(HEAD_OPEN_FILE);
        let head_path = self.directory.join(HEAD_FILE);
        let mut file =
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(&open_path).map_err(
                |error| {
                    self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
                },
            )?;
        file.write_all(&Self::head_bytes(next_sequence, record_hash))
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
            })?;
        fs::rename(&open_path, &head_path).map_err(|error| {
            self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
        })?;
        self.directory_handle.sync_all().map_err(|error| {
            self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
        })
    }

    fn head_bytes(next_sequence: u64, record_hash: B256) -> [u8; 40] {
        let mut bytes = [0_u8; 40];
        bytes[..8].copy_from_slice(&next_sequence.to_be_bytes());
        bytes[8..].copy_from_slice(record_hash.as_slice());
        bytes
    }
    fn encode(&self, input: &SimulationDurableInput) -> Result<Vec<u8>, SimulationPersistError> {
        let economics = input.economics;
        let value = json!({
            "attempt": match input.attempt {
                SimulationAttempt::Initial => "initial",
                SimulationAttempt::AttributionRetry => "attribution-retry",
            },
            "campaignId": Self::hex(input.campaign_id),
            "blockIdentity": {
                "number": input.block_number,
                "parentHash": Self::hex(input.parent_hash),
            },
            "correlationKey": Self::hex(input.correlation_key),
            "economics": {
                "authorityBlock": economics.authority_block(),
                "baseFeePerGasWei": economics.base_fee_per_gas_wei().to_string(),
                "executionGasEstimate": economics.execution_gas_estimate().to_string(),
                "expectedEvWei": economics.expected_ev_wei().map(|value| value.to_string()),
                "grossProfitWei": economics.gross_profit_wei().to_string(),
                "kickbackWei": economics.kickback_wei().to_string(),
                "l1DataFeeWei": economics.l1_data_fee_wei().to_string(),
                "l2ExecutionFeeWei": economics.l2_execution_fee_wei().to_string(),
                "retainedValueWei": economics.retained_value_wei().to_string(),
                "totalCostWei": economics.total_cost_wei().to_string(),
                "victimMaxFeePerGasWei": economics.victim_max_fee_per_gas_wei().to_string(),
                "victimPriorityFeePerGasWei": economics.victim_priority_fee_per_gas_wei().to_string(),
            },
            "execution": {
                "chainId": input.chain_id,
                "executor": Self::hex_address(input.executor),
                "gasLimit": input.gas_limit,
                "maxFeePerGasWei": input.max_fee_per_gas.to_string(),
                "maxPriorityFeePerGasWei": input.max_priority_fee_per_gas.to_string(),
                "nonce": input.nonce,
                "sender": Self::hex_address(input.sender),
                "validUntilBlock": input.valid_until_block,
            },
            "expectedInclusionHash": Self::hex(input.signed_tx_hash),
            "ledgerEpoch": Self::hex(self.epoch.value()),
            "planDigest": Self::hex(input.plan_digest),
            "proofIdentity": {
                "binaryDigest": Self::hex(input.binary_digest),
                "deploymentCodeHash": Self::hex(input.deployment_code_hash),
                "deploymentDigest": Self::hex(input.deployment_digest),
                "r9StoreIdentity": Self::hex(input.r9_store_identity),
                "validUntilBlock": input.proof_valid_until_block,
            },
            "priorRecordHash": Self::hex(self.prior_hash),
            "requestChannelCount": input.request_channel_count,
            "route": [
                {
                    "adapter": Self::hex_address(input.hop_adapters[0]),
                    "protocol": Self::protocol(input.hop_protocols[0])?,
                    "runtimeHash": Self::hex(input.hop_runtime_hashes[0]),
                },
                {
                    "adapter": Self::hex_address(input.hop_adapters[1]),
                    "protocol": Self::protocol(input.hop_protocols[1])?,
                    "runtimeHash": Self::hex(input.hop_runtime_hashes[1]),
                },
            ],
            "sequence": self.next_sequence,
            "signedTxHash": Self::hex(input.signed_tx_hash),
            "version": VERSION,
            "victimTxHash": Self::hex(input.victim_tx_hash),
        });
        let bytes = serde_json::to_vec(&value).map_err(|_| SimulationPersistError::Oversize)?;
        if bytes.len() > SIMULATION_RECORD_MAX_BYTES {
            return Err(SimulationPersistError::Oversize);
        }
        Ok(bytes)
    }

    fn write_error(
        &self,
        next_sequence: u64,
        operation: SimulationStoreOperation,
        error: io::Error,
    ) -> SimulationPersistError {
        SimulationPersistError::WriteFailed {
            epoch: self.epoch,
            next_sequence,
            operation,
            kind: error.kind(),
        }
    }

    fn open_name(sequence: u64) -> String {
        format!("{sequence:020}.open")
    }

    fn record_name(sequence: u64) -> String {
        format!("{sequence:020}.record")
    }

    fn hex(value: B256) -> String {
        format!("{value:#x}")
    }

    fn hex_address(value: alloy_primitives::Address) -> String {
        format!("{value:#x}")
    }

    fn protocol(value: u8) -> Result<&'static str, SimulationPersistError> {
        match value {
            0 => Ok("uniswap-v2"),
            1 => Ok("aerodrome-volatile"),
            2 => Ok("aerodrome-stable"),
            3 => Ok("uniswap-v3"),
            _ => Err(SimulationPersistError::MissingIdentityEvidence),
        }
    }
}

impl Drop for SimulationStore {
    fn drop(&mut self) {
        let _ = self.directory_handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::economics::{PriorityEconomicsAuthority, PriorityFilterInput, evaluate};
    use alloy_primitives::U256;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "s2-simulation-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn economics() -> PriorityEconomicsReceipt {
        evaluate(PriorityFilterInput {
            gross_profit_wei: Some(U256::from(1_000_000_u64)),
            authority: Some(PriorityEconomicsAuthority::new(
                U256::from(100_u64),
                U256::from(10_000_u64),
                U256::from(100_u64),
                100,
            )),
            victim_max_priority_fee_per_gas_wei: Some(U256::from(100_u64)),
            victim_max_fee_per_gas_wei: Some(U256::from(300_u64)),
            candidate_block: 100,
        })
        .unwrap()
    }

    fn snapshot(path: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name().into_string().unwrap(), fs::read(entry.path()).unwrap())
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn assert_schema_rejected(mutate: impl FnOnce(&mut Value)) {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        store.append(&SimulationRecord::for_store_test(economics())).unwrap();
        drop(store);
        let record_path = path.join(SimulationStore::record_name(0));
        let mut value: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        mutate(&mut value);
        fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema))
        ));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn p0_publish_reopen_and_hash_chain_are_green() {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        let record = SimulationRecord::for_store_test(economics());
        let persisted = store.append(&record).unwrap();
        assert_eq!(persisted.sequence, 0);
        assert_eq!(persisted.correlation_key, record.correlation_key().value());
        let durable: Value =
            serde_json::from_slice(&fs::read(path.join(SimulationStore::record_name(0))).unwrap())
                .unwrap();
        assert_eq!(durable["execution"]["chainId"], super::super::CHAIN_ID_BASE);
        assert_eq!(durable["execution"]["nonce"], 9);
        assert_eq!(durable["requestChannelCount"], 2);
        assert_eq!(durable["route"][0]["protocol"], "uniswap-v2");
        assert_eq!(durable["route"][1]["protocol"], "uniswap-v3");
        assert_eq!(durable["economics"]["expectedEvWei"], U256::from(220_000_u64).to_string());
        drop(store);

        let reopened = SimulationStore::open_at(&path).unwrap();
        assert_eq!(reopened.next_sequence, 1);
        eprintln!("P0: GREEN");
        drop(reopened);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn q2_exclusive_lease_recovers_after_clean_drop() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::AlreadyOpen)
        ));
        drop(store);
        let reopened = SimulationStore::open_at(&path).unwrap();
        drop(reopened);
        eprintln!("Q2: GREEN");
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn p3_trailing_deletion_is_red_against_durable_head() {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        let record = SimulationRecord::for_store_test(economics());
        store.append(&record).unwrap();
        drop(store);
        fs::remove_file(path.join(SimulationStore::record_name(0))).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain
            ))
        ));
        eprintln!("P3-DELETE: RED");
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn p3_stale_open_is_red() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        drop(store);
        fs::write(path.join("00000000000000000000.open"), b"partial").unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::StaleOpen
            ))
        ));
        eprintln!("P3: RED");
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn nonempty_directory_without_epoch_is_rejected_without_mutation() {
        let path = temp();
        fs::write(path.join("operator-note"), b"preserve exactly").unwrap();
        let before = snapshot(&path);
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Epoch))
        ));
        assert_eq!(snapshot(&path), before);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn closed_v1_schema_rejects_unclassified_and_cross_field_variants() {
        assert_schema_rejected(|value| {
            value.as_object_mut().unwrap().insert("futureField".into(), Value::Bool(true));
        });
        assert_schema_rejected(|value| {
            value["campaignId"] = Value::String(format!("0X{}", "02".repeat(32)));
        });
        assert_schema_rejected(|value| {
            value["execution"]["chainId"] = Value::from(1_u64);
        });
        assert_schema_rejected(|value| {
            value["route"][0]["protocol"] = Value::String("unknown".into());
        });
        assert_schema_rejected(|value| {
            value["route"].as_array_mut().unwrap().pop();
        });
        assert_schema_rejected(|value| {
            value["requestChannelCount"] = Value::from(1_u64);
        });
        assert_schema_rejected(|value| {
            value["economics"]["expectedEvWei"] = Value::String("0219999".into());
        });
        assert_schema_rejected(|value| {
            value["economics"]["totalCostWei"] = Value::String("30001".into());
        });
        assert_schema_rejected(|value| {
            value["proofIdentity"]["validUntilBlock"] = Value::from(102_u64);
        });
        assert_schema_rejected(|value| {
            value["correlationKey"] = Value::String(format!("0x{}", "ff".repeat(32)));
        });
    }

    #[test]
    fn capacity_fails_closed_at_the_fixed_bound() {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        store.next_sequence = SIMULATION_RECORD_CAPACITY;
        assert!(!store.has_capacity());
        assert!(matches!(
            store.ensure_capacity(),
            Err(SimulationPersistError::Full {
                next_sequence: SIMULATION_RECORD_CAPACITY,
                capacity: SIMULATION_RECORD_CAPACITY,
                ..
            })
        ));
        assert!(matches!(
            store.append(&SimulationRecord::for_store_test(economics())),
            Err(SimulationPersistError::Full { .. })
        ));
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn published_record_without_published_head_is_rejected() {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        store.append(&SimulationRecord::for_store_test(economics())).unwrap();
        drop(store);
        fs::write(path.join(HEAD_FILE), SimulationStore::head_bytes(0, B256::ZERO)).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain
            ))
        ));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn unpublished_head_is_rejected_as_stale_open() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        drop(store);
        fs::write(path.join(HEAD_OPEN_FILE), SimulationStore::head_bytes(1, B256::ZERO)).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::StaleOpen
            ))
        ));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn corrupt_lease_is_not_misclassified_as_contention() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        drop(store);
        fs::write(path.join(LEASE_FILE), b"not-a-redb-database").unwrap();
        assert!(matches!(SimulationStore::open_at(&path), Err(SimulationStoreOpenError::Lease(_))));
        fs::remove_dir_all(path).unwrap();
    }
}
