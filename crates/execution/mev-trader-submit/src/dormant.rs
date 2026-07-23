//! B5-1a dormant provisioning-snapshot value surface (`presign` tier).
//!
//! This module is the P1 "dormant" slice of the B5-1a plan: a pure, in-memory
//! digest surface with NO provider, filesystem, environment, receipt, compiled
//! constant, global, or callback access. It compiles only under the `presign`
//! feature, whose direct dependency allowlist is exactly
//! `{alloy-primitives, sha2}`.
//!
//! Everything here is **forgeable and non-authorizing**: any caller can build
//! any [`AuthenticatedProvisioningSnapshot`] from any scalar bit patterns, so
//! possessing a snapshot (or its digest) grants no arming, signing, network,
//! or operational authority. Authentication of the scalars happens strictly
//! *before* this builder, in the separately-reviewed CLI-private verifier; the
//! builder itself never reads or compares evidence artifacts, receipts, or
//! compiled authority.
//!
//! All semantic digests share one framing:
//!
//! ```text
//! SHA256(domain_ascii || 0x00 || u32_be(payload_len) || payload)
//! ```
//!
//! with a single-byte `0x00` separator, a big-endian `u32` payload byte length
//! (lengths above `u32::MAX` are rejected before hashing), and a non-empty
//! ASCII domain containing no NUL byte.

use alloy_primitives::{B256, hex};
use sha2::{Digest, Sha256};

/// Domain-separation prefix for the dormant provisioning snapshot digest
/// (plan node N11). The payload is the exact eight-field RFC 8785 JCS object
/// produced by
/// [`AuthenticatedProvisioningSnapshot::from_authenticated_bindings`].
pub const B5_DORMANT_PROVISIONING_DOMAIN: &str = "base-mev:b5-dormant:provisioning:v1";

/// Domain-separation prefix for the sole closed-record value-set aggregate
/// `value_set_sha256` (plan node N8). The payload is the RFC 8785 JCS object
/// holding exactly the manifest wire values `chain_id`, `coordination_lock`,
/// `r9`, `suppression_hw`, and `suppression_json`. There is no other
/// aggregate, and no combined source/release digest exists anywhere in this
/// surface.
pub const B5_PROVISIONING_VALUE_SET_DOMAIN: &str = "base-mev:b5-provisioning:value-set:v1";

/// Domain-separation prefix for the semantic deployment-review digest (plan
/// node N7). N7 is computed over the exact finalized canonical review bytes —
/// the very same bytes whose ordinary (un-domain-framed) SHA-256 is the
/// review-file hash N6. The two values are therefore always distinct
/// authorities over identical input bytes and are retained separately.
pub const B5_DEPLOYMENT_REVIEW_DOMAIN: &str = "base-mev:b5-deployment-review:v1";

/// The only chain id this dormant surface accepts (Base mainnet).
const SUPPORTED_CHAIN_ID: u64 = 8453;

/// A closed framing failure from [`DomainSeparatedSha256`]. Variants carry no
/// values, paths, bytes, or OS messages — only the failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFramingError {
    /// The domain string is empty, non-ASCII, or contains a NUL byte, any of
    /// which would make the `domain || 0x00 || …` framing ambiguous.
    InvalidDomain,
    /// The payload byte length is not representable as a `u32`, so the
    /// big-endian length prefix cannot frame it.
    PayloadLengthOverflow,
}

impl core::fmt::Display for DigestFramingError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidDomain => {
                formatter.write_str("digest domain must be non-empty ASCII without NUL")
            }
            Self::PayloadLengthOverflow => {
                formatter.write_str("digest payload length exceeds the u32 framing limit")
            }
        }
    }
}

impl core::error::Error for DigestFramingError {}

/// The single domain-separated SHA-256 framing used by every B5 semantic
/// digest (N7, N8, N11):
///
/// ```text
/// SHA256(domain_ascii || 0x00 || u32_be(payload_len) || payload)
/// ```
///
/// A unit struct rather than a bare function so the public API exports a
/// type. It holds no state and performs no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainSeparatedSha256;

impl DomainSeparatedSha256 {
    /// Computes `SHA256(domain || 0x00 || u32_be(payload.len()) || payload)`.
    ///
    /// Fails closed with [`DigestFramingError::InvalidDomain`] for an empty,
    /// non-ASCII, or NUL-bearing domain and with
    /// [`DigestFramingError::PayloadLengthOverflow`] when the payload length
    /// does not fit a `u32`. It never truncates, defaults, or retries.
    pub fn digest(domain: &str, payload: &[u8]) -> Result<B256, DigestFramingError> {
        Self::validate_domain(domain)?;
        let payload_len = Self::checked_payload_len(payload.len())?;
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update([0u8]);
        hasher.update(payload_len.to_be_bytes());
        hasher.update(payload);
        let digest: [u8; 32] = hasher.finalize().into();
        Ok(B256::new(digest))
    }

