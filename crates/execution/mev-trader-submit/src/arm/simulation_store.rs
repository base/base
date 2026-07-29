//! Bounded, local-only persistence for inert simulation attempts.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use alloy_primitives::{B256, keccak256};
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
        fs::create_dir_all(directory)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::FileType,
            ));
        }
        let directory_handle =
            File::open(directory).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        let lease_path = directory.join(LEASE_FILE);
        let lease = redb::Database::create(&lease_path)
            .map_err(|_| SimulationStoreOpenError::AlreadyOpen)?;
        fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;

        match Self::inspect(directory) {
            Ok((epoch, next_sequence, prior_hash)) => Ok(Self {
                directory: directory.to_path_buf(),
                directory_handle,
                _lease: lease,
                epoch,
                next_sequence,
                prior_hash,
            }),
            Err(error) => Err(error),
        }
    }

    fn inspect(
        directory: &Path,
    ) -> Result<(SimulationLedgerEpoch, u64, B256), SimulationStoreOpenError> {
        let epoch_path = directory.join(EPOCH_FILE);
        let epoch = if epoch_path.exists() {
            let bytes = fs::read(&epoch_path)
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Epoch)
            })?;
            SimulationLedgerEpoch(B256::from(bytes))
        } else {
            let mut bytes = [0_u8; 32];
            OsRng.fill_bytes(&mut bytes);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&epoch_path)
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            File::open(directory)
                .and_then(|handle| handle.sync_all())
                .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            SimulationLedgerEpoch(B256::from(bytes))
        };

        let mut sequences = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?
        {
            let entry = entry.map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::UnknownEntry,
            ))?;
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
            let metadata =
                entry.metadata().map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(SimulationStoreOpenError::InvalidExistingLedger(
                    SimulationLedgerInvalid::FileType,
                ));
            }
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
        Self::validate_or_initialize_head(directory, next_sequence, prior_hash)?;
        Ok((epoch, next_sequence, prior_hash))
    }

    fn validate_or_initialize_head(
        directory: &Path,
        next_sequence: u64,
        prior_hash: B256,
    ) -> Result<(), SimulationStoreOpenError> {
        let head_path = directory.join(HEAD_FILE);
        if head_path.exists() {
            let bytes =
                fs::read(&head_path).map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
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
            return Ok(());
        }
        if next_sequence != 0 || prior_hash != B256::ZERO {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain,
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&head_path)
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        file.write_all(&Self::head_bytes(0, B256::ZERO))
            .and_then(|()| file.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;
        File::open(directory)
            .and_then(|handle| handle.sync_all())
            .map_err(|error| SimulationStoreOpenError::Io(error.kind()))
    }
    fn validate_existing(
        value: &Value,
        epoch: SimulationLedgerEpoch,
        sequence: u64,
        prior_hash: B256,
    ) -> Result<(), SimulationStoreOpenError> {
        let object = value.as_object().ok_or(SimulationStoreOpenError::InvalidExistingLedger(
            SimulationLedgerInvalid::Schema,
        ))?;
        let version = object.get("version").and_then(Value::as_u64);
        let observed_sequence = object.get("sequence").and_then(Value::as_u64);
        let observed_epoch = object.get("ledgerEpoch").and_then(Value::as_str);
        let observed_prior = object.get("priorRecordHash").and_then(Value::as_str);
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
            || !matches!(
                object.get("attempt").and_then(Value::as_str),
                Some("initial" | "attribution-retry")
            )
            || !matches!(object.get("requestChannelCount").and_then(Value::as_u64), Some(1 | 2))
            || !Self::nested_keys(value, "blockIdentity", &["number", "parentHash"])
            || !Self::nested_keys(
                value,
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
            )
            || !Self::nested_keys(
                value,
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
            )
            || !Self::nested_keys(
                value,
                "proofIdentity",
                &[
                    "binaryDigest",
                    "deploymentCodeHash",
                    "deploymentDigest",
                    "r9StoreIdentity",
                    "validUntilBlock",
                ],
            )
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Schema,
            ));
        }
        let route = object.get("route").and_then(Value::as_array).ok_or(
            SimulationStoreOpenError::InvalidExistingLedger(SimulationLedgerInvalid::Schema),
        )?;
        if route.len() != 2
            || route.iter().any(|hop| {
                hop.as_object().is_none_or(|object| {
                    !Self::has_exact_keys(object, &["adapter", "protocol", "runtimeHash"])
                })
            })
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Schema,
            ));
        }
        if version != Some(VERSION)
            || observed_sequence != Some(sequence)
            || observed_epoch != Some(Self::hex(epoch.value()).as_str())
        {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::Schema,
            ));
        }
        if observed_prior != Some(Self::hex(prior_hash).as_str()) {
            return Err(SimulationStoreOpenError::InvalidExistingLedger(
                SimulationLedgerInvalid::HashChain,
            ));
        }
        Ok(())
    }

    fn nested_keys(value: &Value, field: &str, expected: &[&str]) -> bool {
        value
            .get(field)
            .and_then(Value::as_object)
            .is_some_and(|object| Self::has_exact_keys(object, expected))
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
                "authorityBlock": economics.authority_block,
                "baseFeePerGasWei": economics.base_fee_per_gas_wei.to_string(),
                "executionGasEstimate": economics.execution_gas_estimate.to_string(),
                "expectedEvWei": economics.expected_ev_wei.map(|value| value.to_string()),
                "grossProfitWei": economics.gross_profit_wei.to_string(),
                "kickbackWei": economics.kickback_wei.to_string(),
                "l1DataFeeWei": economics.l1_data_fee_wei.to_string(),
                "l2ExecutionFeeWei": economics.l2_execution_fee_wei.to_string(),
                "retainedValueWei": economics.retained_value_wei.to_string(),
                "totalCostWei": economics.total_cost_wei.to_string(),
                "victimMaxFeePerGasWei": economics.victim_max_fee_per_gas_wei.to_string(),
                "victimPriorityFeePerGasWei": economics.victim_priority_fee_per_gas_wei.to_string(),
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
        PriorityEconomicsReceipt {
            gross_profit_wei: U256::from(1_000_000_u64),
            kickback_wei: U256::from(750_000_u64),
            retained_value_wei: U256::from(250_000_u64),
            execution_gas_estimate: U256::from(100_u64),
            l2_execution_fee_wei: U256::from(20_000_u64),
            l1_data_fee_wei: U256::from(10_000_u64),
            total_cost_wei: U256::from(30_000_u64),
            expected_ev_wei: Some(U256::from(220_000_u64)),
            authority_block: 100,
            base_fee_per_gas_wei: U256::from(100_u64),
            victim_priority_fee_per_gas_wei: U256::from(100_u64),
            victim_max_fee_per_gas_wei: U256::from(300_u64),
        }
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
}
