//! Strict owner-signed settled-loss projection consumer and rollback authority.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

use alloy_primitives::{B256, Signature, U256, keccak256};
use base_mev_trader::{DrawdownInput, LossProvenance};
#[cfg(not(test))]
use base_mev_trader::OWNER_ATTEST_ADDRESS;

use super::{DrawdownAuthority, ProviderError};

/// Compile-pinned terminal projection path.
pub const SETTLED_LOSS_PROJECTION_PATH: &str =
    "/home/ubuntu/.local/state/base-mev/settled-loss-v1/projection.bin";
/// Compile-pinned frozen population manifest path.
pub const P2_POPULATION_MANIFEST_PATH: &str =
    "/home/ubuntu/.local/state/base-mev/p2-population-v1/manifest.bin";
/// Compile-pinned accepted projection head path.
pub const SETTLED_LOSS_ANCHOR_PATH: &str =
    "/home/ubuntu/.local/state/base-mev/settled-loss-v1/accepted-head";
/// Compile-pinned production install bundle path.
pub const T4E_INSTALL_BUNDLE_PATH: &str =
    "/home/ubuntu/.local/state/base-mev/t4e-install-v1/authority.bundle";
/// Compile-pinned victim claim store path.
pub const R9_CLAIM_STORE_PATH: &str =
    "/home/ubuntu/.local/state/base-mev/r9-victim-claims-v1/claims.redb";
/// Terminal projection signature domain.
pub const SETTLED_LOSS_DOMAIN: &[u8; 35] = b"base-mev/t4e-terminal-settlement/v1";
/// Frozen population signature domain.
pub const POPULATION_CLOSURE_DOMAIN: &[u8; 33] = b"base-mev/p2-population-closure/v1";
/// Production install bundle signature domain.
pub const INSTALL_BUNDLE_DOMAIN: &[u8; 30] = b"base-mev/t4e-install-bundle/v1";
/// Wire schema version.
pub const SETTLED_LOSS_SCHEMA_VERSION: u16 = 1;
/// Base mainnet chain identifier.
pub const SETTLED_LOSS_CHAIN_ID: u64 = 8453;
/// Maximum accepted finalized-head lag.
pub const MAX_FINALIZED_HEAD_LAG: u64 = 128;
/// Maximum population and terminal entry count.
pub const MAX_TERMINAL_ENTRIES: usize = 200_000;
/// Maximum canonical projection size.
pub const MAX_PROJECTION_BYTES: usize = 128 * 1024 * 1024;
/// Maximum canonical population-manifest size.
pub const MAX_POPULATION_MANIFEST_BYTES: usize = 40 * 1024 * 1024;
/// Exact maximum canonical projection size at 200,000 entries.
pub const MAX_CANONICAL_PROJECTION_BYTES: usize = 114_400_664;
/// Exact maximum canonical population-manifest size at 200,000 entries.
pub const MAX_CANONICAL_POPULATION_BYTES: usize = 33_800_256;
/// Fixed rollback-anchor size.
pub const ACCEPTED_HEAD_BYTES: usize = 136;
/// Fixed source manifest entry size.
pub const SOURCE_ENTRY_BYTES: usize = 169;
/// Fixed terminal settlement entry size.
pub const TERMINAL_ENTRY_BYTES: usize = 403;

const SECP256K1_ORDER: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];
const SECP256K1_HALF_ORDER: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

/// Canonical terminal classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKindV1 {
    /// Successful submitted transaction.
    Successful,
    /// Reverted submitted transaction.
    Reverted,
    /// Proven fee-only cancellation replacement.
    Ejected,
    /// Terminal evidence remains unresolved.
    Unresolved,
}

impl TerminalKindV1 {
    fn encode(self) -> u8 {
        match self {
            Self::Successful => 1,
            Self::Reverted => 2,
            Self::Ejected => 4,
            Self::Unresolved => 5,
        }
    }

    fn decode(value: u8) -> Result<Self, SettledLossUnavailableReason> {
        match value {
            1 => Ok(Self::Successful),
            2 => Ok(Self::Reverted),
            4 => Ok(Self::Ejected),
            5 => Ok(Self::Unresolved),
            0 | 3 | 6..=u8::MAX => Err(SettledLossUnavailableReason::Malformed),
        }
    }
}

/// Closed unresolved-evidence classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReasonV1 {
    /// A resolved terminal has no unresolved reason.
    None,
    /// Required receipt or header evidence is absent.
    ReceiptMissing,
    /// Receipt evidence conflicts.
    ReceiptConflict,
    /// A complete call trace is absent.
    TraceMissing,
    /// A required fee formula cannot be established.
    FormulaUnknown,
    /// Pair attribution is incomplete or ambiguous.
    PairAttributionIncomplete,
    /// Finality evidence is insufficient.
    FinalityInsufficient,
    /// Canonical chain evidence conflicts.
    CanonicalityConflict,
}

impl UnresolvedReasonV1 {
    fn encode(self) -> u8 {
        match self {
            Self::None => 0,
            Self::ReceiptMissing => 1,
            Self::ReceiptConflict => 2,
            Self::TraceMissing => 3,
            Self::FormulaUnknown => 4,
            Self::PairAttributionIncomplete => 5,
            Self::FinalityInsufficient => 6,
            Self::CanonicalityConflict => 7,
        }
    }

    fn decode(value: u8) -> Result<Self, SettledLossUnavailableReason> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::ReceiptMissing),
            2 => Ok(Self::ReceiptConflict),
            3 => Ok(Self::TraceMissing),
            4 => Ok(Self::FormulaUnknown),
            5 => Ok(Self::PairAttributionIncomplete),
            6 => Ok(Self::FinalityInsufficient),
            7 => Ok(Self::CanonicalityConflict),
            8..=u8::MAX => Err(SettledLossUnavailableReason::Malformed),
        }
    }
}

/// One immutable member of the owner-signed P2 population.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSubmissionManifestEntryV1 {
    submission_sequence: u64,
    source_submission_id: B256,
    target_tx_hash: B256,
    correlation_key: B256,
    candidate_signed_tx_hash: B256,
    our_backrun_tx_hash: B256,
}

impl SourceSubmissionManifestEntryV1 {
    /// Constructs one fixed-width source entry. Exactly one submitted hash is encoded.
    pub const fn new(
        submission_sequence: u64,
        source_submission_id: B256,
        target_tx_hash: B256,
        correlation_key: B256,
        candidate_signed_tx_hash: B256,
        our_backrun_tx_hash: B256,
    ) -> Self {
        Self {
            submission_sequence,
            source_submission_id,
            target_tx_hash,
            correlation_key,
            candidate_signed_tx_hash,
            our_backrun_tx_hash,
        }
    }

    /// Returns the immutable submission sequence.
    pub const fn submission_sequence(&self) -> u64 {
        self.submission_sequence
    }

    /// Returns the canonical source-submission identity.
    pub const fn source_submission_id(&self) -> B256 {
        self.source_submission_id
    }

    /// Returns the candidate correlation key.
    pub const fn correlation_key(&self) -> B256 {
        self.correlation_key
    }

    /// Returns the sole submitted transaction hash.
    pub const fn our_backrun_tx_hash(&self) -> B256 {
        self.our_backrun_tx_hash
    }

    fn encode_into(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.submission_sequence.to_be_bytes());
        output.extend_from_slice(self.source_submission_id.as_slice());
        output.extend_from_slice(self.target_tx_hash.as_slice());
        output.extend_from_slice(self.correlation_key.as_slice());
        output.extend_from_slice(self.candidate_signed_tx_hash.as_slice());
        output.push(1);
        output.extend_from_slice(self.our_backrun_tx_hash.as_slice());
    }

    fn decode(reader: &mut CanonicalReader<'_>) -> Result<Self, SettledLossUnavailableReason> {
        let entry = Self {
            submission_sequence: reader.u64()?,
            source_submission_id: reader.b256()?,
            target_tx_hash: reader.b256()?,
            correlation_key: reader.b256()?,
            candidate_signed_tx_hash: reader.b256()?,
            our_backrun_tx_hash: {
                if reader.u8()? != 1 {
                    return Err(SettledLossUnavailableReason::Malformed);
                }
                reader.b256()?
            },
        };
        Ok(entry)
    }
}

/// One terminal settlement row joined to the frozen population.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSettlementEntryV1 {
    submission_sequence: u64,
    source_submission_id: B256,
    correlation_key: B256,
    candidate_signed_tx_hash: B256,
    our_backrun_tx_hash: B256,
    terminal: TerminalKindV1,
    unresolved_reason: UnresolvedReasonV1,
    terminal_block_number: u64,
    terminal_block_hash: B256,
    execution_gas_loss_wei: U256,
    l1_data_fee_loss_wei: U256,
    operator_fee_loss_wei: U256,
    kickback_loss_wei: U256,
    ejection_loss_wei: U256,
    settled_loss_wei: U256,
    realized_profit_wei: U256,
}

impl TerminalSettlementEntryV1 {
    /// Constructs a terminal entry; projection validation checks all terminal and sum invariants.
    #[expect(clippy::too_many_arguments, reason = "the canonical fixed-order wire row is explicit")]
    pub const fn new(
        submission_sequence: u64,
        source_submission_id: B256,
        correlation_key: B256,
        candidate_signed_tx_hash: B256,
        our_backrun_tx_hash: B256,
        terminal: TerminalKindV1,
        unresolved_reason: UnresolvedReasonV1,
        terminal_block_number: u64,
        terminal_block_hash: B256,
        execution_gas_loss_wei: U256,
        l1_data_fee_loss_wei: U256,
        operator_fee_loss_wei: U256,
        kickback_loss_wei: U256,
        ejection_loss_wei: U256,
        settled_loss_wei: U256,
        realized_profit_wei: U256,
    ) -> Self {
        Self {
            submission_sequence,
            source_submission_id,
            correlation_key,
            candidate_signed_tx_hash,
            our_backrun_tx_hash,
            terminal,
            unresolved_reason,
            terminal_block_number,
            terminal_block_hash,
            execution_gas_loss_wei,
            l1_data_fee_loss_wei,
            operator_fee_loss_wei,
            kickback_loss_wei,
            ejection_loss_wei,
            settled_loss_wei,
            realized_profit_wei,
        }
    }

