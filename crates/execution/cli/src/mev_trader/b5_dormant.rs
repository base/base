//! B5-1a dormant strict typed provisioning-binding verification (P1 slice).
//!
//! Private default-off child of `mev_trader`, compiled only under the
//! `b5-dormant-presign` feature. This slice owns the child-private three-input
//! verifier [`verify_provisioning_bindings_against`] — checks 1–8 in fixed
//! order, then the pure eight-scalar snapshot builder — together with its
//! synthetic-only unit tests. It deliberately contains no reviewed production
//! constant, no two-input production wrapper, and no hook callsite; Commit B
//! alone adds those on top of this compiled surface.

use std::{error::Error, fmt};

use alloy_primitives::B256;
use mev_trader_submit::AuthenticatedProvisioningSnapshot;

/// The only chain identity both binding sides may claim: Base mainnet.
const BOUND_CHAIN_ID: u64 = 8453;

/// Decoded pinned source commit `6a4258f646c6aeb65fd825abbcbd878d93af6e43`,
/// held as a local `[u8; 20]` comparison value rather than a Commit-B reviewed
/// authority constant.
const PINNED_SOURCE_COMMIT: [u8; 20] = [
    0x6a, 0x42, 0x58, 0xf6, 0x46, 0xc6, 0xae, 0xb6, 0x5f, 0xd8, 0x25, 0xab, 0xbc, 0xbd, 0x87, 0x8d,
    0x93, 0xaf, 0x6e, 0x43,
];

/// Strict-reader manifest observations carried as typed scalars, so that every
/// mismatch is a typed closed error rather than a provenance lookup.
pub(super) struct VerifiedProvisioningManifestBinding {
    /// Wire `chain_id` decoded from the strict manifest.
    pub(super) chain_id: u64,
    /// Ordinary SHA-256 of the exact observed manifest bytes (N10 position).
    pub(super) observed_manifest_sha256: B256,
    /// `value_set_sha256` decoded from the manifest wire (N8 position).
    pub(super) decoded_value_set_sha256: B256,
    /// `value_set_sha256` recomputed from the decoded closed records (N8 position).
    pub(super) recomputed_value_set_sha256: B256,
    /// Deployment-evidence digest decoded from the manifest (N4 position).
    pub(super) deployment_evidence_sha256: B256,
    /// Source commit decoded from the manifest (N0 position).
    pub(super) source_commit: [u8; 20],
    /// Release-artifact digest decoded from the manifest (N2 position).
    pub(super) release_artifact_sha256: B256,
    /// Deployment-review file digest decoded from the manifest (N6 position).
    pub(super) deployment_review_file_sha256: B256,
}

/// One reviewed provisioning binding: the shape Commit B will later populate
/// with the actual B5-P-reviewed scalars. In this P1 slice only synthetic test
/// values ever inhabit it.
#[derive(Clone, Copy)]
pub(super) struct CommitBReviewedProvisioningBinding {
    /// Reviewed chain identity.
    pub(super) chain_id: u64,
    /// Reviewed manifest digest (N10 position).
    pub(super) manifest_sha256: B256,
    /// Reviewed sole closed-record aggregate (N8 position).
    pub(super) value_set_sha256: B256,
    /// Reviewed deployment-evidence digest (N4 position).
    pub(super) deployment_evidence_sha256: B256,
    /// Reviewed source commit (N0 position).
    pub(super) source_commit: [u8; 20],
    /// Reviewed release-artifact digest (N2 position).
    pub(super) release_artifact_sha256: B256,
    /// Reviewed deployment-review file digest (N6 position).
    pub(super) deployment_review_file_sha256: B256,
    /// Reviewed semantic deployment-review digest (N7 position).
    pub(super) deployment_review_digest: B256,
}