    /// The domain must be non-empty ASCII with no NUL byte: the `0x00`
    /// separator is exactly one byte, so a NUL inside (or a non-ASCII byte
    /// masquerading near) the domain would create framing ambiguity.
    const fn validate_domain(domain: &str) -> Result<(), DigestFramingError> {
        let bytes = domain.as_bytes();
        if bytes.is_empty() {
            return Err(DigestFramingError::InvalidDomain);
        }
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == 0 || bytes[index] > 0x7f {
                return Err(DigestFramingError::InvalidDomain);
            }
            index += 1;
        }
        Ok(())
    }

    /// Checked conversion of the payload byte length into the `u32` framing
    /// prefix; lengths above `u32::MAX` are a hard failure, never a wrap.
    const fn checked_payload_len(len: usize) -> Result<u32, DigestFramingError> {
        if len > u32::MAX as usize {
            return Err(DigestFramingError::PayloadLengthOverflow);
        }
        // The cast is exact: the bound check above guarantees `len <= u32::MAX`.
        Ok(len as u32)
    }
}

/// A closed builder failure from
/// [`AuthenticatedProvisioningSnapshot::from_authenticated_bindings`]. Only
/// intrinsic validation can fail — the builder performs no provenance lookup —
/// and variants expose only the failure class, never values, paths, bytes, or
/// OS messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningSnapshotError {
    /// The supplied chain id is not the single supported chain (8453).
    UnsupportedChainId,
    /// Framing the canonical eight-field payload failed.
    PayloadFraming(DigestFramingError),
}

impl core::fmt::Display for ProvisioningSnapshotError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedChainId => {
                formatter.write_str("provisioning snapshot chain id is not the supported chain")
            }
            Self::PayloadFraming(_) => {
                formatter.write_str("provisioning snapshot payload framing failed")
            }
        }
    }
}

impl core::error::Error for ProvisioningSnapshotError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::UnsupportedChainId => None,
            Self::PayloadFraming(inner) => Some(inner),
        }
    }
}

impl From<DigestFramingError> for ProvisioningSnapshotError {
    fn from(error: DigestFramingError) -> Self {
        Self::PayloadFraming(error)
    }
}

/// A dormant, forgeable, non-authorizing value snapshot of the eight
/// already-authenticated provisioning scalars plus their recomputed snapshot
/// digest (plan node N11).
///
/// The eight scalars are retained **separately** and never merged: the 20-byte
/// pinned source commit (N0), the release-artifact hash (N2), the deployment
/// evidence hash (N4), the ordinary review-file hash (N6), the semantic
/// review digest (N7), the sole value-set aggregate `value_set_sha256` (N8),
/// the manifest hash (N10), and the chain id. Holding this value conveys no
/// authority of any kind; anyone can construct an identical one from the same
/// scalars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedProvisioningSnapshot {
    chain_id: u64,
    manifest_sha256: B256,
    value_set_sha256: B256,
    deployment_evidence_sha256: B256,
    source_commit: [u8; 20],
    release_artifact_sha256: B256,
    deployment_review_file_sha256: B256,
    deployment_review_digest: B256,
    snapshot_digest: B256,
}