    /// Returns the terminal kind.
    pub const fn terminal(&self) -> TerminalKindV1 {
        self.terminal
    }

    /// Returns the unresolved reason.
    pub const fn unresolved_reason(&self) -> UnresolvedReasonV1 {
        self.unresolved_reason
    }

    /// Returns the settled-loss amount.
    pub const fn settled_loss_wei(&self) -> U256 {
        self.settled_loss_wei
    }

    fn encode_into(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.submission_sequence.to_be_bytes());
        output.extend_from_slice(self.source_submission_id.as_slice());
        output.extend_from_slice(self.correlation_key.as_slice());
        output.extend_from_slice(self.candidate_signed_tx_hash.as_slice());
        output.push(1);
        output.extend_from_slice(self.our_backrun_tx_hash.as_slice());
        output.push(self.terminal.encode());
        output.push(self.unresolved_reason.encode());
        output.extend_from_slice(&self.terminal_block_number.to_be_bytes());
        output.extend_from_slice(self.terminal_block_hash.as_slice());
        encode_u256(output, self.execution_gas_loss_wei);
        encode_u256(output, self.l1_data_fee_loss_wei);
        encode_u256(output, self.operator_fee_loss_wei);
        encode_u256(output, self.kickback_loss_wei);
        encode_u256(output, self.ejection_loss_wei);
        encode_u256(output, self.settled_loss_wei);
        encode_u256(output, self.realized_profit_wei);
    }

    fn decode(reader: &mut CanonicalReader<'_>) -> Result<Self, SettledLossUnavailableReason> {
        let submission_sequence = reader.u64()?;
        let source_submission_id = reader.b256()?;
        let correlation_key = reader.b256()?;
        let candidate_signed_tx_hash = reader.b256()?;
        if reader.u8()? != 1 {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        Ok(Self {
            submission_sequence,
            source_submission_id,
            correlation_key,
            candidate_signed_tx_hash,
            our_backrun_tx_hash: reader.b256()?,
            terminal: TerminalKindV1::decode(reader.u8()?)?,
            unresolved_reason: UnresolvedReasonV1::decode(reader.u8()?)?,
            terminal_block_number: reader.u64()?,
            terminal_block_hash: reader.b256()?,
            execution_gas_loss_wei: reader.u256()?,
            l1_data_fee_loss_wei: reader.u256()?,
            operator_fee_loss_wei: reader.u256()?,
            kickback_loss_wei: reader.u256()?,
            ejection_loss_wei: reader.u256()?,
            settled_loss_wei: reader.u256()?,
            realized_profit_wei: reader.u256()?,
        })
    }
}

/// Canonical independently signed frozen population manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenP2PopulationManifestV1 {
    campaign_id: B256,
    chain_id: u64,
    source_window_start_ms: u64,
    source_window_end_ms: u64,
    source_snapshot_xmin: u64,
    source_snapshot_xmax: u64,
    source_snapshot_xip_hash: B256,
    source_snapshot_wal_lsn: u64,
    submission_count: u64,
    source_manifest_hash: B256,
    entries: Vec<SourceSubmissionManifestEntryV1>,
    signature: [u8; 65],
}

