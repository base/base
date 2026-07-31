#![cfg(feature = "arm-provisioning")]

//! `arm-provisioning`-gated signer-free preparation and attachment for T4e artifacts.

#[cfg(test)]
use std::cell::RefCell;
use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};

use alloy_primitives::{Address, B256, U256, hex, keccak256};
use serde_json::{Map, Value};

use super::{
    BoundedSubmissionIdV1, CanonicalDeploymentPairV1, CanonicalG7PairV1, CanonicalLivePairV1,
    PopulationClosureFieldsV1, PopulationKindV1, ProducerConformance, ProducerError,
    ProjectionClosureFieldsV1, SignedPopulationManifestV1, SourceLedgerRowV1,
    SourceSubmissionManifestEntryV1, TerminalKindV1, TerminalSettlementEntryV1, UnresolvedReasonV1,
};

const EXPORT_SCHEMA: &str = "base-mev/t4e-frozen-export/v2";
const XIP_DOMAIN: &[u8] = b"base-mev/postgres-snapshot-xip/v1";
const MAX_EXPORT_BYTES: u64 = 128 * 1024 * 1024;
// The read bound deliberately admits the two 134-byte malformed text forms (CRLF and two LFs)
// so canonical validation, rather than the byte cap, rejects them.
const MAX_SIGNATURE_BYTES: u64 = 134;

#[cfg(test)]
thread_local! {
    static READ_BOUNDED_AFTER_OPEN_HOOK: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
    static READ_BOUNDED_AFTER_METADATA_HOOK: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
}

/// Closed signer-free provisioning-tool failure.
#[derive(Debug)]
pub enum ProvisioningToolError {
    /// Input bytes or fields were invalid.
    Input {
        /// Stable failure class.
        code: &'static str,
        /// Submission identity when the failure belongs to one row.
        submission_id: Option<String>,
    },
    /// Canonical producer validation failed.
    Producer(ProducerError),
    /// A bounded safe filesystem operation failed.
    Io,
}

impl std::fmt::Display for ProvisioningToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input { code, submission_id: Some(id) } => {
                write!(formatter, "{code} submission_id={id}")
            }
            Self::Input { code, submission_id: None } => formatter.write_str(code),
            Self::Producer(error) => write!(formatter, "producer error: {error:?}"),
            Self::Io => formatter.write_str("bounded provisioning I/O failed"),
        }
    }
}

impl From<ProducerError> for ProvisioningToolError {
    fn from(error: ProducerError) -> Self {
        Self::Producer(error)
    }
}

/// Parsed, checked export fields used by both population and projection preparation.
#[derive(Debug)]
pub struct ParsedFrozenExportV2 {
    /// Campaign bound into every prepared artifact.
    pub campaign_id: B256,
    /// Source chain identifier.
    pub chain_id: u64,
    /// Inclusive source-window start in milliseconds.
    pub source_window_start_ms: u64,
    /// Exclusive source-window end in milliseconds.
    pub source_window_end_ms: u64,
    /// PostgreSQL snapshot lower transaction bound.
    pub source_snapshot_xmin: u64,
    /// PostgreSQL snapshot upper transaction bound.
    pub source_snapshot_xmax: u64,
    /// Domain-separated digest of in-progress transaction identifiers.
    pub source_snapshot_xip_hash: B256,
    /// WAL position captured under the frozen transaction.
    pub source_snapshot_wal_lsn: u64,
    /// Authenticated population classification requested by the export.
    pub population_kind: PopulationKindV1,
    /// Canonical source rows for population preparation.
    pub source_rows: Vec<SourceLedgerRowV1>,
    /// Source entries copied into the terminal projection.
    pub source_entries: Vec<SourceSubmissionManifestEntryV1>,
    /// Checked terminal settlement entries.
    pub terminal_entries: Vec<TerminalSettlementEntryV1>,
}

/// Offline-only orchestration over the canonical producer APIs.
#[derive(Debug, Clone, Copy)]
pub struct T4eProvisioningTool;

