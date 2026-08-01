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

use super::{SimulationAttempt, SimulationCorrelationKey, SimulationRecord};
use crate::PriorityEconomicsReceipt;

/// Compile-pinned local simulation ledger directory.
pub const SIMULATION_LEDGER_PATH: &str = "/home/ubuntu/.local/state/base-mev/simulation-v1";
/// Maximum durable records in one ledger epoch.
pub const SIMULATION_RECORD_CAPACITY: u64 = 262_144;
/// Maximum canonical JSON bytes in one durable record.
pub const SIMULATION_RECORD_MAX_BYTES: usize = 16 * 1024;

const LEASE_FILE: &str = ".lease";
const HEAD_FILE: &str = "head";
const HEAD_OPEN_FILE: &str = ".head.open";
const VERSION: u64 = 1;

/// One random identity separating owner-rotated ledger epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationLedgerEpoch([u8; 32]);

impl SimulationLedgerEpoch {
    /// Returns the opaque epoch bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(bytes: [u8; 32]) -> Self {
        assert!(bytes != [0; 32]);
        Self(bytes)
    }
}

/// Immutable durable correlation coordinates for one simulation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationCorrelationEnvelopeV1 {
    ledger_epoch: SimulationLedgerEpoch,
    sequence: u64,
    correlation_key: SimulationCorrelationKey,
}

impl SimulationCorrelationEnvelopeV1 {
    /// Returns the ledger generation containing the attempt.
    pub const fn ledger_epoch(&self) -> SimulationLedgerEpoch {
        self.ledger_epoch
    }