impl FrozenP2PopulationManifestV1 {
    /// Decodes, canonicalizes, authenticates, and validates a frozen population.
    pub fn decode_checked(bytes: &[u8]) -> Result<Self, SettledLossUnavailableReason> {
        if bytes.is_empty() || bytes.len() > MAX_POPULATION_MANIFEST_BYTES {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let mut reader = CanonicalReader::new(bytes);
        if reader.array::<33>()? != *POPULATION_CLOSURE_DOMAIN
            || reader.u16()? != SETTLED_LOSS_SCHEMA_VERSION
        {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let campaign_id = reader.b256()?;
        let chain_id = reader.u64()?;
        let source_window_start_ms = reader.u64()?;
        let source_window_end_ms = reader.u64()?;
        let source_snapshot_xmin = reader.u64()?;
        let source_snapshot_xmax = reader.u64()?;
        let source_snapshot_xip_hash = reader.b256()?;
        let source_snapshot_wal_lsn = reader.u64()?;
        let submission_count = reader.u64()?;
        let source_manifest_hash = reader.b256()?;
        let count = reader.u32()? as usize;
        validate_count_before_allocation(count, reader.remaining(), SOURCE_ENTRY_BYTES, 65)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(SourceSubmissionManifestEntryV1::decode(&mut reader)?);
        }
        let signature = reader.array::<65>()?;
        reader.finish()?;
        let manifest = Self {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
            submission_count,
            source_manifest_hash,
            entries,
            signature,
        };
        let canonical = manifest.encode();
        if canonical != bytes {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        manifest.validate_structure()?;
        verify_canonical_signature(&manifest.signature, &canonical[..canonical.len() - 65])?;
        Ok(manifest)
    }

    /// Returns the exact canonical bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(256 + self.entries.len() * SOURCE_ENTRY_BYTES);
        output.extend_from_slice(POPULATION_CLOSURE_DOMAIN);
        output.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        output.extend_from_slice(self.campaign_id.as_slice());
        output.extend_from_slice(&self.chain_id.to_be_bytes());
        output.extend_from_slice(&self.source_window_start_ms.to_be_bytes());
        output.extend_from_slice(&self.source_window_end_ms.to_be_bytes());
        output.extend_from_slice(&self.source_snapshot_xmin.to_be_bytes());
        output.extend_from_slice(&self.source_snapshot_xmax.to_be_bytes());
        output.extend_from_slice(self.source_snapshot_xip_hash.as_slice());
        output.extend_from_slice(&self.source_snapshot_wal_lsn.to_be_bytes());
        output.extend_from_slice(&self.submission_count.to_be_bytes());
        output.extend_from_slice(self.source_manifest_hash.as_slice());
        output.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for entry in &self.entries {
            entry.encode_into(&mut output);
        }
        output.extend_from_slice(&self.signature);
        output
    }

    /// Returns the authenticated campaign identifier.
    pub const fn campaign_id(&self) -> B256 {
        self.campaign_id
    }

    /// Returns the authenticated entry inventory.
    pub fn entries(&self) -> &[SourceSubmissionManifestEntryV1] {
        &self.entries
    }

    fn validate_structure(&self) -> Result<(), SettledLossUnavailableReason> {
        let count = self.entries.len();
        if self.chain_id != SETTLED_LOSS_CHAIN_ID
            || self.campaign_id == B256::ZERO
            || self.source_window_start_ms >= self.source_window_end_ms
            || self.source_snapshot_xmin > self.source_snapshot_xmax
            || count == 0
            || count > MAX_TERMINAL_ENTRIES
            || self.submission_count != count as u64
        {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        for (sequence, entry) in self.entries.iter().enumerate() {
            if entry.submission_sequence != sequence as u64 {
                return Err(SettledLossUnavailableReason::Malformed);
            }
        }
        let mut encoded_entries = Vec::with_capacity(count * SOURCE_ENTRY_BYTES);
        for entry in &self.entries {
            entry.encode_into(&mut encoded_entries);
        }
        if keccak256(encoded_entries) != self.source_manifest_hash {
            return Err(SettledLossUnavailableReason::AuthenticationFailed);
        }
        Ok(())
    }
}

/// Canonical owner-signed terminal settlement projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSettlementProjectionV1 {
    campaign_id: B256,
    chain_id: u64,
    source_window_start_ms: u64,
    source_window_end_ms: u64,
    source_snapshot_xmin: u64,
    source_snapshot_xmax: u64,
    source_snapshot_xip_hash: B256,
    source_snapshot_wal_lsn: u64,
    projection_sequence: u64,
    manifest_start_sequence: u64,
    manifest_next_sequence: u64,
    submission_count: u64,
    terminal_count: u64,
    complete: bool,
    unresolved_count: u64,
    source_manifest_hash: B256,
    population_closure_signature: [u8; 65],
    finalized_block_number: u64,
    finalized_block_hash: B256,
    previous_content_hash: B256,
    total_execution_gas_loss_wei: U256,
    total_l1_data_fee_loss_wei: U256,
    total_operator_fee_loss_wei: U256,
    total_kickback_loss_wei: U256,
    total_ejection_loss_wei: U256,
    total_settled_loss_wei: U256,
    source_manifest_entries: Vec<SourceSubmissionManifestEntryV1>,
    terminal_entries: Vec<TerminalSettlementEntryV1>,
    content_hash: B256,
    signature: [u8; 65],
}

impl TerminalSettlementProjectionV1 {
    /// Decodes, canonicalizes, authenticates, and validates a terminal projection.
    pub fn decode_checked(bytes: &[u8]) -> Result<Self, SettledLossUnavailableReason> {
        if bytes.is_empty() || bytes.len() > MAX_PROJECTION_BYTES {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let mut reader = CanonicalReader::new(bytes);
        if reader.array::<35>()? != *SETTLED_LOSS_DOMAIN
            || reader.u16()? != SETTLED_LOSS_SCHEMA_VERSION
        {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let campaign_id = reader.b256()?;
        let chain_id = reader.u64()?;
        let source_window_start_ms = reader.u64()?;
        let source_window_end_ms = reader.u64()?;
        let source_snapshot_xmin = reader.u64()?;
        let source_snapshot_xmax = reader.u64()?;
        let source_snapshot_xip_hash = reader.b256()?;
        let source_snapshot_wal_lsn = reader.u64()?;
        let projection_sequence = reader.u64()?;
        let manifest_start_sequence = reader.u64()?;
        let manifest_next_sequence = reader.u64()?;
        let submission_count = reader.u64()?;
        let terminal_count = reader.u64()?;
        let complete = match reader.u8()? {
            0 => false,
            1 => true,
            2..=u8::MAX => return Err(SettledLossUnavailableReason::Malformed),
        };
        let unresolved_count = reader.u64()?;
        let source_manifest_hash = reader.b256()?;
        let population_closure_signature = reader.array::<65>()?;
        let finalized_block_number = reader.u64()?;
        let finalized_block_hash = reader.b256()?;
        let previous_content_hash = reader.b256()?;
        let total_execution_gas_loss_wei = reader.u256()?;
        let total_l1_data_fee_loss_wei = reader.u256()?;
        let total_operator_fee_loss_wei = reader.u256()?;
        let total_kickback_loss_wei = reader.u256()?;
        let total_ejection_loss_wei = reader.u256()?;
        let total_settled_loss_wei = reader.u256()?;
        let source_count = reader.u32()? as usize;
        validate_count_before_allocation(
            source_count,
            reader.remaining(),
            SOURCE_ENTRY_BYTES,
            4 + 32 + 65,
        )?;
        let mut source_manifest_entries = Vec::with_capacity(source_count);
        for _ in 0..source_count {
            source_manifest_entries.push(SourceSubmissionManifestEntryV1::decode(&mut reader)?);
        }
        let terminal_count_wire = reader.u32()? as usize;
        validate_count_before_allocation(
            terminal_count_wire,
            reader.remaining(),
            TERMINAL_ENTRY_BYTES,
            32 + 65,
        )?;
        let mut terminal_entries = Vec::with_capacity(terminal_count_wire);
        for _ in 0..terminal_count_wire {
            terminal_entries.push(TerminalSettlementEntryV1::decode(&mut reader)?);
        }
        let content_hash = reader.b256()?;
        let signature = reader.array::<65>()?;
        reader.finish()?;
        let projection = Self {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
            projection_sequence,
            manifest_start_sequence,
            manifest_next_sequence,
            submission_count,
            terminal_count,
            complete,
            unresolved_count,
            source_manifest_hash,
            population_closure_signature,
            finalized_block_number,
            finalized_block_hash,
            previous_content_hash,
            total_execution_gas_loss_wei,
            total_l1_data_fee_loss_wei,
            total_operator_fee_loss_wei,
            total_kickback_loss_wei,
            total_ejection_loss_wei,
            total_settled_loss_wei,
            source_manifest_entries,
            terminal_entries,
            content_hash,
            signature,
        };
        let canonical = projection.encode();
        if canonical != bytes {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        projection.validate_structure()?;
        let body_len = canonical.len() - 32 - 65;
        let mut hash_preimage = Vec::with_capacity(SETTLED_LOSS_DOMAIN.len() + body_len);
        hash_preimage.extend_from_slice(SETTLED_LOSS_DOMAIN);
        hash_preimage.extend_from_slice(&canonical[..body_len]);
        if keccak256(hash_preimage) != projection.content_hash {
            return Err(SettledLossUnavailableReason::AuthenticationFailed);
        }
        let mut signature_preimage = Vec::with_capacity(SETTLED_LOSS_DOMAIN.len() + 32);
        signature_preimage.extend_from_slice(SETTLED_LOSS_DOMAIN);
        signature_preimage.extend_from_slice(projection.content_hash.as_slice());
        verify_canonical_signature(&projection.signature, &signature_preimage)?;
        Ok(projection)
    }

    /// Builds the sole canonical unsigned projection body and its owner-signature preimage.
    #[expect(clippy::too_many_arguments, reason = "the canonical projection header is explicit")]
    pub fn prepare_unsigned(
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
        source_manifest_entries: Vec<SourceSubmissionManifestEntryV1>,
        terminal_entries: Vec<TerminalSettlementEntryV1>,
    ) -> Result<(Vec<u8>, Vec<u8>), SettledLossUnavailableReason> {
        let count = u64::try_from(source_manifest_entries.len())
            .map_err(|_| SettledLossUnavailableReason::Malformed)?;
        if terminal_entries.iter().any(|entry| {
            entry.terminal != TerminalKindV1::Unresolved
                && entry.terminal_block_number > finalized_block_number
        }) {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let mut totals = [U256::ZERO; 6];
        for terminal in &terminal_entries {
            totals[0] = checked_add(totals[0], terminal.execution_gas_loss_wei)?;
            totals[1] = checked_add(totals[1], terminal.l1_data_fee_loss_wei)?;
            totals[2] = checked_add(totals[2], terminal.operator_fee_loss_wei)?;
            totals[3] = checked_add(totals[3], terminal.kickback_loss_wei)?;
            totals[4] = checked_add(totals[4], terminal.ejection_loss_wei)?;
            totals[5] = checked_add(totals[5], terminal.settled_loss_wei)?;
        }
        let mut projection = Self {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
            projection_sequence,
            manifest_start_sequence: 0,
            manifest_next_sequence: count,
            submission_count: count,
            terminal_count: count,
            complete: true,
            unresolved_count: 0,
            source_manifest_hash,
            population_closure_signature,
            finalized_block_number,
            finalized_block_hash,
            previous_content_hash,
            total_execution_gas_loss_wei: totals[0],
            total_l1_data_fee_loss_wei: totals[1],
            total_operator_fee_loss_wei: totals[2],
            total_kickback_loss_wei: totals[3],
            total_ejection_loss_wei: totals[4],
            total_settled_loss_wei: totals[5],
            source_manifest_entries,
            terminal_entries,
            content_hash: B256::ZERO,
            signature: [0; 65],
        };
        projection.validate_structure()?;
        let canonical = projection.encode();
        let body_len = canonical.len() - 32 - 65;
        let mut hash_preimage = Vec::with_capacity(SETTLED_LOSS_DOMAIN.len() + body_len);
        hash_preimage.extend_from_slice(SETTLED_LOSS_DOMAIN);
        hash_preimage.extend_from_slice(&canonical[..body_len]);
        projection.content_hash = keccak256(hash_preimage);
        let mut canonical_body = projection.encode();
        canonical_body.truncate(canonical_body.len() - 65);
        let mut signature_preimage = Vec::with_capacity(SETTLED_LOSS_DOMAIN.len() + 32);
        signature_preimage.extend_from_slice(SETTLED_LOSS_DOMAIN);
        signature_preimage.extend_from_slice(projection.content_hash.as_slice());
        Ok((canonical_body, signature_preimage))
    }

    /// Returns the exact canonical projection bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(
            664 + self.source_manifest_entries.len() * SOURCE_ENTRY_BYTES
                + self.terminal_entries.len() * TERMINAL_ENTRY_BYTES,
        );
        output.extend_from_slice(SETTLED_LOSS_DOMAIN);
        output.extend_from_slice(&SETTLED_LOSS_SCHEMA_VERSION.to_be_bytes());
        output.extend_from_slice(self.campaign_id.as_slice());
        output.extend_from_slice(&self.chain_id.to_be_bytes());
        output.extend_from_slice(&self.source_window_start_ms.to_be_bytes());
        output.extend_from_slice(&self.source_window_end_ms.to_be_bytes());
        output.extend_from_slice(&self.source_snapshot_xmin.to_be_bytes());
        output.extend_from_slice(&self.source_snapshot_xmax.to_be_bytes());
        output.extend_from_slice(self.source_snapshot_xip_hash.as_slice());
        output.extend_from_slice(&self.source_snapshot_wal_lsn.to_be_bytes());
        output.extend_from_slice(&self.projection_sequence.to_be_bytes());
        output.extend_from_slice(&self.manifest_start_sequence.to_be_bytes());
        output.extend_from_slice(&self.manifest_next_sequence.to_be_bytes());
        output.extend_from_slice(&self.submission_count.to_be_bytes());
        output.extend_from_slice(&self.terminal_count.to_be_bytes());
        output.push(u8::from(self.complete));
        output.extend_from_slice(&self.unresolved_count.to_be_bytes());
        output.extend_from_slice(self.source_manifest_hash.as_slice());
        output.extend_from_slice(&self.population_closure_signature);
        output.extend_from_slice(&self.finalized_block_number.to_be_bytes());
        output.extend_from_slice(self.finalized_block_hash.as_slice());
        output.extend_from_slice(self.previous_content_hash.as_slice());
        encode_u256(&mut output, self.total_execution_gas_loss_wei);
        encode_u256(&mut output, self.total_l1_data_fee_loss_wei);
        encode_u256(&mut output, self.total_operator_fee_loss_wei);
        encode_u256(&mut output, self.total_kickback_loss_wei);
        encode_u256(&mut output, self.total_ejection_loss_wei);
        encode_u256(&mut output, self.total_settled_loss_wei);
        output.extend_from_slice(&(self.source_manifest_entries.len() as u32).to_be_bytes());
        for entry in &self.source_manifest_entries {
            entry.encode_into(&mut output);
        }
        output.extend_from_slice(&(self.terminal_entries.len() as u32).to_be_bytes());
        for entry in &self.terminal_entries {
            entry.encode_into(&mut output);
        }
        output.extend_from_slice(self.content_hash.as_slice());
        output.extend_from_slice(&self.signature);
        output
    }

    /// Returns the authenticated campaign identifier.
    pub const fn campaign_id(&self) -> B256 {
        self.campaign_id
    }

    /// Returns the authenticated total settled loss.
    pub const fn total_settled_loss_wei(&self) -> U256 {
        self.total_settled_loss_wei
    }

    /// Returns the projection sequence.
    pub const fn projection_sequence(&self) -> u64 {
        self.projection_sequence
    }

    /// Returns the projection finalized block.
    pub const fn finalized_block(&self) -> BlockNumHash {
        BlockNumHash { number: self.finalized_block_number, hash: self.finalized_block_hash }
    }

    fn validate_structure(&self) -> Result<(), SettledLossUnavailableReason> {
        let source_count = self.source_manifest_entries.len();
        let terminal_count = self.terminal_entries.len();
        let expected_count = self
            .manifest_next_sequence
            .checked_sub(self.manifest_start_sequence)
            .ok_or(SettledLossUnavailableReason::Malformed)?;
        if self.chain_id != SETTLED_LOSS_CHAIN_ID
            || self.campaign_id == B256::ZERO
            || self.source_window_start_ms >= self.source_window_end_ms
            || self.source_snapshot_xmin > self.source_snapshot_xmax
            || self.projection_sequence == 0
            || self.manifest_start_sequence != 0
            || expected_count == 0
            || expected_count > MAX_TERMINAL_ENTRIES as u64
            || self.submission_count != expected_count
            || source_count as u64 != expected_count
            || terminal_count as u64 != expected_count
            || self
                .terminal_count
                .checked_add(self.unresolved_count)
                .ok_or(SettledLossUnavailableReason::Malformed)?
                != self.submission_count
        {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        let mut source_bytes = Vec::with_capacity(source_count * SOURCE_ENTRY_BYTES);
        let mut totals = [U256::ZERO; 6];
        let mut resolved = 0u64;
        let mut unresolved = 0u64;
        for (index, (source, terminal)) in
            self.source_manifest_entries.iter().zip(&self.terminal_entries).enumerate()
        {
            let sequence = self
                .manifest_start_sequence
                .checked_add(index as u64)
                .ok_or(SettledLossUnavailableReason::Malformed)?;
            if source.submission_sequence != sequence
                || terminal.submission_sequence != sequence
                || source.source_submission_id != terminal.source_submission_id
                || source.correlation_key != terminal.correlation_key
                || source.candidate_signed_tx_hash != terminal.candidate_signed_tx_hash
                || source.our_backrun_tx_hash != terminal.our_backrun_tx_hash
            {
                return Err(SettledLossUnavailableReason::ManifestMismatch);
            }
            source.encode_into(&mut source_bytes);
            validate_terminal_entry(terminal)?;
            if terminal.terminal == TerminalKindV1::Unresolved {
                unresolved =
                    unresolved.checked_add(1).ok_or(SettledLossUnavailableReason::Malformed)?;
            } else {
                resolved =
                    resolved.checked_add(1).ok_or(SettledLossUnavailableReason::Malformed)?;
            }
            totals[0] = checked_add(totals[0], terminal.execution_gas_loss_wei)?;
            totals[1] = checked_add(totals[1], terminal.l1_data_fee_loss_wei)?;
            totals[2] = checked_add(totals[2], terminal.operator_fee_loss_wei)?;
            totals[3] = checked_add(totals[3], terminal.kickback_loss_wei)?;
            totals[4] = checked_add(totals[4], terminal.ejection_loss_wei)?;
            totals[5] = checked_add(totals[5], terminal.settled_loss_wei)?;
        }
        if keccak256(source_bytes) != self.source_manifest_hash
            || resolved != self.terminal_count
            || unresolved != self.unresolved_count
            || totals
                != [
                    self.total_execution_gas_loss_wei,
                    self.total_l1_data_fee_loss_wei,
                    self.total_operator_fee_loss_wei,
                    self.total_kickback_loss_wei,
                    self.total_ejection_loss_wei,
                    self.total_settled_loss_wei,
                ]
            || checked_component_sum(&totals[..5])? != self.total_settled_loss_wei
        {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        Ok(())
    }

    fn availability(&self) -> Result<(), SettledLossUnavailableReason> {
        if !self.complete {
            return Err(SettledLossUnavailableReason::Incomplete);
        }
        if self.unresolved_count != 0 || self.terminal_count != self.submission_count {
            return Err(SettledLossUnavailableReason::Unresolved(self.unresolved_summary()?));
        }
        Ok(())
    }

    fn unresolved_summary(
        &self,
    ) -> Result<BoundedUnresolvedSummaryV1, SettledLossUnavailableReason> {
        let mut summary = BoundedUnresolvedSummaryV1 {
            total: 0,
            first_sequence: 0,
            first_reason: UnresolvedReasonV1::ReceiptMissing,
            reason_counts: [0; 7],
        };
        for entry in &self.terminal_entries {
            if entry.terminal != TerminalKindV1::Unresolved {
                continue;
            }
            let reason_index = match entry.unresolved_reason {
                UnresolvedReasonV1::None => return Err(SettledLossUnavailableReason::Malformed),
                UnresolvedReasonV1::ReceiptMissing => 0,
                UnresolvedReasonV1::ReceiptConflict => 1,
                UnresolvedReasonV1::TraceMissing => 2,
                UnresolvedReasonV1::FormulaUnknown => 3,
                UnresolvedReasonV1::PairAttributionIncomplete => 4,
                UnresolvedReasonV1::FinalityInsufficient => 5,
                UnresolvedReasonV1::CanonicalityConflict => 6,
            };
            if summary.total == 0 {
                summary.first_sequence = entry.submission_sequence;
                summary.first_reason = entry.unresolved_reason;
            }
            summary.total =
                summary.total.checked_add(1).ok_or(SettledLossUnavailableReason::Malformed)?;
            summary.reason_counts[reason_index] = summary.reason_counts[reason_index]
                .checked_add(1)
                .ok_or(SettledLossUnavailableReason::Malformed)?;
        }
        if summary.total != self.unresolved_count {
            return Err(SettledLossUnavailableReason::Malformed);
        }
        Ok(summary)
    }
}

/// One block number and canonical hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockNumHash {
    /// Block number.
    pub number: u64,
    /// Canonical block hash.
    pub hash: B256,
}

/// Node-local finalized-chain lookup error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizedChainError {
    /// The provider returned an error.
    Provider,
    /// The requested finalized value was absent.
    Unavailable,
    /// The requested historical value disagreed with canonical state.
    CanonicalMismatch,
}

/// Node-local canonical finalized-chain authority.
pub trait FinalizedChainAuthority: std::fmt::Debug + Send + Sync {
    /// Returns the local canonical finalized head.
    fn finalized_head(&self) -> Result<Option<BlockNumHash>, FinalizedChainError>;
    /// Returns the local canonical hash at one block number.
    fn canonical_hash(&self, number: u64) -> Result<Option<B256>, FinalizedChainError>;
}

impl<A: FinalizedChainAuthority + ?Sized> FinalizedChainAuthority for std::sync::Arc<A> {
    fn finalized_head(&self) -> Result<Option<BlockNumHash>, FinalizedChainError> {
        (**self).finalized_head()
    }

    fn canonical_hash(&self, number: u64) -> Result<Option<B256>, FinalizedChainError> {
        (**self).canonical_hash(number)
    }
}

/// Exact canonical-hash mismatch class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalMismatchClass {
    /// The projection's signed finalized hash differs from the local chain.
    ProjectionFinalizedHash,
    /// Two resolved entries assert different hashes at one height.
    TerminalHeightConflict,
    /// A resolved terminal hash differs from the local chain.
    TerminalHistoricalHash,
}

/// Bounded summary of every unresolved terminal reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedUnresolvedSummaryV1 {
    /// Total unresolved entries.
    pub total: u64,
    /// First unresolved submission sequence.
    pub first_sequence: u64,
    /// First unresolved reason.
    pub first_reason: UnresolvedReasonV1,
    /// Counts indexed by wire reasons 1 through 7.
    pub reason_counts: [u64; 7],
}

/// Closed settled-loss unavailability reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettledLossUnavailableReason {
    /// A required artifact is absent.
    Missing,
    /// The signed projection is explicitly incomplete.
    Incomplete,
    /// The signed projection contains unresolved terminals.
    Unresolved(BoundedUnresolvedSummaryV1),
    /// The projection lags the finalized head beyond the bound.
    Stale,
    /// Frozen population and projection membership differ.
    ManifestMismatch,
    /// Finalized chain state could not be established.
    FinalityUnavailable,
    /// Canonical chain evidence disagreed.
    CanonicalMismatch(CanonicalMismatchClass),
    /// Canonical wire bytes or structural invariants are malformed.
    Malformed,
    /// A canonical hash or owner signature failed.
    AuthenticationFailed,
    /// Projection sequence or previous-hash linkage rolled back.
    Rollback,
    /// Filesystem validation or durable publication failed.
    Io,
}