impl T4eProvisioningTool {
    /// Parses and validates one bounded canonical export.
    pub fn parse_export(path: &Path) -> Result<ParsedFrozenExportV2, ProvisioningToolError> {
        let bytes = read_bounded(path, MAX_EXPORT_BYTES)?;
        if !bytes.ends_with(b"\n") || bytes[..bytes.len() - 1].contains(&b'\n') {
            return Err(input("CanonicalExport", None));
        }
        let value: Value = serde_json::from_slice(&bytes[..bytes.len() - 1])
            .map_err(|_| input("CanonicalExport", None))?;
        let root = object(&value, "CanonicalExport", None)?;
        if text(root, "schema", None)? != EXPORT_SCHEMA {
            return Err(input("ExportSchema", None));
        }
        exact_keys(
            root,
            &[
                "schema",
                "campaign_id",
                "chain_id",
                "source_window_start_ms",
                "source_window_end_ms",
                "snapshot",
                "population_kind",
                "rows",
            ],
            "CanonicalExport",
            None,
        )?;
        let campaign_id = b256(text(root, "campaign_id", None)?, "CampaignId", None)?;
        let chain_id = u64_decimal(text(root, "chain_id", None)?, "ChainId", None)?;
        let source_window_start_ms =
            u64_decimal(text(root, "source_window_start_ms", None)?, "SourceWindow", None)?;
        let source_window_end_ms =
            u64_decimal(text(root, "source_window_end_ms", None)?, "SourceWindow", None)?;
        if chain_id != 8453 || source_window_start_ms >= source_window_end_ms {
            return Err(input("IdentityInvalid", None));
        }
        let population_kind = match text(root, "population_kind", None)? {
            "populated" => PopulationKindV1::Populated,
            "genesis" => PopulationKindV1::Genesis,
            _ => return Err(input("PopulationKind", None)),
        };
        let snapshot = object(field(root, "snapshot", None)?, "SnapshotInvalid", None)?;
        exact_keys(
            snapshot,
            &["xmin", "xmax", "xip", "xip_hash", "wal_lsn", "canonical_text"],
            "SnapshotInvalid",
            None,
        )?;
        let source_snapshot_xmin =
            u64_decimal(text(snapshot, "xmin", None)?, "SnapshotInvalid", None)?;
        let source_snapshot_xmax =
            u64_decimal(text(snapshot, "xmax", None)?, "SnapshotInvalid", None)?;
        let source_snapshot_wal_lsn =
            u64_decimal(text(snapshot, "wal_lsn", None)?, "SnapshotInvalid", None)?;
        let xip_values = array(field(snapshot, "xip", None)?, "SnapshotInvalid", None)?;
        let mut xip_preimage = Vec::with_capacity(XIP_DOMAIN.len() + 4 + xip_values.len() * 8);
        xip_preimage.extend_from_slice(XIP_DOMAIN);
        xip_preimage.extend_from_slice(
            &u32::try_from(xip_values.len())
                .map_err(|_| input("SnapshotInvalid", None))?
                .to_be_bytes(),
        );
        let mut previous = None;
        for value in xip_values {
            let xid = u64_decimal(
                value.as_str().ok_or_else(|| input("SnapshotInvalid", None))?,
                "SnapshotInvalid",
                None,
            )?;
            if previous.is_some_and(|prior| prior >= xid) {
                return Err(input("SnapshotInvalid", None));
            }
            previous = Some(xid);
            xip_preimage.extend_from_slice(&xid.to_be_bytes());
        }
        let source_snapshot_xip_hash =
            b256(text(snapshot, "xip_hash", None)?, "SnapshotInvalid", None)?;
        if keccak256(xip_preimage) != source_snapshot_xip_hash
            || source_snapshot_xmin > source_snapshot_xmax
        {
            return Err(input("SnapshotInvalid", None));
        }

        let rows = array(field(root, "rows", None)?, "EmptyPopulation", None)?;
        match population_kind {
            PopulationKindV1::Populated if rows.is_empty() => {
                return Err(input("EmptyPopulation", None));
            }
            PopulationKindV1::Genesis if !rows.is_empty() => {
                return Err(input("NonEmptyGenesis", None));
            }
            PopulationKindV1::Populated | PopulationKindV1::Genesis => {}
        }
        let mut source_rows = Vec::with_capacity(rows.len());
        let mut source_entries = Vec::with_capacity(rows.len());
        let mut terminal_entries = Vec::with_capacity(rows.len());
        for (index, value) in rows.iter().enumerate() {
            let row = object(value, "CanonicalRow", None)?;
            let id = text(row, "submission_id", None)?.to_owned();
            let id_error = Some(id.clone());
            exact_keys(
                row,
                &[
                    "submission_id",
                    "chain_id",
                    "target_tx_hash",
                    "our_backrun_tx_hash",
                    "submit_wallclock_ms",
                    "submitted_block_number",
                    "inclusion_status",
                    "gross_weth_delta_wei",
                    "gas_wei",
                    "l1_data_fee_wei",
                    "operator_fee_wei",
                    "kickback_wei",
                    "ejection_loss_wei",
                    "terminal_block_number",
                    "terminal_block_hash",
                    "terminal_reconciled",
                    "paths_reconciled",
                    "realized_net_wei",
                ],
                "CanonicalRow",
                Some(id.clone()),
            )?;
            let bounded = BoundedSubmissionIdV1::new(id.as_bytes().to_vec())
                .map_err(|_| input("IdentityInvalid", id_error.clone()))?;
            let row_chain = u64_decimal(
                text(row, "chain_id", id_error.clone())?,
                "IdentityInvalid",
                id_error.clone(),
            )?;
            let target = b256(
                text(row, "target_tx_hash", id_error.clone())?,
                "IdentityInvalid",
                id_error.clone(),
            )?;
            let backrun = b256(
                text(row, "our_backrun_tx_hash", id_error.clone())?,
                "IdentityInvalid",
                id_error.clone(),
            )?;
            let submitted_at = u64_decimal(
                text(row, "submit_wallclock_ms", id_error.clone())?,
                "IdentityInvalid",
                id_error.clone(),
            )?;
            let _submitted_block = u64_decimal(
                text(row, "submitted_block_number", id_error.clone())?,
                "IdentityInvalid",
                id_error.clone(),
            )?;
            if row_chain != chain_id
                || submitted_at < source_window_start_ms
                || submitted_at >= source_window_end_ms
            {
                return Err(input("IdentityInvalid", id_error));
            }
            source_rows.push(SourceLedgerRowV1::new(
                bounded,
                row_chain,
                target,
                backrun,
                submitted_at,
            ));

            let mut id_preimage = Vec::with_capacity(32 + id.len());
            id_preimage.extend_from_slice(b"base-mev/p2-submission-id/v1");
            id_preimage.extend_from_slice(
                &u32::try_from(id.len())
                    .map_err(|_| input("IdentityInvalid", Some(id.clone())))?
                    .to_be_bytes(),
            );
            id_preimage.extend_from_slice(id.as_bytes());
            let source_id = keccak256(id_preimage);
            let mut correlation_preimage = Vec::with_capacity(128);
            correlation_preimage.extend_from_slice(b"base-mev/p2-correlation/v1");
            correlation_preimage.extend_from_slice(source_id.as_slice());
            correlation_preimage.extend_from_slice(target.as_slice());
            correlation_preimage.extend_from_slice(backrun.as_slice());
            let correlation = keccak256(correlation_preimage);
            let sequence = u64::try_from(index).map_err(|_| input("Bounds", Some(id.clone())))?;
            source_entries.push(SourceSubmissionManifestEntryV1::new(
                sequence,
                source_id,
                target,
                correlation,
                backrun,
                backrun,
            ));

            let status = text(row, "inclusion_status", Some(id.clone()))?;
            match status {
                "included-success" | "included-reverted" | "ejection" => {}
                "not-included" | "unresolved" => {
                    return Err(input("NonTerminalSettlement", Some(id)));
                }
                _ => return Err(input("UnknownInclusionStatus", Some(id))),
            }
            if !boolean(row, "terminal_reconciled", &id)? {
                return Err(input("UnreconciledSettlement", Some(id)));
            }
            if status == "included-success" && !boolean(row, "paths_reconciled", &id)? {
                return Err(input("UnreconciledSettlement", Some(id)));
            }
            match status {
                "included-success" if !zero_or_null(row, "ejection_loss_wei", &id)? => {
                    return Err(input("LossFormulaMismatch", Some(id)));
                }
                "included-reverted"
                    if !zero_or_null(row, "kickback_wei", &id)?
                        || !zero_or_null(row, "ejection_loss_wei", &id)? =>
                {
                    return Err(input("LossFormulaMismatch", Some(id)));
                }
                "ejection"
                    if !zero_or_null(row, "gas_wei", &id)?
                        || !zero_or_null(row, "l1_data_fee_wei", &id)?
                        || !zero_or_null(row, "operator_fee_wei", &id)?
                        || !zero_or_null(row, "kickback_wei", &id)? =>
                {
                    return Err(input("LossFormulaMismatch", Some(id)));
                }
                _ => {}
            }
            let block_number = u64_decimal(
                text(row, "terminal_block_number", Some(id.clone()))?,
                "TerminalBlockUnavailable",
                Some(id.clone()),
            )?;
            let block_hash = b256(
                text(row, "terminal_block_hash", Some(id.clone()))?,
                "TerminalHashInvalid",
                Some(id.clone()),
            )?;
            let (terminal, execution, l1, operator, kickback, ejection) = match status {
                "included-success" => (
                    TerminalKindV1::Successful,
                    u256_required(row, "gas_wei", &id)?,
                    u256_required(row, "l1_data_fee_wei", &id)?,
                    u256_required(row, "operator_fee_wei", &id)?,
                    u256_required(row, "kickback_wei", &id)?,
                    U256::ZERO,
                ),
                "included-reverted" => (
                    TerminalKindV1::Reverted,
                    u256_required(row, "gas_wei", &id)?,
                    u256_required(row, "l1_data_fee_wei", &id)?,
                    u256_required(row, "operator_fee_wei", &id)?,
                    U256::ZERO,
                    U256::ZERO,
                ),
                "ejection" => (
                    TerminalKindV1::Ejected,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    u256_required(row, "ejection_loss_wei", &id)?,
                ),
                "not-included" | "unresolved" => {
                    return Err(input("NonTerminalSettlement", Some(id)));
                }
                _ => return Err(input("UnknownInclusionStatus", Some(id))),
            };
            let settled = execution
                .checked_add(l1)
                .and_then(|sum| sum.checked_add(operator))
                .and_then(|sum| sum.checked_add(kickback))
                .and_then(|sum| sum.checked_add(ejection))
                .ok_or_else(|| input("Arithmetic", Some(id.clone())))?;
            if status == "included-success" {
                let gross = u256_required(row, "gross_weth_delta_wei", &id)?;
                let realized = text(row, "realized_net_wei", Some(id.clone()))?;
                if kickback == U256::ZERO || !signed_difference_matches(gross, settled, realized) {
                    return Err(input("LossFormulaMismatch", Some(id)));
                }
            }
            if status == "ejection" && ejection == U256::ZERO {
                return Err(input("LossFormulaMismatch", Some(id)));
            }
            terminal_entries.push(TerminalSettlementEntryV1::new(
                sequence,
                source_id,
                correlation,
                backrun,
                backrun,
                terminal,
                UnresolvedReasonV1::None,
                block_number,
                block_hash,
                execution,
                l1,
                operator,
                kickback,
                ejection,
                settled,
                U256::ZERO,
            ));
        }
        Ok(ParsedFrozenExportV2 {
            campaign_id,
            chain_id,
            source_window_start_ms,
            source_window_end_ms,
            source_snapshot_xmin,
            source_snapshot_xmax,
            source_snapshot_xip_hash,
            source_snapshot_wal_lsn,
            population_kind,
            source_rows,
            source_entries,
            terminal_entries,
        })
    }