impl AuthenticatedProvisioningSnapshot {
    /// Builds the snapshot from the eight already-authenticated scalars and
    /// recomputes the snapshot digest N11.
    ///
    /// The builder is deterministic and provider/filesystem/environment/
    /// receipt/constant/global/callback-free. Its intrinsic validation is
    /// exactly: `chain_id == 8453`; fixed-width lowercase hex wire encoding
    /// generated from the supplied bytes; the exact eight-field RFC 8785 JCS
    /// payload below; the common `u32` length framing; and N11 construction
    /// under [`B5_DORMANT_PROVISIONING_DOMAIN`]. Every `B256` and `[u8; 20]`
    /// bit pattern is structurally admissible — mutating any scalar
    /// deterministically changes the payload and N11 instead of triggering
    /// any provenance lookup. It never reads or compares evidence files,
    /// receipts, or compiled authority.
    ///
    /// The canonical payload holds exactly these fields, in JCS (sorted) key
    /// order, with `chain_id` as a JSON number and every other value as
    /// fixed-width lowercase hex without a `0x` prefix:
    ///
    /// ```json
    /// {
    ///   "chain_id": 8453,
    ///   "deployment_evidence_sha256": "<64 hex>",
    ///   "deployment_review_digest": "<64 hex>",
    ///   "deployment_review_sha256": "<64 hex>",
    ///   "manifest_sha256": "<64 hex>",
    ///   "release_artifact_sha256": "<64 hex>",
    ///   "source_commit": "<40 hex>",
    ///   "value_set_sha256": "<64 hex>"
    /// }
    /// ```
    pub fn from_authenticated_bindings(
        chain_id: u64,
        manifest_sha256: B256,
        value_set_sha256: B256,
        deployment_evidence_sha256: B256,
        source_commit: [u8; 20],
        release_artifact_sha256: B256,
        deployment_review_file_sha256: B256,
        deployment_review_digest: B256,
    ) -> Result<Self, ProvisioningSnapshotError> {
        if chain_id != SUPPORTED_CHAIN_ID {
            return Err(ProvisioningSnapshotError::UnsupportedChainId);
        }
        // RFC 8785 JCS for this object is exactly: ASCII keys in sorted
        // order, no whitespace, the integer in its shortest decimal form, and
        // string values that contain no escapable characters (they are pure
        // lowercase hex). Building the bytes directly is therefore the exact
        // canonical serialization.
        let payload = format!(
            concat!(
                r#"{{"chain_id":{chain_id}"#,
                r#","deployment_evidence_sha256":"{evidence}""#,
                r#","deployment_review_digest":"{review_digest}""#,
                r#","deployment_review_sha256":"{review_file}""#,
                r#","manifest_sha256":"{manifest}""#,
                r#","release_artifact_sha256":"{release}""#,
                r#","source_commit":"{commit}""#,
                r#","value_set_sha256":"{value_set}"}}"#,
            ),
            chain_id = chain_id,
            evidence = hex::encode(deployment_evidence_sha256),
            review_digest = hex::encode(deployment_review_digest),
            review_file = hex::encode(deployment_review_file_sha256),
            manifest = hex::encode(manifest_sha256),
            release = hex::encode(release_artifact_sha256),
            commit = hex::encode(source_commit),
            value_set = hex::encode(value_set_sha256),
        );
        let snapshot_digest =
            DomainSeparatedSha256::digest(B5_DORMANT_PROVISIONING_DOMAIN, payload.as_bytes())?;
        Ok(Self {
            chain_id,
            manifest_sha256,
            value_set_sha256,
            deployment_evidence_sha256,
            source_commit,
            release_artifact_sha256,
            deployment_review_file_sha256,
            deployment_review_digest,
            snapshot_digest,
        })
    }

    /// The validated chain id (always 8453 for a constructed snapshot).
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The ordinary SHA-256 of the exact strict manifest bytes (N10).
    pub const fn manifest_sha256(&self) -> B256 {
        self.manifest_sha256
    }

    /// The sole closed-record value-set aggregate (N8). No other aggregate
    /// digest exists in this surface.
    pub const fn value_set_sha256(&self) -> B256 {
        self.value_set_sha256
    }

    /// The ordinary SHA-256 of the deployment-evidence fixture bytes (N4).
    pub const fn deployment_evidence_sha256(&self) -> B256 {
        self.deployment_evidence_sha256
    }

    /// The decoded 20-byte pinned source commit (N0), retained separately
    /// from the release-artifact hash — there is no combined source/release
    /// digest.
    pub const fn source_commit(&self) -> [u8; 20] {
        self.source_commit
    }

    /// The ordinary SHA-256 of the retained release binary bytes (N2),
    /// retained separately from the source commit.
    pub const fn release_artifact_sha256(&self) -> B256 {
        self.release_artifact_sha256
    }

    /// The ordinary SHA-256 of the exact finalized review-file bytes (N6),
    /// retained separately from the semantic review digest computed over the
    /// same bytes.
    pub const fn deployment_review_file_sha256(&self) -> B256 {
        self.deployment_review_file_sha256
    }

    /// The domain-framed semantic deployment-review digest (N7) under
    /// [`B5_DEPLOYMENT_REVIEW_DOMAIN`], retained separately from the ordinary
    /// review-file hash.
    pub const fn deployment_review_digest(&self) -> B256 {
        self.deployment_review_digest
    }

    /// The recomputed snapshot digest N11 over the exact eight-field JCS
    /// payload under [`B5_DORMANT_PROVISIONING_DOMAIN`]. Forgeable and
    /// non-authorizing, like the snapshot itself.
    pub const fn snapshot_digest(&self) -> B256 {
        self.snapshot_digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The u32 framing boundary cannot be reached through `digest` in a test
    // (it would need a >4 GiB allocation), so the checked length conversion is
    // exercised directly at the exact boundary.
    #[test]
    fn payload_length_at_the_u32_boundary_is_accepted() {
        assert_eq!(DomainSeparatedSha256::checked_payload_len(0), Ok(0));
        assert_eq!(DomainSeparatedSha256::checked_payload_len(u32::MAX as usize), Ok(u32::MAX));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn payload_length_above_the_u32_boundary_is_rejected() {
        assert_eq!(
            DomainSeparatedSha256::checked_payload_len(u32::MAX as usize + 1),
            Err(DigestFramingError::PayloadLengthOverflow)
        );
        assert_eq!(
            DomainSeparatedSha256::checked_payload_len(usize::MAX),
            Err(DigestFramingError::PayloadLengthOverflow)
        );
    }
}