/// Typed result of one strict settled-loss load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettledLossLoad {
    /// Required projection or manifest is absent.
    Missing,
    /// A valid artifact is not currently usable.
    PendingOrUnresolved(SettledLossUnavailableReason),
    /// Artifact validation failed.
    Error(SettledLossUnavailableReason),
    /// Every check succeeded.
    Complete {
        /// Authenticated cumulative settled loss.
        total_settled_loss_wei: U256,
    },
}
/// Strict one-shot settled-loss reader preserving the closed load classification.
#[derive(Debug, Clone, Copy)]
pub struct SettledLossReader;

impl SettledLossReader {
    /// Rereads, authenticates, finality-checks, and rollback-checks the pinned artifacts.
    pub fn load<A: FinalizedChainAuthority>(chain: &A) -> SettledLossLoad {
        let checked = (|| {
            let (manifest, _manifest_file, projection, _projection_file) = load_artifacts()?;
            validate_manifest_equality(&manifest, &projection)?;
            projection.availability()?;
            validate_finality_snapshot(chain, &projection, true)?;
            accept_projection_head(&projection)?;
            Ok(projection.total_settled_loss_wei)
        })();
        match checked {
            Ok(total_settled_loss_wei) => SettledLossLoad::Complete { total_settled_loss_wei },
            Err(reason) => classify_load_failure(reason),
        }
    }
}