    /// Prepares population request files from one checked export.
    pub fn prepare_population(
        export: &Path,
        request_dir: &Path,
    ) -> Result<(), ProvisioningToolError> {
        let parsed = Self::parse_export(export)?;
        let closure = PopulationClosureFieldsV1::new(
            parsed.campaign_id,
            parsed.chain_id,
            parsed.source_window_start_ms,
            parsed.source_window_end_ms,
            parsed.source_snapshot_xmin,
            parsed.source_snapshot_xmax,
            parsed.source_snapshot_xip_hash,
            parsed.source_snapshot_wal_lsn,
            parsed.population_kind,
        );
        let unsigned = ProducerConformance::prepare_frozen_manifest(parsed.source_rows, closure)?;
        write_request(request_dir, unsigned.canonical_preimage(), unsigned.canonical_preimage())
    }

    /// Prepares projection request files and proves exact membership against the signed population.
    pub fn prepare_projection(
        export: &Path,
        signed_population: &Path,
        fields_path: &Path,
        request_dir: &Path,
    ) -> Result<(), ProvisioningToolError> {
        let parsed = Self::parse_export(export)?;
        let closure = PopulationClosureFieldsV1::new(
            parsed.campaign_id,
            parsed.chain_id,
            parsed.source_window_start_ms,
            parsed.source_window_end_ms,
            parsed.source_snapshot_xmin,
            parsed.source_snapshot_xmax,
            parsed.source_snapshot_xip_hash,
            parsed.source_snapshot_wal_lsn,
            parsed.population_kind,
        );
        let expected = ProducerConformance::prepare_frozen_manifest(parsed.source_rows, closure)?;
        let signed_bytes = read_bounded(signed_population, MAX_EXPORT_BYTES)?;
        let signed = SignedPopulationManifestV1::from_canonical(signed_bytes.clone())?;
        if signed.canonical_bytes().len() != expected.canonical_preimage().len() + 65
            || &signed.canonical_bytes()[..expected.canonical_preimage().len()]
                != expected.canonical_preimage()
        {
            return Err(input("PopulationMembershipMismatch", None));
        }
        let source_hash_offset = expected
            .canonical_preimage()
            .len()
            .checked_sub(32)
            .ok_or_else(|| input("PopulationMembershipMismatch", None))?;
        let source_hash = B256::from_slice(&expected.canonical_preimage()[source_hash_offset..]);
        let population_signature: [u8; 65] = signed.canonical_bytes()
            [signed.canonical_bytes().len() - 65..]
            .try_into()
            .map_err(|_| input("PopulationSignature", None))?;
        let fields_bytes = read_bounded(fields_path, 16 * 1024)?;
        let fields_value: Value =
            serde_json::from_slice(&fields_bytes).map_err(|_| input("ProjectionFields", None))?;
        let fields = object(&fields_value, "ProjectionFields", None)?;
        exact_keys(
            fields,
            &[
                "projection_sequence",
                "finalized_block_number",
                "finalized_block_hash",
                "campaign_valid_until_block",
                "previous_content_hash",
            ],
            "ProjectionFields",
            None,
        )?;
        let projection_sequence =
            u64_decimal(text(fields, "projection_sequence", None)?, "ProjectionFields", None)?;
        let finalized_block_number =
            u64_decimal(text(fields, "finalized_block_number", None)?, "ProjectionFields", None)?;
        let finalized_block_hash =
            b256(text(fields, "finalized_block_hash", None)?, "ProjectionFields", None)?;
        let campaign_valid_until_block = u64_decimal(
            text(fields, "campaign_valid_until_block", None)?,
            "ProjectionFields",
            None,
        )?;
        let previous_content_hash =
            b256(text(fields, "previous_content_hash", None)?, "ProjectionFields", None)?;
        let projection_closure = ProjectionClosureFieldsV1::new(
            parsed.campaign_id,
            parsed.chain_id,
            parsed.source_window_start_ms,
            parsed.source_window_end_ms,
            parsed.source_snapshot_xmin,
            parsed.source_snapshot_xmax,
            parsed.source_snapshot_xip_hash,
            parsed.source_snapshot_wal_lsn,
            projection_sequence,
            source_hash,
            population_signature,
            finalized_block_number,
            finalized_block_hash,
            previous_content_hash,
            parsed.population_kind,
            campaign_valid_until_block,
        );
        let unsigned = ProducerConformance::prepare_terminal_projection(
            parsed.source_entries,
            parsed.terminal_entries,
            projection_closure,
        )?;
        write_request(request_dir, unsigned.canonical_body(), unsigned.signature_preimage())
    }