/// Closed binding-verification error. Every rendering — `Debug`, `Display` and
/// the `Error` impl — exposes only the variant class; no value, digest, path,
/// source or wrapped message ever escapes.
pub(super) enum B5ProvisioningBindingError {
    /// Check 1: a chain identity is not exactly Base mainnet on both sides.
    ChainIdMismatch,
    /// Check 2: observed manifest digest differs from the reviewed one.
    ManifestBindingMismatch,
    /// Check 3: decoded, recomputed and reviewed value-set digests disagree.
    ValueSetBindingMismatch,
    /// Check 4: deployment-evidence digests disagree.
    EvidenceBindingMismatch,
    /// Check 5: a source commit differs from the pinned source literal.
    SourceCommitBindingMismatch,
    /// Check 6: release-artifact digests disagree.
    ReleaseArtifactBindingMismatch,
    /// Check 7: deployment-review file digests disagree.
    ReviewFileBindingMismatch,
    /// Check 8: reviewed semantic review digest differs from the expected one.
    ReviewSemanticBindingMismatch,
    /// The eight-scalar snapshot builder rejected the bound scalars; the
    /// builder's own error is deliberately discarded, never wrapped.
    BuilderRejected,
}

impl B5ProvisioningBindingError {
    /// The redacted class label — the only information any rendering exposes.
    const fn class(&self) -> &'static str {
        match self {
            Self::ChainIdMismatch => "ChainIdMismatch",
            Self::ManifestBindingMismatch => "ManifestBindingMismatch",
            Self::ValueSetBindingMismatch => "ValueSetBindingMismatch",
            Self::EvidenceBindingMismatch => "EvidenceBindingMismatch",
            Self::SourceCommitBindingMismatch => "SourceCommitBindingMismatch",
            Self::ReleaseArtifactBindingMismatch => "ReleaseArtifactBindingMismatch",
            Self::ReviewFileBindingMismatch => "ReviewFileBindingMismatch",
            Self::ReviewSemanticBindingMismatch => "ReviewSemanticBindingMismatch",
            Self::BuilderRejected => "BuilderRejected",
        }
    }
}

impl fmt::Debug for B5ProvisioningBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.class())
    }
}

impl fmt::Display for B5ProvisioningBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.class())
    }
}

impl Error for B5ProvisioningBindingError {}