fn classify_load_failure(reason: SettledLossUnavailableReason) -> SettledLossLoad {
    match reason {
        SettledLossUnavailableReason::Missing => SettledLossLoad::Missing,
        SettledLossUnavailableReason::Incomplete => {
            SettledLossLoad::PendingOrUnresolved(SettledLossUnavailableReason::Incomplete)
        }
        SettledLossUnavailableReason::Unresolved(summary) => {
            SettledLossLoad::PendingOrUnresolved(SettledLossUnavailableReason::Unresolved(summary))
        }
        SettledLossUnavailableReason::Stale => {
            SettledLossLoad::PendingOrUnresolved(SettledLossUnavailableReason::Stale)
        }
        SettledLossUnavailableReason::ManifestMismatch => {
            SettledLossLoad::PendingOrUnresolved(SettledLossUnavailableReason::ManifestMismatch)
        }
        SettledLossUnavailableReason::FinalityUnavailable => {
            SettledLossLoad::Error(SettledLossUnavailableReason::FinalityUnavailable)
        }
        SettledLossUnavailableReason::CanonicalMismatch(class) => {
            SettledLossLoad::Error(SettledLossUnavailableReason::CanonicalMismatch(class))
        }
        SettledLossUnavailableReason::Malformed => {
            SettledLossLoad::Error(SettledLossUnavailableReason::Malformed)
        }
        SettledLossUnavailableReason::AuthenticationFailed => {
            SettledLossLoad::Error(SettledLossUnavailableReason::AuthenticationFailed)
        }
        SettledLossUnavailableReason::Rollback => {
            SettledLossLoad::Error(SettledLossUnavailableReason::Rollback)
        }
        SettledLossUnavailableReason::Io => {
            SettledLossLoad::Error(SettledLossUnavailableReason::Io)
        }
    }
}

/// Non-clone startup proof retaining the verified artifacts until worker activation.
#[derive(Debug)]
pub struct PreparedSettledLossAuthority<A> {
    chain: A,
    manifest: OpenedArtifact,
    projection_file: OpenedArtifact,
    projection: TerminalSettlementProjectionV1,
    accepted_finalized: BlockNumHash,
}

impl<A: FinalizedChainAuthority> PreparedSettledLossAuthority<A> {
    /// Returns the authenticated campaign without exposing a Ready authority.
    pub const fn campaign_id(&self) -> B256 {
        self.projection.campaign_id
    }
    /// Rechecks held artifact identity and current finality before moving authority into the worker.
    pub fn activate(
        self,
    ) -> Result<NodeLocalSettledLossAuthority<A>, SettledLossUnavailableReason> {
        self.manifest.recheck_path(Path::new(P2_POPULATION_MANIFEST_PATH))?;
        self.projection_file.recheck_path(Path::new(SETTLED_LOSS_PROJECTION_PATH))?;
        validate_finality_snapshot(&self.chain, &self.projection, false)?;
        let current = self
            .chain
            .finalized_head()
            .map_err(|_| SettledLossUnavailableReason::FinalityUnavailable)?
            .ok_or(SettledLossUnavailableReason::FinalityUnavailable)?;
        if current.number < self.accepted_finalized.number {
            return Err(SettledLossUnavailableReason::FinalityUnavailable);
        }
        Ok(NodeLocalSettledLossAuthority { chain: self.chain })
    }
}

/// Strict node-local settled-loss authority.
#[derive(Debug)]
pub struct NodeLocalSettledLossAuthority<A> {
    chain: A,
}

impl<A: FinalizedChainAuthority> NodeLocalSettledLossAuthority<A> {
    /// Performs the complete pre-spawn artifact, finality, and rollback validation.
    pub fn prepare_complete(
        chain: A,
    ) -> Result<PreparedSettledLossAuthority<A>, SettledLossUnavailableReason> {
        let (manifest, manifest_file, projection, projection_file) = load_artifacts()?;
        validate_manifest_equality(&manifest, &projection)?;
        projection.availability()?;
        let accepted_finalized = validate_finality_snapshot(&chain, &projection, true)?;
        accept_projection_head(&projection)?;
        Ok(PreparedSettledLossAuthority {
            chain,
            manifest: manifest_file,
            projection_file,
            projection,
            accepted_finalized,
        })
    }

    /// Rereads every artifact and returns only a fully checked on-chain-realized drawdown input.
    pub fn load_complete(&self) -> Result<DrawdownInput, SettledLossUnavailableReason> {
        let (manifest, _manifest_file, projection, _projection_file) = load_artifacts()?;
        validate_manifest_equality(&manifest, &projection)?;
        projection.availability()?;
        validate_finality_snapshot(&self.chain, &projection, true)?;
        accept_projection_head(&projection)?;
        Ok(DrawdownInput::Complete {
            cumulative_realized_loss_wei: projection.total_settled_loss_wei,
            provenance: LossProvenance::OnchainRealized,
        })
    }
}

impl<A: FinalizedChainAuthority> DrawdownAuthority for NodeLocalSettledLossAuthority<A> {
    fn load_drawdown(&self) -> Result<DrawdownInput, ProviderError> {
        self.load_complete().map_err(|_| ProviderError::Invalid("settled loss is unavailable"))
    }
}

#[derive(Debug)]
struct OpenedArtifact {
    file: File,
    identity: FileIdentity,
}

impl OpenedArtifact {
    fn recheck_path(&self, path: &Path) -> Result<(), SettledLossUnavailableReason> {
        let held = FileIdentity::from_metadata(
            &self.file.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
        );
        let current_file = open_final_no_follow(path)?;
        let current = FileIdentity::from_metadata(
            &current_file.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
        );
        if held != self.identity || current != self.identity {
            return Err(SettledLossUnavailableReason::Io);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    uid: u32,
    mode: u32,
    nlink: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            uid: metadata.uid(),
            mode: metadata.mode() & 0o777,
            nlink: metadata.nlink(),
        }
    }
}

fn load_artifacts() -> Result<
    (FrozenP2PopulationManifestV1, OpenedArtifact, TerminalSettlementProjectionV1, OpenedArtifact),
    SettledLossUnavailableReason,
> {
    validate_directory_inventory(
        Path::new("/home/ubuntu/.local/state/base-mev/p2-population-v1"),
        &["manifest.bin", "manifest.bin.open"],
    )?;
    let (manifest_bytes, manifest_file) =
        read_strict_file(Path::new(P2_POPULATION_MANIFEST_PATH), 1, MAX_POPULATION_MANIFEST_BYTES)?;
    validate_directory_inventory(
        Path::new("/home/ubuntu/.local/state/base-mev/p2-population-v1"),
        &["manifest.bin", "manifest.bin.open"],
    )?;
    let manifest = FrozenP2PopulationManifestV1::decode_checked(&manifest_bytes)?;

    validate_directory_inventory(
        Path::new("/home/ubuntu/.local/state/base-mev/settled-loss-v1"),
        &["projection.bin", "accepted-head", "projection.bin.open", "accepted-head.open"],
    )?;
    let (projection_bytes, projection_file) =
        read_strict_file(Path::new(SETTLED_LOSS_PROJECTION_PATH), 1, MAX_PROJECTION_BYTES)?;
    validate_directory_inventory(
        Path::new("/home/ubuntu/.local/state/base-mev/settled-loss-v1"),
        &["projection.bin", "accepted-head", "projection.bin.open", "accepted-head.open"],
    )?;
    let projection = TerminalSettlementProjectionV1::decode_checked(&projection_bytes)?;
    Ok((manifest, manifest_file, projection, projection_file))
}

fn validate_manifest_equality(
    manifest: &FrozenP2PopulationManifestV1,
    projection: &TerminalSettlementProjectionV1,
) -> Result<(), SettledLossUnavailableReason> {
    if manifest.campaign_id != projection.campaign_id
        || manifest.chain_id != projection.chain_id
        || manifest.source_window_start_ms != projection.source_window_start_ms
        || manifest.source_window_end_ms != projection.source_window_end_ms
        || manifest.source_snapshot_xmin != projection.source_snapshot_xmin
        || manifest.source_snapshot_xmax != projection.source_snapshot_xmax
        || manifest.source_snapshot_xip_hash != projection.source_snapshot_xip_hash
        || manifest.source_snapshot_wal_lsn != projection.source_snapshot_wal_lsn
        || manifest.submission_count != projection.submission_count
        || manifest.source_manifest_hash != projection.source_manifest_hash
        || manifest.signature != projection.population_closure_signature
        || manifest.entries != projection.source_manifest_entries
    {
        return Err(SettledLossUnavailableReason::ManifestMismatch);
    }
    Ok(())
}

fn validate_finality_snapshot<A: FinalizedChainAuthority>(
    chain: &A,
    projection: &TerminalSettlementProjectionV1,
    validate_terminals: bool,
) -> Result<BlockNumHash, SettledLossUnavailableReason> {
    let head = chain
        .finalized_head()
        .map_err(|_| SettledLossUnavailableReason::FinalityUnavailable)?
        .ok_or(SettledLossUnavailableReason::FinalityUnavailable)?;
    if projection.finalized_block_number > head.number {
        return Err(SettledLossUnavailableReason::FinalityUnavailable);
    }
    let lag = head
        .number
        .checked_sub(projection.finalized_block_number)
        .ok_or(SettledLossUnavailableReason::FinalityUnavailable)?;
    if lag > MAX_FINALIZED_HEAD_LAG {
        return Err(SettledLossUnavailableReason::Stale);
    }
    let projection_hash = chain
        .canonical_hash(projection.finalized_block_number)
        .map_err(|_| SettledLossUnavailableReason::FinalityUnavailable)?
        .ok_or(SettledLossUnavailableReason::FinalityUnavailable)?;
    if projection_hash != projection.finalized_block_hash {
        return Err(SettledLossUnavailableReason::CanonicalMismatch(
            CanonicalMismatchClass::ProjectionFinalizedHash,
        ));
    }
    if validate_terminals {
        let mut terminal_blocks = BTreeMap::new();
        for terminal in &projection.terminal_entries {
            if terminal.terminal == TerminalKindV1::Unresolved {
                continue;
            }
            if let Some(previous) =
                terminal_blocks.insert(terminal.terminal_block_number, terminal.terminal_block_hash)
                && previous != terminal.terminal_block_hash
            {
                return Err(SettledLossUnavailableReason::CanonicalMismatch(
                    CanonicalMismatchClass::TerminalHeightConflict,
                ));
            }
        }
        for (number, expected) in terminal_blocks {
            let actual = chain
                .canonical_hash(number)
                .map_err(|_| SettledLossUnavailableReason::FinalityUnavailable)?
                .ok_or(SettledLossUnavailableReason::FinalityUnavailable)?;
            if actual != expected {
                return Err(SettledLossUnavailableReason::CanonicalMismatch(
                    CanonicalMismatchClass::TerminalHistoricalHash,
                ));
            }
        }
    }
    Ok(head)
}