    /// Prepares an install-bundle request from owner-supplied public fields and signatures.
    pub fn prepare_install_bundle(
        input_path: &Path,
        request_dir: &Path,
    ) -> Result<(), ProvisioningToolError> {
        let bytes = read_bounded(input_path, 64 * 1024)?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| input("InstallFields", None))?;
        let fields = object(&value, "InstallFields", None)?;
        exact_keys(
            fields,
            &[
                "generation",
                "campaign_id",
                "g7_closure_epoch",
                "g7_expiry_unix",
                "g7_signature",
                "live_window_start",
                "live_expiry_unix",
                "live_signature",
                "chain_id",
                "executor",
                "code_hash",
                "binary_digest",
                "deployment_digest",
                "r9_store_identity",
                "deployment_signature",
            ],
            "InstallFields",
            None,
        )?;
        let generation = u64_decimal(text(fields, "generation", None)?, "InstallFields", None)?;
        let campaign = b256(text(fields, "campaign_id", None)?, "InstallFields", None)?;
        let g7 = CanonicalG7PairV1::new(
            campaign,
            u64_decimal(text(fields, "g7_closure_epoch", None)?, "InstallFields", None)?,
            u64_decimal(text(fields, "g7_expiry_unix", None)?, "InstallFields", None)?,
            signature(text(fields, "g7_signature", None)?)?,
        )?;
        let live = CanonicalLivePairV1::new(
            campaign,
            u64_decimal(text(fields, "live_window_start", None)?, "InstallFields", None)?,
            u64_decimal(text(fields, "live_expiry_unix", None)?, "InstallFields", None)?,
            signature(text(fields, "live_signature", None)?)?,
        )?;
        let deployment = CanonicalDeploymentPairV1::new(
            u64_decimal(text(fields, "chain_id", None)?, "InstallFields", None)?,
            address(text(fields, "executor", None)?)?,
            b256(text(fields, "code_hash", None)?, "InstallFields", None)?,
            b256(text(fields, "binary_digest", None)?, "InstallFields", None)?,
            b256(text(fields, "deployment_digest", None)?, "InstallFields", None)?,
            b256(text(fields, "r9_store_identity", None)?, "InstallFields", None)?,
            signature(text(fields, "deployment_signature", None)?)?,
        )?;
        let unsigned =
            ProducerConformance::prepare_install_bundle(generation, g7, live, deployment)?;
        write_request(request_dir, unsigned.canonical_body(), unsigned.outer_signature_preimage())
    }

    /// Attaches one supplied signature to a prepared request and writes a create-new signed file.
    pub fn attach(
        kind: &str,
        request_dir: &Path,
        signature_path: &Path,
        output: &Path,
    ) -> Result<(), ProvisioningToolError> {
        let body = read_bounded(&request_dir.join("unsigned.bin"), MAX_EXPORT_BYTES)?;
        let preimage = read_bounded(&request_dir.join("preimage.bin"), MAX_EXPORT_BYTES)?;
        let supplied = parse_signature_file(signature_path)?;
        let canonical = match kind {
            "population" => ProducerConformance::attach_population_signature_bytes(body, supplied)?
                .canonical_bytes()
                .to_vec(),
            "projection" => {
                ProducerConformance::attach_projection_signature_bytes(body, preimage, supplied)?
                    .canonical_bytes()
                    .to_vec()
            }
            "install-bundle" => ProducerConformance::attach_install_bundle_signature_bytes(
                body, preimage, supplied,
            )?
            .canonical_bytes()
            .to_vec(),
            _ => return Err(input("ArtifactKind", None)),
        };
        write_new_file(output, &canonical)
    }
}

fn input(code: &'static str, submission_id: Option<String>) -> ProvisioningToolError {
    ProvisioningToolError::Input { code, submission_id }
}

fn field<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    id: Option<String>,
) -> Result<&'a Value, ProvisioningToolError> {
    object.get(name).ok_or_else(|| input("NullRequiredColumn", id))
}

fn object<'a>(
    value: &'a Value,
    code: &'static str,
    id: Option<String>,
) -> Result<&'a Map<String, Value>, ProvisioningToolError> {
    value.as_object().ok_or_else(|| input(code, id))
}

fn exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
    code: &'static str,
    id: Option<String>,
) -> Result<(), ProvisioningToolError> {
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(input(code, id));
    }
    Ok(())
}