    /// Returns the sequence within the ledger generation.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the stable candidate correlation key.
    pub const fn correlation_key(&self) -> SimulationCorrelationKey {
        self.correlation_key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SimulationLedgerHead {
    ledger_epoch: SimulationLedgerEpoch,
    next_sequence: u64,
    latest_record_hash: B256,
}

impl SimulationLedgerHead {
    const ENCODED_LEN: usize = 72;

    fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0_u8; Self::ENCODED_LEN];
        bytes[..32].copy_from_slice(self.ledger_epoch.as_bytes());
        bytes[32..40].copy_from_slice(&self.next_sequence.to_be_bytes());
        bytes[40..].copy_from_slice(self.latest_record_hash.as_slice());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, SimulationLedgerInvalid> {
        let bytes: &[u8; Self::ENCODED_LEN] =
            bytes.try_into().map_err(|_| SimulationLedgerInvalid::HashChain)?;
        let epoch: [u8; 32] = bytes[..32].try_into().map_err(|_| SimulationLedgerInvalid::Epoch)?;
        if epoch.iter().all(|byte| *byte == 0) {
            return Err(SimulationLedgerInvalid::Epoch);
        }
        Ok(Self {
            ledger_epoch: SimulationLedgerEpoch(epoch),
            next_sequence: u64::from_be_bytes(
                bytes[32..40].try_into().map_err(|_| SimulationLedgerInvalid::HashChain)?,
            ),
            latest_record_hash: B256::from_slice(&bytes[40..]),
        })
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
    InvalidExistingLedger {
        /// Trusted epoch decoded from the durable head, when available.
        ledger_epoch: Option<SimulationLedgerEpoch>,
        /// Closed structural class.
        class: SimulationLedgerInvalid,
    },
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
    correlation: SimulationCorrelationEnvelopeV1,
    ledger_full_after_commit: bool,
}

impl SimulationPersisted {
    /// Returns the immutable durable correlation coordinates.
    pub const fn correlation(&self) -> &SimulationCorrelationEnvelopeV1 {
        &self.correlation
    }

    /// Returns whether this durable commit consumed the final ledger slot.
    pub const fn ledger_full_after_commit(&self) -> bool {
        self.ledger_full_after_commit
    }
}

#[derive(Debug)]
struct SimulationDurableInput {
    correlation_key: SimulationCorrelationKey,
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
            correlation_key: record.correlation_key(),
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
        let created = match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(SimulationStoreOpenError::InvalidExistingLedger {
                        ledger_epoch: None,
                        class: SimulationLedgerInvalid::FileType,
                    });
                }
                let is_empty = fs::read_dir(directory)
                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
                    .next()
                    .transpose()
                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
                    .is_none();
                if is_empty {
                    return Err(SimulationStoreOpenError::InvalidExistingLedger {
                        ledger_epoch: None,
                        class: SimulationLedgerInvalid::Epoch,
                    });
                }
                let lease_metadata =
                    fs::symlink_metadata(directory.join(LEASE_FILE)).map_err(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            SimulationStoreOpenError::InvalidExistingLedger {
                                ledger_epoch: None,
                                class: SimulationLedgerInvalid::Epoch,
                            }
                        } else {
                            SimulationStoreOpenError::Io(error.kind())
                        }
                    })?;
                if !lease_metadata.file_type().is_file()
                    || lease_metadata.file_type().is_symlink()
                    || lease_metadata.nlink() != 1
                {
                    return Err(SimulationStoreOpenError::InvalidExistingLedger {
                        ledger_epoch: None,
                        class: SimulationLedgerInvalid::FileType,
                    });
                }
                false
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Self::create_directory(directory)
                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
                true
            }
            Err(error) => return Err(SimulationStoreOpenError::Io(error.kind())),
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

        let state = if created {
            Self::initialize(directory, &directory_handle)?
        } else {
            Self::inspect(directory)?
        };

        Ok(Self {
            directory: directory.to_path_buf(),
            directory_handle,
            _lease: lease,
            epoch: state.0,
            next_sequence: state.1,
            prior_hash: state.2,
        })
    }

    fn create_directory(directory: &Path) -> io::Result<()> {
        Self::create_directory_with(
            directory,
            |path| File::open(path),
            |path| fs::DirBuilder::new().mode(0o700).create(path),
            File::sync_all,
        )
    }

    fn create_directory_with<Parent, OpenParent, CreateChild, SyncParent>(
        directory: &Path,
        open_parent: OpenParent,
        create_child: CreateChild,
        sync_parent: SyncParent,
    ) -> io::Result<()>
    where
        OpenParent: FnOnce(&Path) -> io::Result<Parent>,
        CreateChild: FnOnce(&Path) -> io::Result<()>,
        SyncParent: FnOnce(&Parent) -> io::Result<()>,
    {
        let parent =
            directory.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let parent_handle = open_parent(parent)?;
        create_child(directory)?;
        sync_parent(&parent_handle)
    }

    fn initialize(
        directory: &Path,
        directory_handle: &File,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        let mut bytes = [0_u8; 32];
        while bytes.iter().all(|byte| *byte == 0) {
            OsRng.fill_bytes(&mut bytes);
        }
        let epoch = SimulationLedgerEpoch(bytes);
        let head = SimulationLedgerHead {
            ledger_epoch: epoch,
            next_sequence: 0,
            latest_record_hash: B256::ZERO,
        };
        let open_path = directory.join(HEAD_OPEN_FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&open_path)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        file.write_all(&head.encode())
            .and_then(|()| file.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        fs::rename(&open_path, directory.join(HEAD_FILE))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        directory_handle.sync_all().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        Ok((epoch, 0, B256::ZERO))
    }

    fn inspect(
        directory: &Path,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        Self::inspect_with_capacity(directory, SIMULATION_RECORD_CAPACITY)
    }

    fn inspect_with_capacity(
        directory: &Path,
        capacity: u64,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        let head = Self::read_head(directory)?;
        let epoch = head.ledger_epoch;
        let invalid = |class| SimulationStoreOpenError::InvalidExistingLedger {
            ledger_epoch: Some(epoch),
            class,
        };

        let mut sequences = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
        {
            let entry = entry.map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let name = entry.file_name();
            let name =
                name.to_str().ok_or_else(|| invalid(SimulationLedgerInvalid::UnknownEntry))?;
            let file_type =
                entry.file_type().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let metadata =
                entry.metadata().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            if !file_type.is_file() || metadata.nlink() != 1 {
                return Err(invalid(SimulationLedgerInvalid::FileType));
            }
            if name == HEAD_FILE || name == LEASE_FILE {
                continue;
            }
            if name.ends_with(".open") {
                return Err(invalid(SimulationLedgerInvalid::StaleOpen));
            }
            let sequence = name
                .strip_suffix(".record")
                .filter(|digits| {
                    digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
                .and_then(|digits| digits.parse::<u64>().ok())
                .ok_or_else(|| invalid(SimulationLedgerInvalid::UnknownEntry))?;
            let accumulated = u64::try_from(sequences.len())
                .map_err(|_| invalid(SimulationLedgerInvalid::Sequence))?;
            if accumulated >= capacity {
                return Err(invalid(SimulationLedgerInvalid::Sequence));
            }
            sequences.push(sequence);
        }
        sequences.sort_unstable();

        let mut prior_hash = B256::ZERO;
        for (expected, sequence) in sequences.iter().copied().enumerate() {
            let expected =
                u64::try_from(expected).map_err(|_| invalid(SimulationLedgerInvalid::Sequence))?;
            if sequence != expected {
                return Err(invalid(SimulationLedgerInvalid::Sequence));
            }
            let bytes = fs::read(directory.join(Self::record_name(sequence)))
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            if bytes.len() > SIMULATION_RECORD_MAX_BYTES {
                return Err(invalid(SimulationLedgerInvalid::Oversize));
            }
            if bytes
                .windows(b"\"ledgerEpoch\":".len())
                .filter(|window| *window == b"\"ledgerEpoch\":")
                .count()
                != 2
            {
                return Err(SimulationStoreOpenError::InvalidExistingLedger {
                    ledger_epoch: None,
                    class: SimulationLedgerInvalid::Epoch,
                });
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| invalid(SimulationLedgerInvalid::Schema))?;
            if serde_json::to_vec(&value).ok().as_deref() != Some(bytes.as_slice()) {
                return Err(invalid(SimulationLedgerInvalid::Schema));
            }
            Self::validate_existing(&value, epoch, sequence, prior_hash)?;
            prior_hash = keccak256(&bytes);
        }
        let next_sequence = u64::try_from(sequences.len())
            .map_err(|_| invalid(SimulationLedgerInvalid::Sequence))?;
        if head.next_sequence != next_sequence || head.latest_record_hash != prior_hash {
            return Err(invalid(SimulationLedgerInvalid::HashChain));
        }
        Ok((epoch, next_sequence, prior_hash))
    }

    fn read_head(directory: &Path) -> Result<SimulationLedgerHead, SimulationStoreOpenError> {
        let path = directory.join(HEAD_FILE);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                SimulationStoreOpenError::InvalidExistingLedger {
                    ledger_epoch: None,
                    class: SimulationLedgerInvalid::HashChain,
                }
            } else {
                SimulationStoreOpenError::Io(error.kind())
            }
        })?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.nlink() != 1
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: None,
                class: SimulationLedgerInvalid::FileType,
            });
        }
        let bytes = fs::read(path).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        SimulationLedgerHead::decode(&bytes).map_err(|class| {
            SimulationStoreOpenError::InvalidExistingLedger { ledger_epoch: None, class }
        })
    }
    fn validate_existing(
        value: &Value,
        epoch: SimulationLedgerEpoch,
        sequence: u64,
        prior_hash: B256,
    ) -> Result<(), SimulationStoreOpenError> {
        let invalid = || SimulationStoreOpenError::InvalidExistingLedger {
            ledger_epoch: Some(epoch),
            class: SimulationLedgerInvalid::Schema,
        };
        let invalid_epoch = || SimulationStoreOpenError::InvalidExistingLedger {
            ledger_epoch: None,
            class: SimulationLedgerInvalid::Epoch,
        };
        let object = value.as_object().ok_or_else(invalid)?;
        if !Self::hash_field(object, "ledgerEpoch", false)
            || object.get("ledgerEpoch").and_then(Value::as_str)
                != Some(Self::hex_epoch(epoch).as_str())
        {
            return Err(invalid_epoch());
        }
        let top_keys = [
            "attempt",
            "blockIdentity",
            "campaignId",
            "correlation",
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
        {
            return Err(invalid());
        }
        let correlation = Self::exact_object(
            object,
            epoch,
            "correlation",
            &["correlationKey", "ledgerEpoch", "sequence"],
        )?;
        if !Self::hash_field(correlation, "ledgerEpoch", false)
            || correlation.get("ledgerEpoch") != object.get("ledgerEpoch")
        {
            return Err(invalid_epoch());
        }
        if correlation.get("sequence").and_then(Value::as_u64) != Some(sequence)
            || !Self::hash_field(correlation, "correlationKey", false)
            || correlation.get("correlationKey") != object.get("correlationKey")
        {
            return Err(invalid());
        }
        if !Self::hash_field(object, "priorRecordHash", true) {
            return Err(invalid());
        }
        if object.get("priorRecordHash").and_then(Value::as_str)
            != Some(Self::hex(prior_hash).as_str())
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(epoch),
                class: SimulationLedgerInvalid::HashChain,
            });
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

        let block = Self::exact_object(object, epoch, "blockIdentity", &["number", "parentHash"])?;
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
            epoch,
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
        let max_fee = Self::decimal_field(execution, epoch, "maxFeePerGasWei")?;
        let max_priority = Self::decimal_field(execution, epoch, "maxPriorityFeePerGasWei")?;
        if max_fee.is_zero() || max_priority.is_zero() || max_priority > max_fee {
            return Err(invalid());
        }

        let proof = Self::exact_object(
            object,
            epoch,
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
            epoch,
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
        let gross = Self::positive_decimal(economics, epoch, "grossProfitWei")?;
        let kickback = Self::positive_decimal(economics, epoch, "kickbackWei")?;
        let retained = Self::positive_decimal(economics, epoch, "retainedValueWei")?;
        let gas = Self::positive_decimal(economics, epoch, "executionGasEstimate")?;
        let l1_fee = Self::positive_decimal(economics, epoch, "l1DataFeeWei")?;
        let l2_fee = Self::positive_decimal(economics, epoch, "l2ExecutionFeeWei")?;
        let total = Self::positive_decimal(economics, epoch, "totalCostWei")?;
        let expected = Self::positive_decimal(economics, epoch, "expectedEvWei")?;
        let base_fee = Self::positive_decimal(economics, epoch, "baseFeePerGasWei")?;
        let victim_priority =
            Self::positive_decimal(economics, epoch, "victimPriorityFeePerGasWei")?;
        let victim_max = Self::positive_decimal(economics, epoch, "victimMaxFeePerGasWei")?;
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
        let recomputed = Self::recompute_correlation_key(campaign, victim, plan, signed);
        let recomputed = Self::hex(recomputed);
        if object.get("correlationKey").and_then(Value::as_str) != Some(recomputed.as_str())
            || correlation.get("correlationKey").and_then(Value::as_str)
                != Some(recomputed.as_str())
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn exact_object<'a>(
        object: &'a serde_json::Map<String, Value>,
        epoch: SimulationLedgerEpoch,
        field: &str,
        expected: &[&str],
    ) -> Result<&'a serde_json::Map<String, Value>, SimulationStoreOpenError> {
        let object = object.get(field).and_then(Value::as_object).ok_or(
            SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(epoch),
                class: SimulationLedgerInvalid::Schema,
            },
        )?;
        if !Self::has_exact_keys(object, expected) {
            return Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(epoch),
                class: SimulationLedgerInvalid::Schema,
            });
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
        epoch: SimulationLedgerEpoch,
        field: &str,
    ) -> Result<U256, SimulationStoreOpenError> {
        let value = object.get(field).and_then(Value::as_str).ok_or(
            SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(epoch),
                class: SimulationLedgerInvalid::Schema,
            },
        )?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(epoch),
                class: SimulationLedgerInvalid::Schema,
            });
        }
        value.parse().map_err(|_| SimulationStoreOpenError::InvalidExistingLedger {
            ledger_epoch: Some(epoch),
            class: SimulationLedgerInvalid::Schema,
        })
    }

    fn positive_decimal(
        object: &serde_json::Map<String, Value>,
        epoch: SimulationLedgerEpoch,
        field: &str,
    ) -> Result<U256, SimulationStoreOpenError> {
        Self::decimal_field(object, epoch, field).and_then(|value| {
            if value.is_zero() {
                Err(SimulationStoreOpenError::InvalidExistingLedger {
                    ledger_epoch: Some(epoch),
                    class: SimulationLedgerInvalid::Schema,
                })
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
        let recomputed = Self::recompute_correlation_key(
            input.campaign_id,
            input.victim_tx_hash,
            input.plan_digest,
            input.signed_tx_hash,
        );
        if input.correlation_key.value() != recomputed {
            return Err(SimulationPersistError::MissingIdentityEvidence);
        }
        let sequence = self.next_sequence;
        let correlation = SimulationCorrelationEnvelopeV1 {
            ledger_epoch: self.epoch,
            sequence,
            correlation_key: input.correlation_key,
        };
        let bytes = self.encode(&input, correlation)?;
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
            correlation,
            ledger_full_after_commit: self.next_sequence == SIMULATION_RECORD_CAPACITY,
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
        let head = SimulationLedgerHead {
            ledger_epoch: self.epoch,
            next_sequence,
            latest_record_hash: record_hash,
        };
        file.write_all(&head.encode()).and_then(|()| file.sync_all()).map_err(|error| {
            self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
        })?;
        fs::rename(&open_path, &head_path).map_err(|error| {
            self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
        })?;
        self.directory_handle.sync_all().map_err(|error| {
            self.write_error(failed_sequence, SimulationStoreOperation::UpdateHead, error)
        })
    }

    fn encode(
        &self,
        input: &SimulationDurableInput,
        correlation: SimulationCorrelationEnvelopeV1,
    ) -> Result<Vec<u8>, SimulationPersistError> {
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
            "correlation": {
                "correlationKey": Self::hex(correlation.correlation_key().value()),
                "ledgerEpoch": Self::hex_epoch(correlation.ledger_epoch()),
                "sequence": correlation.sequence(),
            },
            "correlationKey": Self::hex(correlation.correlation_key().value()),
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
            "ledgerEpoch": Self::hex_epoch(self.epoch),
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

    fn hex_epoch(epoch: SimulationLedgerEpoch) -> String {
        Self::hex(B256::from(*epoch.as_bytes()))
    }

    fn recompute_correlation_key(campaign: B256, victim: B256, plan: B256, signed: B256) -> B256 {
        let mut input = Vec::with_capacity(32 * 4 + 30);
        input.extend_from_slice(b"base-mev/simulation-correlation/v1");
        input.extend_from_slice(campaign.as_slice());
        input.extend_from_slice(victim.as_slice());
        input.extend_from_slice(plan.as_slice());
        input.extend_from_slice(signed.as_slice());
        keccak256(input)
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
    use std::{
        cell::RefCell,
        sync::atomic::{AtomicU64, Ordering},
    };

    use alloy_primitives::U256;

    use crate::economics::{PriorityEconomicsAuthority, PriorityFilterInput, evaluate};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "s2-simulation-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
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

    fn populated() -> (PathBuf, SimulationLedgerEpoch) {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        store.append(&SimulationRecord::for_store_test(economics())).unwrap();
        drop(store);
        (path, epoch)
    }

    fn assert_rejected(path: &Path) {
        assert!(matches!(
            SimulationStore::open_at(path),
            Err(SimulationStoreOpenError::InvalidExistingLedger { .. })
        ));
    }

    fn assert_invalid(
        path: &Path,
        expected_epoch: Option<SimulationLedgerEpoch>,
        expected_class: SimulationLedgerInvalid,
    ) {
        match SimulationStore::open_at(path) {
            Err(SimulationStoreOpenError::InvalidExistingLedger { ledger_epoch, class }) => {
                assert_eq!(ledger_epoch, expected_epoch);
                assert_eq!(class, expected_class);
            }
            other => panic!("unexpected ledger-open result: {other:?}"),
        }
    }

    fn rewrite_chain(path: &Path, values: &mut [Value]) {
        let mut prior_hash = B256::ZERO;
        for (sequence, value) in values.iter_mut().enumerate() {
            value["priorRecordHash"] = Value::String(SimulationStore::hex(prior_hash));
            let bytes = serde_json::to_vec(value).unwrap();
            fs::write(
                path.join(SimulationStore::record_name(u64::try_from(sequence).unwrap())),
                &bytes,
            )
            .unwrap();
            prior_hash = keccak256(bytes);
        }
        let head_path = path.join(HEAD_FILE);
        let mut head = SimulationLedgerHead::decode(&fs::read(&head_path).unwrap()).unwrap();
        head.next_sequence = u64::try_from(values.len()).unwrap();
        head.latest_record_hash = prior_hash;
        fs::write(head_path, head.encode()).unwrap();
    }

    fn assert_mutation_rejected(
        expected_epoch: bool,
        expected_class: SimulationLedgerInvalid,
        mutate: impl FnOnce(&mut Value),
    ) {
        let (path, epoch) = populated();
        let record_path = path.join(SimulationStore::record_name(0));
        let value: Value = serde_json::from_slice(&fs::read(record_path).unwrap()).unwrap();
        let mut values = [value];
        mutate(&mut values[0]);
        rewrite_chain(&path, &mut values);
        assert_invalid(&path, expected_epoch.then_some(epoch), expected_class);
        fs::remove_dir_all(path).unwrap();
    }

    fn assert_schema_rejected(mutate: impl FnOnce(&mut Value)) {
        assert_mutation_rejected(true, SimulationLedgerInvalid::Schema, mutate);
    }

    fn assert_epoch_rejected(mutate: impl FnOnce(&mut Value)) {
        assert_mutation_rejected(false, SimulationLedgerInvalid::Epoch, mutate);
    }

    #[test]
    fn initialized_and_populated_ledgers_reopen_with_exact_head_layout() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        let initialized_head = fs::read(path.join(HEAD_FILE)).unwrap();
        assert_eq!(initialized_head.len(), SimulationLedgerHead::ENCODED_LEN);
        assert_eq!(&initialized_head[..32], epoch.as_bytes());
        assert_eq!(&initialized_head[32..40], &0_u64.to_be_bytes());
        assert_eq!(&initialized_head[40..], B256::ZERO.as_slice());
        drop(store);

        let mut reopened = SimulationStore::open_at(&path).unwrap();
        let record = SimulationRecord::for_store_test(economics());
        let persisted = reopened.append(&record).unwrap();
        assert_eq!(persisted.correlation().ledger_epoch(), epoch);
        assert_eq!(persisted.correlation().sequence(), 0);
        assert_eq!(persisted.correlation().correlation_key(), record.correlation_key());
        drop(reopened);

        let head_bytes = fs::read(path.join(HEAD_FILE)).unwrap();
        let head = SimulationLedgerHead::decode(&head_bytes).unwrap();
        let record_bytes = fs::read(path.join(SimulationStore::record_name(0))).unwrap();
        assert_eq!(head.ledger_epoch, epoch);
        assert_eq!(head.next_sequence, 1);
        assert_eq!(head.latest_record_hash, keccak256(record_bytes));
        let reopened = SimulationStore::open_at(&path).unwrap();
        assert_eq!(reopened.epoch, epoch);
        assert_eq!(reopened.next_sequence, 1);
        drop(reopened);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn absent_ledger_opens_parent_before_create_and_requires_parent_sync() {
        let exercise = |failure: Option<&str>| {
            let events = RefCell::new(Vec::new());
            let result = SimulationStore::create_directory_with(
                Path::new("/parent/ledger"),
                |_| {
                    events.borrow_mut().push("open-parent");
                    if failure == Some("open-parent") {
                        Err(io::Error::from(io::ErrorKind::PermissionDenied))
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    events.borrow_mut().push("create-child");
                    if failure == Some("create-child") {
                        Err(io::Error::from(io::ErrorKind::AlreadyExists))
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    events.borrow_mut().push("sync-parent");
                    if failure == Some("sync-parent") {
                        Err(io::Error::from(io::ErrorKind::ReadOnlyFilesystem))
                    } else {
                        Ok(())
                    }
                },
            );
            (result.map_err(|error| error.kind()), events.into_inner())
        };

        assert_eq!(exercise(None), (Ok(()), vec!["open-parent", "create-child", "sync-parent"]));
        assert_eq!(
            exercise(Some("open-parent")),
            (Err(io::ErrorKind::PermissionDenied), vec!["open-parent"])
        );
        assert_eq!(
            exercise(Some("create-child")),
            (Err(io::ErrorKind::AlreadyExists), vec!["open-parent", "create-child"],)
        );
        assert_eq!(
            exercise(Some("sync-parent")),
            (
                Err(io::ErrorKind::ReadOnlyFilesystem),
                vec!["open-parent", "create-child", "sync-parent"],
            )
        );

        let missing_parent = temp().join("absent-parent").join("ledger");
        assert!(matches!(
            SimulationStore::open_at(&missing_parent),
            Err(SimulationStoreOpenError::Io(io::ErrorKind::NotFound))
        ));
        assert!(!missing_parent.exists());
    }

    #[test]
    fn startup_scan_rejects_over_capacity_before_record_read() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        drop(store);
        for sequence in 0..2 {
            let record_path = path.join(SimulationStore::record_name(sequence));
            fs::write(&record_path, b"unreadable").unwrap();
            fs::set_permissions(record_path, fs::Permissions::from_mode(0o000)).unwrap();
        }

        match SimulationStore::inspect_with_capacity(&path, 1) {
            Err(SimulationStoreOpenError::InvalidExistingLedger { ledger_epoch, class }) => {
                assert_eq!(ledger_epoch, Some(epoch));
                assert_eq!(class, SimulationLedgerInvalid::Sequence);
            }
            other => panic!("unexpected bounded-inspection result: {other:?}"),
        }
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn existing_empty_directory_is_rejected_without_mutation() {
        let path = temp();
        fs::create_dir(&path).unwrap();
        let before = snapshot(&path);
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: None,
                class: SimulationLedgerInvalid::Epoch,
            })
        ));
        assert_eq!(snapshot(&path), before);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn old_split_epoch_and_forty_byte_head_are_rejected_without_mutation() {
        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        drop(store);
        fs::write(path.join("epoch"), epoch.as_bytes()).unwrap();
        fs::write(path.join(HEAD_FILE), [0_u8; 40]).unwrap();
        let before = snapshot(&path);
        assert_rejected(&path);
        assert_eq!(snapshot(&path), before);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn missing_truncated_hard_linked_and_tampered_heads_are_rejected() {
        let (missing, _) = populated();
        fs::remove_file(missing.join(HEAD_FILE)).unwrap();
        assert_rejected(&missing);
        fs::remove_dir_all(missing).unwrap();

        let (truncated, _) = populated();
        fs::write(truncated.join(HEAD_FILE), [0_u8; 71]).unwrap();
        assert_rejected(&truncated);
        fs::remove_dir_all(truncated).unwrap();

        let (linked, _) = populated();
        fs::hard_link(linked.join(HEAD_FILE), linked.join("head-copy")).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&linked),
            Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: None,
                class: SimulationLedgerInvalid::FileType,
            })
        ));
        fs::remove_dir_all(linked).unwrap();

        for offset in [0_usize, 32, 71] {
            let (tampered, _) = populated();
            let head_path = tampered.join(HEAD_FILE);
            let mut bytes = fs::read(&head_path).unwrap();
            bytes[offset] ^= 1;
            fs::write(head_path, bytes).unwrap();
            assert_rejected(&tampered);
            fs::remove_dir_all(tampered).unwrap();
        }
    }

    #[test]
    fn zero_epoch_and_rolled_back_head_are_rejected() {
        let (zero, _) = populated();
        let head_path = zero.join(HEAD_FILE);
        let mut bytes = fs::read(&head_path).unwrap();
        bytes[..32].fill(0);
        fs::write(head_path, bytes).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&zero),
            Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: None,
                class: SimulationLedgerInvalid::Epoch,
            })
        ));
        fs::remove_dir_all(zero).unwrap();

        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        let record = SimulationRecord::for_store_test(economics());
        store.append(&record).unwrap();
        let first_hash = store.prior_hash;
        store.append(&record).unwrap();
        drop(store);
        let rolled_back = SimulationLedgerHead {
            ledger_epoch: epoch,
            next_sequence: 1,
            latest_record_hash: first_hash,
        };
        fs::write(path.join(HEAD_FILE), rolled_back.encode()).unwrap();
        assert_rejected(&path);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn multi_record_schema_mutation_rehashes_following_chain_before_classification() {
        let path = temp();
        let mut store = SimulationStore::open_at(&path).unwrap();
        let epoch = store.epoch;
        let record = SimulationRecord::for_store_test(economics());
        store.append(&record).unwrap();
        store.append(&record).unwrap();
        drop(store);

        let mut values: [Value; 2] = [0_u64, 1].map(|sequence| {
            serde_json::from_slice(
                &fs::read(path.join(SimulationStore::record_name(sequence))).unwrap(),
            )
            .unwrap()
        });
        values[0].as_object_mut().unwrap().remove("attempt");
        rewrite_chain(&path, &mut values);
        assert_invalid(&path, Some(epoch), SimulationLedgerInvalid::Schema);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn record_epoch_is_required_exact_nonzero_and_unique() {
        assert_epoch_rejected(|value| {
            value.as_object_mut().unwrap().remove("ledgerEpoch");
        });
        assert_epoch_rejected(|value| {
            value["ledgerEpoch"] = Value::String("0x01".into());
        });
        assert_epoch_rejected(|value| {
            value["ledgerEpoch"] = Value::String(format!("0x{}", "00".repeat(32)));
        });

        let (path, _) = populated();
        let record_path = path.join(SimulationStore::record_name(0));
        let bytes = String::from_utf8(fs::read(&record_path).unwrap()).unwrap();
        let duplicate = bytes.replacen(
            "\"ledgerEpoch\":",
            &format!("\"ledgerEpoch\":\"0x{}\",\"ledgerEpoch\":", "11".repeat(32)),
            1,
        );
        fs::write(&record_path, &duplicate).unwrap();
        let head_path = path.join(HEAD_FILE);
        let mut head = SimulationLedgerHead::decode(&fs::read(&head_path).unwrap()).unwrap();
        head.latest_record_hash = keccak256(duplicate.as_bytes());
        fs::write(head_path, head.encode()).unwrap();
        assert_invalid(&path, None, SimulationLedgerInvalid::Epoch);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn correlation_envelope_is_strict_and_matches_record_and_head() {
        assert_epoch_rejected(|value| {
            value.as_object_mut().unwrap().remove("correlation");
        });
        assert_schema_rejected(|value| {
            value["correlation"].as_object_mut().unwrap().insert("extra".into(), Value::Null);
        });
        assert_epoch_rejected(|value| {
            value["correlation"].as_object_mut().unwrap().remove("ledgerEpoch");
        });
        assert_schema_rejected(|value| {
            value["correlation"].as_object_mut().unwrap().remove("sequence");
        });
        assert_schema_rejected(|value| {
            value["correlation"].as_object_mut().unwrap().remove("correlationKey");
        });
        assert_epoch_rejected(|value| {
            value["correlation"]["ledgerEpoch"] = Value::String(format!("0x{}", "11".repeat(32)));
        });
        assert_schema_rejected(|value| {
            value["correlation"]["sequence"] = Value::from(1_u64);
        });
        assert_schema_rejected(|value| {
            value["sequence"] = Value::from(1_u64);
        });
        assert_schema_rejected(|value| {
            value["correlation"]["correlationKey"] =
                Value::String(format!("0x{}", "22".repeat(32)));
        });
    }

    #[test]
    fn correlation_key_is_recomputed_from_unhashed_join_fields() {
        let (path, epoch) = populated();
        let record_path = path.join(SimulationStore::record_name(0));
        let value: Value = serde_json::from_slice(&fs::read(record_path).unwrap()).unwrap();
        let campaign =
            SimulationStore::parse_hash(value.as_object().unwrap(), "campaignId").unwrap();
        let victim =
            SimulationStore::parse_hash(value.as_object().unwrap(), "victimTxHash").unwrap();
        let plan = SimulationStore::parse_hash(value.as_object().unwrap(), "planDigest").unwrap();
        let signed =
            SimulationStore::parse_hash(value.as_object().unwrap(), "signedTxHash").unwrap();
        let expected = SimulationStore::hex(SimulationStore::recompute_correlation_key(
            campaign, victim, plan, signed,
        ));
        assert_eq!(value["correlationKey"], expected);
        assert_eq!(value["correlation"]["correlationKey"], expected);
        assert_eq!(value["ledgerEpoch"], SimulationStore::hex_epoch(epoch));
        assert_eq!(value["correlation"]["ledgerEpoch"], SimulationStore::hex_epoch(epoch));
        fs::remove_dir_all(path).unwrap();

        assert_schema_rejected(|value| {
            value["campaignId"] = Value::String(format!("0x{}", "33".repeat(32)));
        });
        assert_schema_rejected(|value| {
            value["correlationKey"] = Value::String(format!("0x{}", "44".repeat(32)));
            value["correlation"]["correlationKey"] =
                Value::String(format!("0x{}", "44".repeat(32)));
        });
    }

    #[test]
    fn stale_open_trailing_deletion_and_exclusive_lease_fail_closed() {
        let (path, _) = populated();
        fs::remove_file(path.join(SimulationStore::record_name(0))).unwrap();
        assert_rejected(&path);
        fs::remove_dir_all(path).unwrap();

        let path = temp();
        let store = SimulationStore::open_at(&path).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::AlreadyOpen)
        ));
        drop(store);
        fs::write(path.join(HEAD_OPEN_FILE), [0_u8; 72]).unwrap();
        assert!(matches!(
            SimulationStore::open_at(&path),
            Err(SimulationStoreOpenError::InvalidExistingLedger {
                ledger_epoch: Some(_),
                class: SimulationLedgerInvalid::StaleOpen,
            })
        ));
        fs::remove_dir_all(path).unwrap();
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
}