fn validate_terminal_entry(
    terminal: &TerminalSettlementEntryV1,
) -> Result<(), SettledLossUnavailableReason> {
    if terminal.realized_profit_wei != U256::ZERO {
        return Err(SettledLossUnavailableReason::Malformed);
    }
    let components = [
        terminal.execution_gas_loss_wei,
        terminal.l1_data_fee_loss_wei,
        terminal.operator_fee_loss_wei,
        terminal.kickback_loss_wei,
        terminal.ejection_loss_wei,
    ];
    if checked_component_sum(&components)? != terminal.settled_loss_wei {
        return Err(SettledLossUnavailableReason::Malformed);
    }
    match (terminal.terminal, terminal.unresolved_reason) {
        (TerminalKindV1::Unresolved, UnresolvedReasonV1::None) => {
            Err(SettledLossUnavailableReason::Malformed)
        }
        (TerminalKindV1::Unresolved, _) => {
            if terminal.terminal_block_number != 0
                || terminal.terminal_block_hash != B256::ZERO
                || components.iter().any(|component| *component != U256::ZERO)
                || terminal.settled_loss_wei != U256::ZERO
            {
                return Err(SettledLossUnavailableReason::Malformed);
            }
            Ok(())
        }
        (_, UnresolvedReasonV1::None) => {
            if terminal.terminal_block_number == 0 || terminal.terminal_block_hash == B256::ZERO {
                return Err(SettledLossUnavailableReason::Malformed);
            }
            match terminal.terminal {
                TerminalKindV1::Successful => {
                    if terminal.kickback_loss_wei == U256::ZERO
                        || terminal.ejection_loss_wei != U256::ZERO
                    {
                        return Err(SettledLossUnavailableReason::Malformed);
                    }
                }
                TerminalKindV1::Reverted => {
                    if terminal.kickback_loss_wei != U256::ZERO
                        || terminal.ejection_loss_wei != U256::ZERO
                    {
                        return Err(SettledLossUnavailableReason::Malformed);
                    }
                }
                TerminalKindV1::Ejected => {
                    if terminal.execution_gas_loss_wei != U256::ZERO
                        || terminal.l1_data_fee_loss_wei != U256::ZERO
                        || terminal.operator_fee_loss_wei != U256::ZERO
                        || terminal.kickback_loss_wei != U256::ZERO
                    {
                        return Err(SettledLossUnavailableReason::Malformed);
                    }
                }
                TerminalKindV1::Unresolved => {
                    return Err(SettledLossUnavailableReason::Malformed);
                }
            }
            Ok(())
        }
        (_, _) => Err(SettledLossUnavailableReason::Malformed),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AcceptedProjectionHeadV1 {
    campaign_id: B256,
    source_window_start_ms: u64,
    source_window_end_ms: u64,
    projection_sequence: u64,
    manifest_start_sequence: u64,
    manifest_next_sequence: u64,
    source_manifest_hash: B256,
    content_hash: B256,
}

impl AcceptedProjectionHeadV1 {
    fn from_projection(projection: &TerminalSettlementProjectionV1) -> Self {
        Self {
            campaign_id: projection.campaign_id,
            source_window_start_ms: projection.source_window_start_ms,
            source_window_end_ms: projection.source_window_end_ms,
            projection_sequence: projection.projection_sequence,
            manifest_start_sequence: projection.manifest_start_sequence,
            manifest_next_sequence: projection.manifest_next_sequence,
            source_manifest_hash: projection.source_manifest_hash,
            content_hash: projection.content_hash,
        }
    }

    fn encode(self) -> [u8; ACCEPTED_HEAD_BYTES] {
        let mut output = [0u8; ACCEPTED_HEAD_BYTES];
        let mut offset = 0usize;
        for bytes in [
            self.campaign_id.as_slice(),
            &self.source_window_start_ms.to_be_bytes(),
            &self.source_window_end_ms.to_be_bytes(),
            &self.projection_sequence.to_be_bytes(),
            &self.manifest_start_sequence.to_be_bytes(),
            &self.manifest_next_sequence.to_be_bytes(),
            self.source_manifest_hash.as_slice(),
            self.content_hash.as_slice(),
        ] {
            output[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
        }
        output
    }

    fn decode(bytes: &[u8]) -> Result<Self, SettledLossUnavailableReason> {
        if bytes.len() != ACCEPTED_HEAD_BYTES {
            return Err(SettledLossUnavailableReason::Io);
        }
        let mut reader = CanonicalReader::new(bytes);
        let result = Self {
            campaign_id: reader.b256()?,
            source_window_start_ms: reader.u64()?,
            source_window_end_ms: reader.u64()?,
            projection_sequence: reader.u64()?,
            manifest_start_sequence: reader.u64()?,
            manifest_next_sequence: reader.u64()?,
            source_manifest_hash: reader.b256()?,
            content_hash: reader.b256()?,
        };
        reader.finish()?;
        Ok(result)
    }
}

fn accept_projection_head(
    projection: &TerminalSettlementProjectionV1,
) -> Result<(), SettledLossUnavailableReason> {
    let proposed = AcceptedProjectionHeadV1::from_projection(projection);
    match read_strict_file(
        Path::new(SETTLED_LOSS_ANCHOR_PATH),
        ACCEPTED_HEAD_BYTES,
        ACCEPTED_HEAD_BYTES,
    ) {
        Ok((bytes, _)) => {
            let accepted = AcceptedProjectionHeadV1::decode(&bytes)?;
            if accepted == proposed {
                return Ok(());
            }
            let expected_sequence = accepted
                .projection_sequence
                .checked_add(1)
                .ok_or(SettledLossUnavailableReason::Rollback)?;
            if proposed.projection_sequence != expected_sequence
                || proposed.campaign_id != accepted.campaign_id
                || proposed.source_window_start_ms != accepted.source_window_start_ms
                || proposed.source_window_end_ms != accepted.source_window_end_ms
                || proposed.manifest_start_sequence != accepted.manifest_start_sequence
                || proposed.manifest_next_sequence != accepted.manifest_next_sequence
                || proposed.source_manifest_hash != accepted.source_manifest_hash
                || projection.previous_content_hash != accepted.content_hash
            {
                return Err(SettledLossUnavailableReason::Rollback);
            }
            publish_anchor(proposed)
        }
        Err(SettledLossUnavailableReason::Missing) => {
            if projection.projection_sequence != 1 || projection.previous_content_hash != B256::ZERO
            {
                return Err(SettledLossUnavailableReason::Rollback);
            }
            publish_anchor(proposed)
        }
        Err(reason) => Err(reason),
    }
}

fn publish_anchor(head: AcceptedProjectionHeadV1) -> Result<(), SettledLossUnavailableReason> {
    let final_path = Path::new(SETTLED_LOSS_ANCHOR_PATH);
    let parent = final_path.parent().ok_or(SettledLossUnavailableReason::Io)?;
    let directory = open_directory_no_follow(parent)?;
    let directory_path = proc_fd_path(&directory);
    let temp = directory_path.join("accepted-head.open");
    let final_name = final_path.file_name().ok_or(SettledLossUnavailableReason::Io)?;
    let final_handle_path = directory_path.join(final_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)
        .map_err(|_| SettledLossUnavailableReason::Io)?;
    let result = (|| {
        file.write_all(&head.encode()).map_err(|_| SettledLossUnavailableReason::Io)?;
        file.sync_all().map_err(|_| SettledLossUnavailableReason::Io)?;
        validate_final_metadata(
            &file.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
            ACCEPTED_HEAD_BYTES,
            ACCEPTED_HEAD_BYTES,
        )?;
        std::fs::rename(&temp, &final_handle_path).map_err(|_| SettledLossUnavailableReason::Io)?;
        directory.sync_all().map_err(|_| SettledLossUnavailableReason::Io)?;
        Ok(())
    })();
    if result.is_err() {
        return result;
    }
    let (_, _) = read_strict_file(final_path, ACCEPTED_HEAD_BYTES, ACCEPTED_HEAD_BYTES)?;
    Ok(())
}

fn read_strict_file(
    path: &Path,
    minimum: usize,
    maximum: usize,
) -> Result<(Vec<u8>, OpenedArtifact), SettledLossUnavailableReason> {
    let mut file = open_final_no_follow(path)?;
    let before = file.metadata().map_err(|_| SettledLossUnavailableReason::Io)?;
    validate_final_metadata(&before, minimum, maximum)?;
    let identity = FileIdentity::from_metadata(&before);
    let length = usize::try_from(before.len()).map_err(|_| SettledLossUnavailableReason::Io)?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes).map_err(|_| SettledLossUnavailableReason::Io)?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing).map_err(|_| SettledLossUnavailableReason::Io)? != 0 {
        return Err(SettledLossUnavailableReason::Io);
    }
    let after = file.metadata().map_err(|_| SettledLossUnavailableReason::Io)?;
    if FileIdentity::from_metadata(&after) != identity {
        return Err(SettledLossUnavailableReason::Io);
    }
    Ok((bytes, OpenedArtifact { file, identity }))
}

pub(crate) fn read_strict_bytes(
    path: &Path,
    minimum: usize,
    maximum: usize,
) -> Result<Vec<u8>, SettledLossUnavailableReason> {
    read_strict_file(path, minimum, maximum).map(|(bytes, _)| bytes)
}

pub(crate) fn read_install_bundle_bytes() -> Result<Vec<u8>, SettledLossUnavailableReason> {
    let directory = Path::new("/home/ubuntu/.local/state/base-mev/t4e-install-v1");
    validate_directory_inventory(directory, &["authority.bundle", "authority.bundle.open"])?;
    let bytes = read_strict_bytes(Path::new(T4E_INSTALL_BUNDLE_PATH), 584, 16_384)?;
    validate_directory_inventory(directory, &["authority.bundle", "authority.bundle.open"])?;
    Ok(bytes)
}

fn open_final_no_follow(path: &Path) -> Result<File, SettledLossUnavailableReason> {
    let parent = path.parent().ok_or(SettledLossUnavailableReason::Io)?;
    let directory = open_directory_no_follow(parent)?;
    let file_name = path.file_name().ok_or(SettledLossUnavailableReason::Io)?;
    let handle_path = proc_fd_path(&directory).join(file_name);
    match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(handle_path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(SettledLossUnavailableReason::Missing)
        }
        Err(_) => Err(SettledLossUnavailableReason::Io),
    }
}