fn array<'a>(
    value: &'a Value,
    code: &'static str,
    id: Option<String>,
) -> Result<&'a Vec<Value>, ProvisioningToolError> {
    value.as_array().ok_or_else(|| input(code, id))
}

fn text<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    id: Option<String>,
) -> Result<&'a str, ProvisioningToolError> {
    field(object, name, id.clone())?.as_str().ok_or_else(|| input("NullRequiredColumn", id))
}

fn boolean(
    object: &Map<String, Value>,
    name: &str,
    id: &str,
) -> Result<bool, ProvisioningToolError> {
    field(object, name, Some(id.to_owned()))?
        .as_bool()
        .ok_or_else(|| input("NullRequiredColumn", Some(id.to_owned())))
}

fn zero_or_null(
    object: &Map<String, Value>,
    name: &str,
    id: &str,
) -> Result<bool, ProvisioningToolError> {
    match field(object, name, Some(id.to_owned()))? {
        Value::Null => Ok(true),
        Value::String(value) => Ok(value == "0"),
        _ => Err(input("InvalidMonetaryValue", Some(id.to_owned()))),
    }
}

fn u64_decimal(
    value: &str,
    code: &'static str,
    id: Option<String>,
) -> Result<u64, ProvisioningToolError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(input(code, id));
    }
    value.parse().map_err(|_| input(code, id))
}

fn u256_required(
    object: &Map<String, Value>,
    name: &str,
    id: &str,
) -> Result<U256, ProvisioningToolError> {
    let value = text(object, name, Some(id.to_owned()))?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(input("InvalidMonetaryValue", Some(id.to_owned())));
    }
    U256::from_str_radix(value, 10).map_err(|_| input("InvalidMonetaryValue", Some(id.to_owned())))
}

fn signed_difference_matches(gross: U256, costs: U256, value: &str) -> bool {
    if let Some(magnitude) = value.strip_prefix('-') {
        if magnitude.is_empty() || magnitude == "0" || magnitude.starts_with('0') || gross >= costs
        {
            return false;
        }
        return U256::from_str_radix(magnitude, 10).is_ok_and(|parsed| parsed == costs - gross);
    }
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || gross < costs
    {
        return false;
    }
    U256::from_str_radix(value, 10).is_ok_and(|parsed| parsed == gross - costs)
}

fn b256(
    value: &str,
    code: &'static str,
    id: Option<String>,
) -> Result<B256, ProvisioningToolError> {
    let bytes = decode_hex(value, 32).map_err(|_| input(code, id))?;
    Ok(B256::from_slice(&bytes))
}

fn address(value: &str) -> Result<Address, ProvisioningToolError> {
    let bytes = decode_hex(value, 20).map_err(|_| input("InstallFields", None))?;
    Ok(Address::from_slice(&bytes))
}

fn signature(value: &str) -> Result<[u8; 65], ProvisioningToolError> {
    decode_hex(value, 65)?.try_into().map_err(|_| input("Signature", None))
}

fn decode_hex(value: &str, expected: usize) -> Result<Vec<u8>, ProvisioningToolError> {
    if value.len() != expected * 2 + 2
        || !value.starts_with("0x")
        || value.bytes().skip(2).any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return Err(input("CanonicalHex", None));
    }
    hex::decode(&value[2..]).map_err(|_| input("CanonicalHex", None))
}

fn parse_signature_file(path: &Path) -> Result<[u8; 65], ProvisioningToolError> {
    let bytes = read_bounded(path, MAX_SIGNATURE_BYTES)?;
    if bytes.len() == 65 {
        return bytes.try_into().map_err(|_| input("Signature", None));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| input("Signature", None))?;
    signature(text.strip_suffix('\n').unwrap_or(text))
}

fn eip191_digest(preimage: &[u8]) -> B256 {
    let mut bytes = Vec::with_capacity(32 + preimage.len());
    bytes.extend_from_slice(b"\x19Ethereum Signed Message:\n");
    bytes.extend_from_slice(preimage.len().to_string().as_bytes());
    bytes.extend_from_slice(preimage);
    keccak256(bytes)
}

fn write_request(
    directory: &Path,
    body: &[u8],
    preimage: &[u8],
) -> Result<(), ProvisioningToolError> {
    DirBuilder::new().mode(0o700).create(directory).map_err(|_| ProvisioningToolError::Io)?;
    write_new_file(&directory.join("unsigned.bin"), body)?;
    write_new_file(&directory.join("preimage.bin"), preimage)?;
    let digest = format!("0x{}\n", hex::encode(eip191_digest(preimage)));
    write_new_file(&directory.join("digest.hex"), digest.as_bytes())?;
    File::open(directory).and_then(|file| file.sync_all()).map_err(|_| ProvisioningToolError::Io)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), ProvisioningToolError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| ProvisioningToolError::Io)?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|_| ProvisioningToolError::Io)
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, ProvisioningToolError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ProvisioningToolError::Io)?;
    #[cfg(test)]
    READ_BOUNDED_AFTER_OPEN_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    read_bounded_file(file, maximum)
}