/// Verifies the eight binding checks in fixed order — chain identity, manifest
/// digest, value-set three-way equality, deployment evidence, pinned source
/// commit, release artifact, review file, and the reviewed-vs-expected semantic
/// review digest — and only then delegates the reviewed scalars, in the fixed
/// eight-scalar order, to the pure snapshot builder. Builder rejection maps
/// opaquely to [`B5ProvisioningBindingError::BuilderRejected`].
fn verify_provisioning_bindings_against(
    manifest: VerifiedProvisioningManifestBinding,
    reviewed: CommitBReviewedProvisioningBinding,
    expected: CommitBReviewedProvisioningBinding,
) -> Result<AuthenticatedProvisioningSnapshot, B5ProvisioningBindingError> {
    if manifest.chain_id != BOUND_CHAIN_ID || reviewed.chain_id != BOUND_CHAIN_ID {
        return Err(B5ProvisioningBindingError::ChainIdMismatch);
    }
    if manifest.observed_manifest_sha256 != reviewed.manifest_sha256 {
        return Err(B5ProvisioningBindingError::ManifestBindingMismatch);
    }
    if manifest.decoded_value_set_sha256 != manifest.recomputed_value_set_sha256
        || manifest.decoded_value_set_sha256 != reviewed.value_set_sha256
    {
        return Err(B5ProvisioningBindingError::ValueSetBindingMismatch);
    }
    if manifest.deployment_evidence_sha256 != reviewed.deployment_evidence_sha256 {
        return Err(B5ProvisioningBindingError::EvidenceBindingMismatch);
    }
    if manifest.source_commit != PINNED_SOURCE_COMMIT
        || reviewed.source_commit != PINNED_SOURCE_COMMIT
    {
        return Err(B5ProvisioningBindingError::SourceCommitBindingMismatch);
    }
    if manifest.release_artifact_sha256 != reviewed.release_artifact_sha256 {
        return Err(B5ProvisioningBindingError::ReleaseArtifactBindingMismatch);
    }
    if manifest.deployment_review_file_sha256 != reviewed.deployment_review_file_sha256 {
        return Err(B5ProvisioningBindingError::ReviewFileBindingMismatch);
    }
    if reviewed.deployment_review_digest != expected.deployment_review_digest {
        return Err(B5ProvisioningBindingError::ReviewSemanticBindingMismatch);
    }
    AuthenticatedProvisioningSnapshot::from_authenticated_bindings(
        reviewed.chain_id,
        reviewed.manifest_sha256,
        reviewed.value_set_sha256,
        reviewed.deployment_evidence_sha256,
        reviewed.source_commit,
        reviewed.release_artifact_sha256,
        reviewed.deployment_review_file_sha256,
        reviewed.deployment_review_digest,
    )
    .map_err(|_| B5ProvisioningBindingError::BuilderRejected)
}
const _: fn(
    VerifiedProvisioningManifestBinding,
    CommitBReviewedProvisioningBinding,
    CommitBReviewedProvisioningBinding,
) -> Result<AuthenticatedProvisioningSnapshot, B5ProvisioningBindingError> =
    verify_provisioning_bindings_against;

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_MANIFEST_SHA256: B256 = B256::repeat_byte(0xa1);
    const SYNTHETIC_VALUE_SET_SHA256: B256 = B256::repeat_byte(0xa2);
    const SYNTHETIC_EVIDENCE_SHA256: B256 = B256::repeat_byte(0xa3);
    const SYNTHETIC_RELEASE_ARTIFACT_SHA256: B256 = B256::repeat_byte(0xa4);
    const SYNTHETIC_REVIEW_FILE_SHA256: B256 = B256::repeat_byte(0xa5);
    const SYNTHETIC_REVIEW_DIGEST: B256 = B256::repeat_byte(0xa6);
    const MUTATED_DIGEST: B256 = B256::repeat_byte(0xff);
    const MUTATED_COMMIT: [u8; 20] = [0xff; 20];
    const MUTATED_CHAIN_ID: u64 = 1;

    const fn synthetic_manifest() -> VerifiedProvisioningManifestBinding {
        VerifiedProvisioningManifestBinding {
            chain_id: BOUND_CHAIN_ID,
            observed_manifest_sha256: SYNTHETIC_MANIFEST_SHA256,
            decoded_value_set_sha256: SYNTHETIC_VALUE_SET_SHA256,
            recomputed_value_set_sha256: SYNTHETIC_VALUE_SET_SHA256,
            deployment_evidence_sha256: SYNTHETIC_EVIDENCE_SHA256,
            source_commit: PINNED_SOURCE_COMMIT,
            release_artifact_sha256: SYNTHETIC_RELEASE_ARTIFACT_SHA256,
            deployment_review_file_sha256: SYNTHETIC_REVIEW_FILE_SHA256,
        }
    }

    const fn synthetic_reviewed() -> CommitBReviewedProvisioningBinding {
        CommitBReviewedProvisioningBinding {
            chain_id: BOUND_CHAIN_ID,
            manifest_sha256: SYNTHETIC_MANIFEST_SHA256,
            value_set_sha256: SYNTHETIC_VALUE_SET_SHA256,
            deployment_evidence_sha256: SYNTHETIC_EVIDENCE_SHA256,
            source_commit: PINNED_SOURCE_COMMIT,
            release_artifact_sha256: SYNTHETIC_RELEASE_ARTIFACT_SHA256,
            deployment_review_file_sha256: SYNTHETIC_REVIEW_FILE_SHA256,
            deployment_review_digest: SYNTHETIC_REVIEW_DIGEST,
        }
    }

    /// Runs the verifier with an independently constructed synthetic
    /// `expected` binding, as Commit B's reviewed constant will later be.
    fn verify(
        manifest: VerifiedProvisioningManifestBinding,
        reviewed: CommitBReviewedProvisioningBinding,
    ) -> Result<AuthenticatedProvisioningSnapshot, B5ProvisioningBindingError> {
        verify_provisioning_bindings_against(manifest, reviewed, synthetic_reviewed())
    }

    fn assert_renders_only_class(error: &B5ProvisioningBindingError, class: &str) {
        assert_eq!(format!("{error:?}"), class);
        assert_eq!(format!("{error}"), class);
    }

    #[test]
    fn accepts_internally_consistent_synthetic_bindings() {
        assert!(verify(synthetic_manifest(), synthetic_reviewed()).is_ok());
    }

    #[test]
    fn accepts_expected_as_direct_copy_of_reviewed() {
        let reviewed = synthetic_reviewed();
        let expected = reviewed;
        assert!(
            verify_provisioning_bindings_against(synthetic_manifest(), reviewed, expected).is_ok()
        );
    }

    #[test]
    fn pinned_source_commit_decodes_the_pinned_literal() {
        let rendered: String =
            PINNED_SOURCE_COMMIT.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(rendered, "6a4258f646c6aeb65fd825abbcbd878d93af6e43");
    }

    #[test]
    fn edge_1_rejects_any_non_base_chain_id() {
        let mut manifest = synthetic_manifest();
        manifest.chain_id = MUTATED_CHAIN_ID;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ChainIdMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.chain_id = MUTATED_CHAIN_ID;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ChainIdMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.chain_id = MUTATED_CHAIN_ID;
        let mut reviewed = synthetic_reviewed();
        reviewed.chain_id = MUTATED_CHAIN_ID;
        assert!(matches!(
            verify(manifest, reviewed),
            Err(B5ProvisioningBindingError::ChainIdMismatch)
        ));
    }

    #[test]
    fn edge_2_rejects_manifest_digest_mismatch_on_either_side() {
        let mut manifest = synthetic_manifest();
        manifest.observed_manifest_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ManifestBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.manifest_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ManifestBindingMismatch)
        ));
    }

    #[test]
    fn edge_3_rejects_each_value_set_subedge_mismatch() {
        let mut manifest = synthetic_manifest();
        manifest.decoded_value_set_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ValueSetBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.recomputed_value_set_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ValueSetBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.value_set_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ValueSetBindingMismatch)
        ));

        // Decoded and recomputed agreeing with each other is insufficient when
        // both differ from the reviewed aggregate.
        let mut manifest = synthetic_manifest();
        manifest.decoded_value_set_sha256 = MUTATED_DIGEST;
        manifest.recomputed_value_set_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ValueSetBindingMismatch)
        ));
    }

    #[test]
    fn edge_4_rejects_evidence_digest_mismatch_on_either_side() {
        let mut manifest = synthetic_manifest();
        manifest.deployment_evidence_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::EvidenceBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_evidence_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::EvidenceBindingMismatch)
        ));
    }

    #[test]
    fn edge_5_rejects_any_source_commit_off_the_pinned_literal() {
        let mut manifest = synthetic_manifest();
        manifest.source_commit = MUTATED_COMMIT;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::SourceCommitBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.source_commit = MUTATED_COMMIT;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::SourceCommitBindingMismatch)
        ));

        // Manifest and reviewed agreeing on a non-pinned commit is still a
        // mismatch against the decoded pinned literal.
        let mut manifest = synthetic_manifest();
        manifest.source_commit = MUTATED_COMMIT;
        let mut reviewed = synthetic_reviewed();
        reviewed.source_commit = MUTATED_COMMIT;
        assert!(matches!(
            verify(manifest, reviewed),
            Err(B5ProvisioningBindingError::SourceCommitBindingMismatch)
        ));
    }

    #[test]
    fn edge_6_rejects_release_artifact_mismatch_on_either_side() {
        let mut manifest = synthetic_manifest();
        manifest.release_artifact_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ReleaseArtifactBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.release_artifact_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ReleaseArtifactBindingMismatch)
        ));
    }

    #[test]
    fn edge_7_rejects_review_file_mismatch_on_either_side() {
        let mut manifest = synthetic_manifest();
        manifest.deployment_review_file_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ReviewFileBindingMismatch)
        ));

        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_review_file_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ReviewFileBindingMismatch)
        ));
    }

    #[test]
    fn edge_8_rejects_review_semantic_digest_mismatch_on_either_side() {
        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_review_digest = MUTATED_DIGEST;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ReviewSemanticBindingMismatch)
        ));

        let mut expected = synthetic_reviewed();
        expected.deployment_review_digest = MUTATED_DIGEST;
        assert!(matches!(
            verify_provisioning_bindings_against(
                synthetic_manifest(),
                synthetic_reviewed(),
                expected
            ),
            Err(B5ProvisioningBindingError::ReviewSemanticBindingMismatch)
        ));
    }

    #[test]
    fn n6_n7_swap_in_reviewed_fails_the_review_file_edge_first() {
        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_review_file_sha256 = SYNTHETIC_REVIEW_DIGEST;
        reviewed.deployment_review_digest = SYNTHETIC_REVIEW_FILE_SHA256;
        assert!(matches!(
            verify(synthetic_manifest(), reviewed),
            Err(B5ProvisioningBindingError::ReviewFileBindingMismatch)
        ));
    }

    #[test]
    fn n6_n7_swap_consistent_across_manifest_and_reviewed_fails_semantically() {
        // A swap that manifest and reviewed agree on slips past check 7; the
        // independent expected semantic digest still rejects it at check 8.
        let mut manifest = synthetic_manifest();
        manifest.deployment_review_file_sha256 = SYNTHETIC_REVIEW_DIGEST;
        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_review_file_sha256 = SYNTHETIC_REVIEW_DIGEST;
        reviewed.deployment_review_digest = SYNTHETIC_REVIEW_FILE_SHA256;
        assert!(matches!(
            verify(manifest, reviewed),
            Err(B5ProvisioningBindingError::ReviewSemanticBindingMismatch)
        ));
    }

    #[test]
    fn expected_binds_only_through_the_review_semantic_digest() {
        // `expected` is a full independent synthetic reviewed binding, but it
        // participates in exactly one check: mutating every field other than
        // its semantic review digest leaves checks 1-8 satisfied.
        let mutations: [fn(&mut CommitBReviewedProvisioningBinding); 7] = [
            |expected| expected.chain_id = MUTATED_CHAIN_ID,
            |expected| expected.manifest_sha256 = MUTATED_DIGEST,
            |expected| expected.value_set_sha256 = MUTATED_DIGEST,
            |expected| expected.deployment_evidence_sha256 = MUTATED_DIGEST,
            |expected| expected.source_commit = MUTATED_COMMIT,
            |expected| expected.release_artifact_sha256 = MUTATED_DIGEST,
            |expected| expected.deployment_review_file_sha256 = MUTATED_DIGEST,
        ];
        for mutate in mutations {
            let mut expected = synthetic_reviewed();
            mutate(&mut expected);
            assert!(
                verify_provisioning_bindings_against(
                    synthetic_manifest(),
                    synthetic_reviewed(),
                    expected
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn checks_run_in_fixed_order_and_stop_at_the_first_failed_edge() {
        let mut manifest = synthetic_manifest();
        manifest.chain_id = MUTATED_CHAIN_ID;
        manifest.observed_manifest_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ChainIdMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.observed_manifest_sha256 = MUTATED_DIGEST;
        manifest.decoded_value_set_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ManifestBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.decoded_value_set_sha256 = MUTATED_DIGEST;
        manifest.deployment_evidence_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ValueSetBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.deployment_evidence_sha256 = MUTATED_DIGEST;
        manifest.source_commit = MUTATED_COMMIT;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::EvidenceBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.source_commit = MUTATED_COMMIT;
        manifest.release_artifact_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::SourceCommitBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.release_artifact_sha256 = MUTATED_DIGEST;
        manifest.deployment_review_file_sha256 = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, synthetic_reviewed()),
            Err(B5ProvisioningBindingError::ReleaseArtifactBindingMismatch)
        ));

        let mut manifest = synthetic_manifest();
        manifest.deployment_review_file_sha256 = MUTATED_DIGEST;
        let mut reviewed = synthetic_reviewed();
        reviewed.deployment_review_digest = MUTATED_DIGEST;
        assert!(matches!(
            verify(manifest, reviewed),
            Err(B5ProvisioningBindingError::ReviewFileBindingMismatch)
        ));
    }

    #[test]
    fn errors_render_exactly_their_variant_class_and_nothing_else() {
        assert_renders_only_class(&B5ProvisioningBindingError::ChainIdMismatch, "ChainIdMismatch");
        assert_renders_only_class(
            &B5ProvisioningBindingError::ManifestBindingMismatch,
            "ManifestBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::ValueSetBindingMismatch,
            "ValueSetBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::EvidenceBindingMismatch,
            "EvidenceBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::SourceCommitBindingMismatch,
            "SourceCommitBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::ReleaseArtifactBindingMismatch,
            "ReleaseArtifactBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::ReviewFileBindingMismatch,
            "ReviewFileBindingMismatch",
        );
        assert_renders_only_class(
            &B5ProvisioningBindingError::ReviewSemanticBindingMismatch,
            "ReviewSemanticBindingMismatch",
        );
        assert_renders_only_class(&B5ProvisioningBindingError::BuilderRejected, "BuilderRejected");
        assert!(Error::source(&B5ProvisioningBindingError::BuilderRejected).is_none());
    }
}