pub(super) fn open_directory_no_follow(path: &Path) -> Result<File, SettledLossUnavailableReason> {
    if !path.is_absolute() {
        return Err(SettledLossUnavailableReason::Io);
    }
    let effective_uid = effective_uid()?;
    let mut current_path = PathBuf::from("/");
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|_| SettledLossUnavailableReason::Io)?;
    validate_directory_metadata(
        &current_path,
        &current.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
        effective_uid,
    )?;

    for component in path.components().skip(1) {
        let std::path::Component::Normal(name) = component else {
            return Err(SettledLossUnavailableReason::Io);
        };
        current_path.push(name);
        let next_path = proc_fd_path(&current).join(name);
        let next = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(next_path)
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SettledLossUnavailableReason::Missing);
            }
            Err(_) => return Err(SettledLossUnavailableReason::Io),
        };
        validate_directory_metadata(
            &current_path,
            &next.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
            effective_uid,
        )?;
        current = next;
    }
    Ok(current)
}

pub(super) fn proc_fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn validate_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    effective_uid: u32,
) -> Result<(), SettledLossUnavailableReason> {
    if !metadata.is_dir() {
        return Err(SettledLossUnavailableReason::Io);
    }
    let mode = metadata.mode() & 0o777;
    let private_root = Path::new("/home/ubuntu/.local/state/base-mev");
    if path == Path::new("/") || path == Path::new("/home") {
        if metadata.uid() != 0 || mode & 0o022 != 0 {
            return Err(SettledLossUnavailableReason::Io);
        }
    } else if path.starts_with(private_root) {
        if metadata.uid() != effective_uid || mode != 0o700 {
            return Err(SettledLossUnavailableReason::Io);
        }
    } else if path == Path::new("/home/ubuntu")
        || path == Path::new("/home/ubuntu/.local")
        || path == Path::new("/home/ubuntu/.local/state")
    {
        if metadata.uid() != effective_uid || mode & 0o022 != 0 {
            return Err(SettledLossUnavailableReason::Io);
        }
    } else {
        return Err(SettledLossUnavailableReason::Io);
    }
    Ok(())
}

pub(super) fn validate_directory_inventory(
    directory: &Path,
    permitted: &[&str],
) -> Result<(), SettledLossUnavailableReason> {
    let opened = open_directory_no_follow(directory)?;
    let before = FileIdentity::from_metadata(
        &opened.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
    );
    for entry in
        std::fs::read_dir(proc_fd_path(&opened)).map_err(|_| SettledLossUnavailableReason::Io)?
    {
        let entry = entry.map_err(|_| SettledLossUnavailableReason::Io)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(SettledLossUnavailableReason::Io)?;
        if !permitted.contains(&name) || name.ends_with(".open") {
            return Err(SettledLossUnavailableReason::Io);
        }
    }
    let after = FileIdentity::from_metadata(
        &opened.metadata().map_err(|_| SettledLossUnavailableReason::Io)?,
    );
    if before != after {
        return Err(SettledLossUnavailableReason::Io);
    }
    Ok(())
}

fn validate_final_metadata(
    metadata: &std::fs::Metadata,
    minimum: usize,
    maximum: usize,
) -> Result<(), SettledLossUnavailableReason> {
    let effective_uid = effective_uid()?;
    let length = usize::try_from(metadata.len()).map_err(|_| SettledLossUnavailableReason::Io)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || length < minimum
        || length > maximum
    {
        return Err(SettledLossUnavailableReason::Io);
    }
    Ok(())
}

fn effective_uid() -> Result<u32, SettledLossUnavailableReason> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|_| SettledLossUnavailableReason::Io)?;
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or(SettledLossUnavailableReason::Io)?;
    line.split_ascii_whitespace()
        .nth(2)
        .ok_or(SettledLossUnavailableReason::Io)?
        .parse()
        .map_err(|_| SettledLossUnavailableReason::Io)
}

pub(crate) fn verify_canonical_signature(
    bytes: &[u8; 65],
    preimage: &[u8],
) -> Result<(), SettledLossUnavailableReason> {
    verify_signature_shape(bytes)?;
    #[cfg(not(test))]
    let owner = OWNER_ATTEST_ADDRESS.ok_or(SettledLossUnavailableReason::AuthenticationFailed)?;
    #[cfg(test)]
    let owner = super::testkit::owner_address();
    let signature = Signature::from_raw_array(bytes)
        .map_err(|_| SettledLossUnavailableReason::AuthenticationFailed)?;
    let recovered = signature
        .recover_address_from_msg(preimage)
        .map_err(|_| SettledLossUnavailableReason::AuthenticationFailed)?;
    if recovered != owner {
        return Err(SettledLossUnavailableReason::AuthenticationFailed);
    }
    Ok(())
}

pub(crate) fn verify_signature_shape(bytes: &[u8; 65]) -> Result<(), SettledLossUnavailableReason> {
    let r: &[u8; 32] =
        bytes[..32].try_into().map_err(|_| SettledLossUnavailableReason::AuthenticationFailed)?;
    let s: &[u8; 32] =
        bytes[32..64].try_into().map_err(|_| SettledLossUnavailableReason::AuthenticationFailed)?;
    if bytes[64] > 1
        || r.iter().all(|byte| *byte == 0)
        || s.iter().all(|byte| *byte == 0)
        || *r >= SECP256K1_ORDER
        || *s >= SECP256K1_ORDER
        || *s > SECP256K1_HALF_ORDER
    {
        return Err(SettledLossUnavailableReason::AuthenticationFailed);
    }
    Ok(())
}

fn validate_count_before_allocation(
    count: usize,
    available: usize,
    entry_bytes: usize,
    trailing: usize,
) -> Result<(), SettledLossUnavailableReason> {
    if count == 0 || count > MAX_TERMINAL_ENTRIES {
        return Err(SettledLossUnavailableReason::Malformed);
    }
    let required = count
        .checked_mul(entry_bytes)
        .and_then(|value| value.checked_add(trailing))
        .ok_or(SettledLossUnavailableReason::Malformed)?;
    if required > available {
        return Err(SettledLossUnavailableReason::Malformed);
    }
    Ok(())
}

fn checked_add(left: U256, right: U256) -> Result<U256, SettledLossUnavailableReason> {
    left.checked_add(right).ok_or(SettledLossUnavailableReason::Malformed)
}

fn checked_component_sum(components: &[U256]) -> Result<U256, SettledLossUnavailableReason> {
    components.iter().try_fold(U256::ZERO, |sum, value| checked_add(sum, *value))
}

fn encode_u256(output: &mut Vec<u8>, value: U256) {
    output.extend_from_slice(&value.to_be_bytes::<32>());
}

struct CanonicalReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> CanonicalReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SettledLossUnavailableReason> {
        let end = self.offset.checked_add(length).ok_or(SettledLossUnavailableReason::Malformed)?;
        let value =
            self.bytes.get(self.offset..end).ok_or(SettledLossUnavailableReason::Malformed)?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SettledLossUnavailableReason> {
        self.take(N)?.try_into().map_err(|_| SettledLossUnavailableReason::Malformed)
    }

    fn u8(&mut self) -> Result<u8, SettledLossUnavailableReason> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, SettledLossUnavailableReason> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, SettledLossUnavailableReason> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, SettledLossUnavailableReason> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn b256(&mut self) -> Result<B256, SettledLossUnavailableReason> {
        Ok(B256::from(self.array::<32>()?))
    }