fn read_bounded_file(file: File, maximum: u64) -> Result<Vec<u8>, ProvisioningToolError> {
    let metadata = file.metadata().map_err(|_| ProvisioningToolError::Io)?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(ProvisioningToolError::Io);
    }
    #[cfg(test)]
    READ_BOUNDED_AFTER_METADATA_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let read_limit = maximum.checked_add(1).ok_or(ProvisioningToolError::Io)?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| ProvisioningToolError::Io)?);
    file.take(read_limit).read_to_end(&mut bytes).map_err(|_| ProvisioningToolError::Io)?;
    if bytes.is_empty() || bytes.len() as u64 > maximum {
        return Err(ProvisioningToolError::Io);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "arm-provisioning")]
    use std::{collections::BTreeMap, path::PathBuf};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use super::*;
    #[cfg(feature = "arm-provisioning")]
    use crate::arm::{
        BlockNumHash, FinalizedChainAuthority, FinalizedChainError, NodeLocalSettledLossAuthority,
    };
    use crate::arm::{
        ProducerConformance,
        testkit::{eip191_sign, owner_key},
    };

    #[cfg(feature = "arm-provisioning")]
    #[derive(Debug)]
    struct TestChain {
        head: BlockNumHash,
        hashes: BTreeMap<u64, B256>,
    }

    #[cfg(feature = "arm-provisioning")]
    impl FinalizedChainAuthority for TestChain {
        fn finalized_head(&self) -> Result<Option<BlockNumHash>, FinalizedChainError> {
            Ok(Some(self.head))
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, FinalizedChainError> {
            Ok(self.hashes.get(&number).copied())
        }
    }

    fn hex_signature(bytes: [u8; 65]) -> String {
        format!("0x{}", hex::encode(bytes))
    }

    fn fixture_export(path: &Path) {
        let xip_hash = keccak256([XIP_DOMAIN, &0u32.to_be_bytes()].concat());
        let json = format!(
            concat!(
                "{{\"schema\":\"base-mev/t4e-frozen-export/v2\",",
                "\"campaign_id\":\"0x{campaign}\",\"chain_id\":\"8453\",",
                "\"source_window_start_ms\":\"0\",\"source_window_end_ms\":\"100\",",
                "\"population_kind\":\"populated\",",
                "\"snapshot\":{{\"xmin\":\"7\",\"xmax\":\"9\",\"xip\":[],",
                "\"xip_hash\":\"0x{xip}\",\"wal_lsn\":\"12\",\"canonical_text\":\"7:9:\"}},",
                "\"rows\":[{{\"submission_id\":\"fixture-1\",\"chain_id\":\"8453\",",
                "\"target_tx_hash\":\"0x{target}\",\"our_backrun_tx_hash\":\"0x{backrun}\",",
                "\"submit_wallclock_ms\":\"10\",\"submitted_block_number\":\"1000\",",
                "\"inclusion_status\":\"included-success\",\"gross_weth_delta_wei\":\"40\",",
                "\"gas_wei\":\"6\",\"l1_data_fee_wei\":\"1\",\"operator_fee_wei\":\"2\",",
                "\"kickback_wei\":\"30\",\"ejection_loss_wei\":null,",
                "\"terminal_block_number\":\"1001\",\"terminal_block_hash\":\"0x{terminal}\",",
                "\"terminal_reconciled\":true,\"paths_reconciled\":true,",
                "\"realized_net_wei\":\"1\"}}]}}\n"
            ),
            campaign = "99".repeat(32),
            xip = hex::encode(xip_hash),
            target = "22".repeat(32),
            backrun = "11".repeat(32),
            terminal = "33".repeat(32),
        );
        fs::write(path, json).expect("export");
    }
    fn fixture_genesis_export(path: &Path) {
        let xip_hash = keccak256([XIP_DOMAIN, &0u32.to_be_bytes()].concat());
        let json = format!(
            concat!(
                "{{\"schema\":\"base-mev/t4e-frozen-export/v2\",",
                "\"campaign_id\":\"0x{campaign}\",\"chain_id\":\"8453\",",
                "\"source_window_start_ms\":\"0\",\"source_window_end_ms\":\"100\",",
                "\"population_kind\":\"genesis\",",
                "\"snapshot\":{{\"xmin\":\"7\",\"xmax\":\"9\",\"xip\":[],",
                "\"xip_hash\":\"0x{xip}\",\"wal_lsn\":\"12\",\"canonical_text\":\"7:9:\"}},",
                "\"rows\":[]}}\n"
            ),
            campaign = "99".repeat(32),
            xip = hex::encode(xip_hash),
        );
        fs::write(path, json).expect("genesis export");
    }

    fn prepare_signed_artifacts(root: &Path, export: &Path) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let population_request = root.join("population-request");
        T4eProvisioningTool::prepare_population(export, &population_request)
            .expect("prepare population");
        assert_eq!(
            fs::metadata(population_request.join("preimage.bin"))
                .expect("preimage metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
        let population_preimage =
            fs::read(population_request.join("preimage.bin")).expect("population preimage");
        let population_signature = eip191_sign(&population_preimage, &owner_key());
        let signed_population = ProducerConformance::attach_population_signature_bytes(
            population_preimage,
            population_signature,
        )
        .expect("attach population");
        let population = signed_population.canonical_bytes().to_vec();
        let signed_population_path = root.join("population.bin");
        fs::write(&signed_population_path, &population).expect("signed population");

        let projection_fields = root.join("projection-fields.json");
        fs::write(
            &projection_fields,
            format!(
                "{{\"projection_sequence\":\"1\",\"finalized_block_number\":\"1100\",\
                 \"finalized_block_hash\":\"0x{}\",\"previous_content_hash\":\"0x{}\",\
                 \"campaign_valid_until_block\":\"1228\"}}",
                "44".repeat(32),
                "00".repeat(32),
            ),
        )
        .expect("projection fields");
        let projection_request = root.join("projection-request");
        T4eProvisioningTool::prepare_projection(
            export,
            &signed_population_path,
            &projection_fields,
            &projection_request,
        )
        .expect("prepare projection");
        let projection_body =
            fs::read(projection_request.join("unsigned.bin")).expect("projection body");
        let projection_preimage =
            fs::read(projection_request.join("preimage.bin")).expect("projection preimage");
        let projection_signature = eip191_sign(&projection_preimage, &owner_key());
        let projection = ProducerConformance::attach_projection_signature_bytes(
            projection_body,
            projection_preimage,
            projection_signature,
        )
        .expect("attach projection")
        .canonical_bytes()
        .to_vec();

        let campaign = B256::repeat_byte(0x99);
        let mut g7_preimage = Vec::new();
        g7_preimage.extend_from_slice(keccak256(b"base-mev/g7-closure/v1").as_slice());
        g7_preimage.extend_from_slice(campaign.as_slice());
        g7_preimage.extend_from_slice(&1u64.to_be_bytes());
        g7_preimage.extend_from_slice(&2_000u64.to_be_bytes());
        let mut live_preimage = Vec::new();
        live_preimage.extend_from_slice(keccak256(b"base-mev/live-run/v1").as_slice());
        live_preimage.extend_from_slice(campaign.as_slice());
        live_preimage.extend_from_slice(&1_000u64.to_be_bytes());
        live_preimage.extend_from_slice(&2_000u64.to_be_bytes());
        let executor = Address::repeat_byte(0x55);
        let code_hash = B256::repeat_byte(0x66);
        let binary = B256::repeat_byte(0x77);
        let deployment_digest = B256::repeat_byte(0x88);
        let store = B256::repeat_byte(0xaa);
        let mut deployment_preimage = Vec::new();
        deployment_preimage.extend_from_slice(keccak256(b"base-mev/deploy/v1").as_slice());
        deployment_preimage.extend_from_slice(&8453u64.to_be_bytes());
        deployment_preimage.extend_from_slice(executor.as_slice());
        deployment_preimage.extend_from_slice(code_hash.as_slice());
        deployment_preimage.extend_from_slice(binary.as_slice());
        deployment_preimage.extend_from_slice(deployment_digest.as_slice());
        deployment_preimage.extend_from_slice(store.as_slice());
        let install_fields = root.join("install-fields.json");
        fs::write(
            &install_fields,
            format!(
                concat!(
                    "{{\"generation\":\"1\",\"campaign_id\":\"0x{campaign}\",",
                    "\"g7_closure_epoch\":\"1\",\"g7_expiry_unix\":\"2000\",",
                    "\"g7_signature\":\"{g7}\",\"live_window_start\":\"1000\",",
                    "\"live_expiry_unix\":\"2000\",\"live_signature\":\"{live}\",",
                    "\"chain_id\":\"8453\",\"executor\":\"0x{executor}\",",
                    "\"code_hash\":\"0x{code}\",\"binary_digest\":\"0x{binary}\",",
                    "\"deployment_digest\":\"0x{deployment}\",",
                    "\"r9_store_identity\":\"0x{store}\",",
                    "\"deployment_signature\":\"{deployment_signature}\"}}"
                ),
                campaign = hex::encode(campaign),
                g7 = hex_signature(eip191_sign(&g7_preimage, &owner_key())),
                live = hex_signature(eip191_sign(&live_preimage, &owner_key())),
                executor = hex::encode(executor),
                code = hex::encode(code_hash),
                binary = hex::encode(binary),
                deployment = hex::encode(deployment_digest),
                store = hex::encode(store),
                deployment_signature =
                    hex_signature(eip191_sign(&deployment_preimage, &owner_key())),
            ),
        )
        .expect("install fields");
        let install_request = root.join("install-request");
        T4eProvisioningTool::prepare_install_bundle(&install_fields, &install_request)
            .expect("prepare install");
        let install_body = fs::read(install_request.join("unsigned.bin")).expect("install body");
        let install_preimage =
            fs::read(install_request.join("preimage.bin")).expect("install preimage");
        let install_signature = eip191_sign(&install_preimage, &owner_key());
        let install = ProducerConformance::attach_install_bundle_signature_bytes(
            install_body,
            install_preimage,
            install_signature,
        )
        .expect("attach install")
        .canonical_bytes()
        .to_vec();

        (population, projection, install)
    }

    #[test]
    fn signature_file_accepts_one_trailing_lf_but_rejects_noncanonical_whitespace() {
        let root = std::env::temp_dir().join(format!("t4e-signature-input-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let path = root.join("signature.hex");
        let expected = [0x11; 65];
        let canonical = hex_signature(expected);

        fs::write(&path, format!("{canonical}\n")).expect("signature with LF");
        assert_eq!(parse_signature_file(&path).expect("one trailing LF"), expected);

        for (name, invalid, code) in [
            ("two-lfs", format!("{canonical}\n\n").into_bytes(), "CanonicalHex"),
            ("crlf", format!("{canonical}\r\n").into_bytes(), "CanonicalHex"),
            (
                "embedded-lf",
                format!("{}\n{}", &canonical[..20], &canonical[20..]).into_bytes(),
                "CanonicalHex",
            ),
            ("trailing-space", format!("{canonical} ").into_bytes(), "CanonicalHex"),
            ("non-utf8-at-bound", vec![0xff; MAX_SIGNATURE_BYTES as usize], "Signature"),
        ] {
            fs::write(&path, invalid).expect("invalid signature");
            assert!(
                matches!(
                    parse_signature_file(&path),
                    Err(ProvisioningToolError::Input {
                        code: actual,
                        submission_id: None
                    }) if actual == code
                ),
                "{name} must reach canonical signature validation and fail as {code}"
            );
        }
        fs::write(&path, vec![b'x'; 135]).expect("one byte past sealed read bound");
        assert!(
            matches!(parse_signature_file(&path), Err(ProvisioningToolError::Io)),
            "a 135-byte signature file must be rejected by the effective read bound"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn bounded_read_rejects_symlinks() {
        let root = std::env::temp_dir().join(format!("t4e-bounded-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"canonical").expect("target");
        symlink(&target, &link).expect("symlink");

        assert_eq!(read_bounded(&target, 9).expect("regular file"), b"canonical");
        assert!(read_bounded(&link, 9).is_err(), "symlink must fail closed");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn bounded_read_consumes_the_opened_handle_not_a_replaced_path() {
        let root = std::env::temp_dir().join(format!("t4e-bounded-handle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let path = root.join("input");
        let moved = root.join("opened");
        fs::write(&path, b"opened").expect("input");
        let hook_path = path.clone();
        let hook_moved = moved.clone();
        READ_BOUNDED_AFTER_OPEN_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::rename(&hook_path, &hook_moved).expect("move opened file");
                fs::write(&hook_path, b"replacement-is-deliberately-oversized")
                    .expect("replacement");
            }));
        });

        assert_eq!(read_bounded(&path, 16).expect("opened handle"), b"opened");
        assert_eq!(fs::read(&moved).expect("hook moved opened inode"), b"opened");
        assert_eq!(
            fs::read(&path).expect("hook installed replacement"),
            b"replacement-is-deliberately-oversized"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn bounded_read_rejects_an_opened_file_that_grows_past_the_limit() {
        let root = std::env::temp_dir().join(format!("t4e-bounded-growth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let path = root.join("input");
        fs::write(&path, b"opened").expect("input");
        let hook_path = path.clone();
        READ_BOUNDED_AFTER_METADATA_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let mut file =
                    OpenOptions::new().append(true).open(&hook_path).expect("append opened file");
                file.write_all(b"-grew-past-sixteen-bytes").expect("grow opened file");
            }));
        });

        assert!(
            read_bounded(&path, 16).is_err(),
            "growth after metadata must be caught by bounded read and post-read recheck"
        );
        assert!(fs::metadata(&path).expect("grown file").len() > 16);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(not(feature = "arm-live-egress"))]
    #[test]
    fn genesis_export_prepares_authenticated_zero_population_and_projection() {
        let root =
            std::env::temp_dir().join(format!("t4e-genesis-provisioning-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let export = root.join("export.json");
        fixture_genesis_export(&export);

        let population_request = root.join("population-request");
        T4eProvisioningTool::prepare_population(&export, &population_request)
            .expect("prepare genesis population");
        let population_preimage =
            fs::read(population_request.join("preimage.bin")).expect("population preimage");
        let population_signature = eip191_sign(&population_preimage, &owner_key());
        let population = ProducerConformance::attach_population_signature_bytes(
            population_preimage,
            population_signature,
        )
        .expect("attach genesis population");
        let decoded_population =
            crate::arm::FrozenP2PopulationManifestV1::decode_checked(population.canonical_bytes())
                .expect("decode genesis population");
        assert_eq!(decoded_population.population_kind(), PopulationKindV1::Genesis);
        assert!(decoded_population.entries().is_empty());
        let population_bytes = population.canonical_bytes();
        let population_kind_offset = population_bytes.len() - (1 + 32 + 65);
        assert_eq!(population_bytes[population_kind_offset], PopulationKindV1::Genesis.encode());

        let population_path = root.join("population.bin");
        fs::write(&population_path, population.canonical_bytes()).expect("population");
        let projection_fields = root.join("projection-fields.json");
        fs::write(
            &projection_fields,
            format!(
                "{{\"projection_sequence\":\"1\",\"finalized_block_number\":\"1100\",\
                 \"finalized_block_hash\":\"0x{}\",\"previous_content_hash\":\"0x{}\",\
                 \"campaign_valid_until_block\":\"2000\"}}",
                "44".repeat(32),
                "00".repeat(32),
            ),
        )
        .expect("projection fields");
        let projection_request = root.join("projection-request");
        T4eProvisioningTool::prepare_projection(
            &export,
            &population_path,
            &projection_fields,
            &projection_request,
        )
        .expect("prepare genesis projection");
        let projection_body =
            fs::read(projection_request.join("unsigned.bin")).expect("projection body");
        let projection_preimage =
            fs::read(projection_request.join("preimage.bin")).expect("projection preimage");
        let projection_signature = eip191_sign(&projection_preimage, &owner_key());
        let projection = ProducerConformance::attach_projection_signature_bytes(
            projection_body,
            projection_preimage,
            projection_signature,
        )
        .expect("attach genesis projection");
        let decoded_projection = crate::arm::TerminalSettlementProjectionV1::decode_checked(
            projection.canonical_bytes(),
        )
        .expect("decode genesis projection");
        assert_eq!(decoded_projection.population_kind(), PopulationKindV1::Genesis);
        assert_eq!(decoded_projection.campaign_valid_until_block(), 2_000);
        assert_eq!(decoded_projection.total_settled_loss_wei(), U256::ZERO);
        let projection_bytes = projection.canonical_bytes();
        let projection_kind_offset = projection_bytes.len() - (1 + 8 + 32 + 65);
        assert_eq!(projection_bytes[projection_kind_offset], PopulationKindV1::Genesis.encode());
        assert_eq!(
            &projection_bytes[projection_kind_offset + 1..projection_kind_offset + 9],
            &2_000u64.to_be_bytes()
        );

        let mut zero_kind = projection_bytes.to_vec();
        zero_kind[projection_kind_offset] = 0;
        assert!(crate::arm::TerminalSettlementProjectionV1::decode_checked(&zero_kind).is_err());

        let mut expiry_after_signature = Vec::with_capacity(projection_bytes.len());
        expiry_after_signature.extend_from_slice(&projection_bytes[..projection_kind_offset + 1]);
        expiry_after_signature.extend_from_slice(&projection_bytes[projection_kind_offset + 9..]);
        expiry_after_signature.extend_from_slice(
            &projection_bytes[projection_kind_offset + 1..projection_kind_offset + 9],
        );
        assert!(
            crate::arm::TerminalSettlementProjectionV1::decode_checked(&expiry_after_signature,)
                .is_err()
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn export_prepare_attach_all_three_artifacts_without_a_key_surface() {
        let root =
            std::env::temp_dir().join(format!("t4e-provisioning-tool-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let export = root.join("export.json");
        fixture_export(&export);
        let _ = prepare_signed_artifacts(&root, &export);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(feature = "arm-provisioning")]
    #[test]
    #[ignore = "must run in an isolated namespace for compile-pinned paths"]
    fn real_export_publishes_all_artifacts_and_prepare_complete_accepts() {
        let export = PathBuf::from(
            std::env::var("BASE_MEV_T4E_E2E_EXPORT").expect("real Postgres export path"),
        );
        let root =
            std::env::temp_dir().join(format!("t4e-provisioning-publish-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("root");
        let (population, projection, install) = prepare_signed_artifacts(&root, &export);

        let population_path = root.join("population.bin");
        let projection_path = root.join("projection.bin");
        let install_path = root.join("install.bin");
        fs::write(&population_path, population).expect("population file");
        fs::write(&projection_path, projection).expect("projection file");
        fs::write(&install_path, install).expect("install file");
        for path in [&population_path, &projection_path, &install_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("private artifact mode");
        }
        ProducerConformance::publish_population_file(&population_path).expect("publish population");
        ProducerConformance::publish_projection_file(&projection_path).expect("publish projection");
        ProducerConformance::publish_install_bundle_file(&install_path).expect("publish install");

        let prepared = NodeLocalSettledLossAuthority::prepare_complete(TestChain {
            head: BlockNumHash { number: 1_100, hash: B256::repeat_byte(0x44) },
            hashes: BTreeMap::from([
                (1_001, B256::repeat_byte(0x33)),
                (1_100, B256::repeat_byte(0x44)),
            ]),
        })
        .expect("prepare complete");
        assert_eq!(prepared.campaign_id(), B256::repeat_byte(0x99));
        fs::remove_dir_all(root).expect("cleanup");
    }
}