    fn u256(&mut self) -> Result<U256, SettledLossUnavailableReason> {
        Ok(U256::from_be_bytes(self.array::<32>()?))
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn finish(self) -> Result<(), SettledLossUnavailableReason> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(SettledLossUnavailableReason::Malformed)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn exact_wire_size_boundaries_are_pinned() {
        assert_eq!(SOURCE_ENTRY_BYTES, 169);
        assert_eq!(TERMINAL_ENTRY_BYTES, 403);
        assert_eq!(
            664 + MAX_TERMINAL_ENTRIES * (SOURCE_ENTRY_BYTES + TERMINAL_ENTRY_BYTES),
            MAX_CANONICAL_PROJECTION_BYTES
        );
        assert_eq!(256 + MAX_TERMINAL_ENTRIES * SOURCE_ENTRY_BYTES, MAX_CANONICAL_POPULATION_BYTES);
        assert!(MAX_CANONICAL_PROJECTION_BYTES < MAX_PROJECTION_BYTES);
    }

    #[test]
    fn count_overflow_rejects_before_allocation() {
        assert_eq!(
            validate_count_before_allocation(
                MAX_TERMINAL_ENTRIES + 1,
                usize::MAX,
                SOURCE_ENTRY_BYTES,
                65
            ),
            Err(SettledLossUnavailableReason::Malformed)
        );
        assert_eq!(
            validate_count_before_allocation(
                MAX_TERMINAL_ENTRIES,
                MAX_TERMINAL_ENTRIES * SOURCE_ENTRY_BYTES + 64,
                SOURCE_ENTRY_BYTES,
                65
            ),
            Err(SettledLossUnavailableReason::Malformed)
        );
    }

    #[test]
    fn signature_shape_rejects_noncanonical_recovery_and_scalars() {
        let mut signature = [0u8; 65];
        signature[31] = 1;
        signature[63] = 1;
        assert!(verify_signature_shape(&signature).is_ok());
        signature[64] = 27;
        assert_eq!(
            verify_signature_shape(&signature),
            Err(SettledLossUnavailableReason::AuthenticationFailed)
        );
        signature[64] = 0;
        signature[32..64].copy_from_slice(&SECP256K1_ORDER);
        assert_eq!(
            verify_signature_shape(&signature),
            Err(SettledLossUnavailableReason::AuthenticationFailed)
        );
        signature[32..64].copy_from_slice(&SECP256K1_HALF_ORDER);
        signature[63] = signature[63].saturating_add(1);
        assert_eq!(
            verify_signature_shape(&signature),
            Err(SettledLossUnavailableReason::AuthenticationFailed)
        );
    }

    #[test]
    fn absence_and_authenticated_zero_are_distinct() {
        assert_ne!(
            SettledLossLoad::Missing,
            SettledLossLoad::Complete { total_settled_loss_wei: U256::ZERO }
        );
    }

    #[test]
    fn trusted_root_components_are_opened_no_follow_and_policy_checked() {
        let root = open_directory_no_follow(Path::new("/")).expect("trusted root");
        let root_metadata = root.metadata().expect("root metadata");
        assert_eq!(root_metadata.uid(), 0);
        assert_eq!(root_metadata.mode() & 0o022, 0);

        let home = open_directory_no_follow(Path::new("/home")).expect("trusted home");
        let home_metadata = home.metadata().expect("home metadata");
        assert_eq!(home_metadata.uid(), 0);
        assert_eq!(home_metadata.mode() & 0o022, 0);
    }

    #[derive(Debug)]
    struct TestChain {
        head: Result<Option<BlockNumHash>, FinalizedChainError>,
        hashes: BTreeMap<u64, Result<Option<B256>, FinalizedChainError>>,
        calls: Mutex<Vec<u64>>,
    }

    impl FinalizedChainAuthority for TestChain {
        fn finalized_head(&self) -> Result<Option<BlockNumHash>, FinalizedChainError> {
            self.head
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, FinalizedChainError> {
            self.calls.lock().expect("calls").push(number);
            self.hashes.get(&number).copied().unwrap_or(Ok(None))
        }
    }

    fn terminal_at(number: u64, hash: B256) -> TerminalSettlementEntryV1 {
        TerminalSettlementEntryV1 {
            submission_sequence: 0,
            source_submission_id: B256::repeat_byte(1),
            correlation_key: B256::repeat_byte(2),
            candidate_signed_tx_hash: B256::repeat_byte(3),
            our_backrun_tx_hash: B256::repeat_byte(4),
            terminal: TerminalKindV1::Reverted,
            unresolved_reason: UnresolvedReasonV1::None,
            terminal_block_number: number,
            terminal_block_hash: hash,
            execution_gas_loss_wei: U256::ZERO,
            l1_data_fee_loss_wei: U256::ZERO,
            operator_fee_loss_wei: U256::ZERO,
            kickback_loss_wei: U256::ZERO,
            ejection_loss_wei: U256::ZERO,
            settled_loss_wei: U256::ZERO,
            realized_profit_wei: U256::ZERO,
        }
    }

    fn projection_at(
        finalized_block_number: u64,
        finalized_block_hash: B256,
        terminal_entries: Vec<TerminalSettlementEntryV1>,
    ) -> TerminalSettlementProjectionV1 {
        TerminalSettlementProjectionV1 {
            campaign_id: B256::repeat_byte(9),
            chain_id: SETTLED_LOSS_CHAIN_ID,
            source_window_start_ms: 1,
            source_window_end_ms: 2,
            source_snapshot_xmin: 1,
            source_snapshot_xmax: 2,
            source_snapshot_xip_hash: B256::repeat_byte(5),
            source_snapshot_wal_lsn: 1,
            projection_sequence: 1,
            manifest_start_sequence: 0,
            manifest_next_sequence: terminal_entries.len() as u64,
            submission_count: terminal_entries.len() as u64,
            terminal_count: terminal_entries.len() as u64,
            complete: true,
            unresolved_count: 0,
            source_manifest_hash: B256::repeat_byte(6),
            population_closure_signature: [0u8; 65],
            finalized_block_number,
            finalized_block_hash,
            previous_content_hash: B256::ZERO,
            total_execution_gas_loss_wei: U256::ZERO,
            total_l1_data_fee_loss_wei: U256::ZERO,
            total_operator_fee_loss_wei: U256::ZERO,
            total_kickback_loss_wei: U256::ZERO,
            total_ejection_loss_wei: U256::ZERO,
            total_settled_loss_wei: U256::ZERO,
            source_manifest_entries: Vec::new(),
            terminal_entries,
            content_hash: B256::ZERO,
            signature: [0u8; 65],
        }
    }

    #[test]
    fn closed_load_mapping_keeps_expected_absence_separate_from_errors() {
        let summary = BoundedUnresolvedSummaryV1 {
            total: 1,
            first_sequence: 7,
            first_reason: UnresolvedReasonV1::ReceiptMissing,
            reason_counts: [1, 0, 0, 0, 0, 0, 0],
        };
        assert_eq!(
            classify_load_failure(SettledLossUnavailableReason::Missing),
            SettledLossLoad::Missing
        );
        for reason in [
            SettledLossUnavailableReason::Incomplete,
            SettledLossUnavailableReason::Unresolved(summary),
            SettledLossUnavailableReason::Stale,
            SettledLossUnavailableReason::ManifestMismatch,
        ] {
            assert_eq!(
                classify_load_failure(reason.clone()),
                SettledLossLoad::PendingOrUnresolved(reason)
            );
        }
        for reason in [
            SettledLossUnavailableReason::FinalityUnavailable,
            SettledLossUnavailableReason::CanonicalMismatch(
                CanonicalMismatchClass::ProjectionFinalizedHash,
            ),
            SettledLossUnavailableReason::Malformed,
            SettledLossUnavailableReason::AuthenticationFailed,
            SettledLossUnavailableReason::Rollback,
            SettledLossUnavailableReason::Io,
        ] {
            assert_eq!(classify_load_failure(reason.clone()), SettledLossLoad::Error(reason));
        }
    }

    #[test]
    fn finalized_lag_boundary_and_every_distinct_terminal_hash_are_checked() {
        let finalized_hash = B256::repeat_byte(0xa1);
        let terminal_hash = B256::repeat_byte(0xb2);
        let projection = projection_at(
            100,
            finalized_hash,
            vec![terminal_at(90, terminal_hash), terminal_at(90, terminal_hash)],
        );
        let chain = TestChain {
            head: Ok(Some(BlockNumHash { number: 228, hash: B256::repeat_byte(0xff) })),
            hashes: BTreeMap::from([
                (100, Ok(Some(finalized_hash))),
                (90, Ok(Some(terminal_hash))),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        assert!(validate_finality_snapshot(&chain, &projection, true).is_ok());
        assert_eq!(*chain.calls.lock().expect("calls"), vec![100, 90]);

        let stale = TestChain {
            head: Ok(Some(BlockNumHash { number: 229, hash: B256::repeat_byte(0xff) })),
            hashes: BTreeMap::from([(100, Ok(Some(finalized_hash)))]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            validate_finality_snapshot(&stale, &projection, true),
            Err(SettledLossUnavailableReason::Stale)
        );
        assert!(stale.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn finalized_and_terminal_canonical_mismatches_remain_distinct() {
        let finalized_hash = B256::repeat_byte(0xa1);
        let terminal_hash = B256::repeat_byte(0xb2);
        let projection = projection_at(100, finalized_hash, vec![terminal_at(90, terminal_hash)]);
        let projection_mismatch = TestChain {
            head: Ok(Some(BlockNumHash { number: 100, hash: finalized_hash })),
            hashes: BTreeMap::from([(100, Ok(Some(B256::repeat_byte(0xee))))]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            validate_finality_snapshot(&projection_mismatch, &projection, true),
            Err(SettledLossUnavailableReason::CanonicalMismatch(
                CanonicalMismatchClass::ProjectionFinalizedHash,
            ))
        );

        let terminal_mismatch = TestChain {
            head: Ok(Some(BlockNumHash { number: 100, hash: finalized_hash })),
            hashes: BTreeMap::from([
                (100, Ok(Some(finalized_hash))),
                (90, Ok(Some(B256::repeat_byte(0xee)))),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            validate_finality_snapshot(&terminal_mismatch, &projection, true),
            Err(SettledLossUnavailableReason::CanonicalMismatch(
                CanonicalMismatchClass::TerminalHistoricalHash,
            ))
        );

        let conflict = projection_at(
            100,
            finalized_hash,
            vec![terminal_at(90, terminal_hash), terminal_at(90, B256::repeat_byte(0xcc))],
        );
        let chain = TestChain {
            head: Ok(Some(BlockNumHash { number: 100, hash: finalized_hash })),
            hashes: BTreeMap::from([(100, Ok(Some(finalized_hash)))]),
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(
            validate_finality_snapshot(&chain, &conflict, true),
            Err(SettledLossUnavailableReason::CanonicalMismatch(
                CanonicalMismatchClass::TerminalHeightConflict,
            ))
        );
        assert_eq!(*chain.calls.lock().expect("calls"), vec![100]);
    }
}
