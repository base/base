//! Consumer-level seal for the forgeable dormant provisioning snapshot.
#![cfg(feature = "b5-dormant-presign")]

use alloy_primitives::B256;
use mev_trader_submit::{
    AuthenticatedProvisioningSnapshot, B5_DORMANT_PROVISIONING_DOMAIN, DomainSeparatedSha256,
    ProvisioningSnapshotError,
};

type EightScalarBuilder =
    fn(
        u64,
        B256,
        B256,
        B256,
        [u8; 20],
        B256,
        B256,
        B256,
    ) -> Result<AuthenticatedProvisioningSnapshot, ProvisioningSnapshotError>;

const EIGHT_SCALAR_BUILDER: EightScalarBuilder =
    AuthenticatedProvisioningSnapshot::from_authenticated_bindings;

const CHAIN_ID: u64 = 8453;
const MANIFEST: B256 = B256::repeat_byte(0x11);
const VALUE_SET: B256 = B256::repeat_byte(0x22);
const EVIDENCE: B256 = B256::repeat_byte(0x33);
const SOURCE_COMMIT: [u8; 20] = [0x44; 20];
const RELEASE: B256 = B256::repeat_byte(0x55);
const REVIEW_FILE: B256 = B256::repeat_byte(0x66);
const REVIEW_DIGEST: B256 = B256::repeat_byte(0x77);

fn build(
    chain_id: u64,
    manifest: B256,
    value_set: B256,
    evidence: B256,
    source_commit: [u8; 20],
    release: B256,
    review_file: B256,
    review_digest: B256,
) -> Result<AuthenticatedProvisioningSnapshot, ProvisioningSnapshotError> {
    EIGHT_SCALAR_BUILDER(
        chain_id,
        manifest,
        value_set,
        evidence,
        source_commit,
        release,
        review_file,
        review_digest,
    )
}

#[test]
fn seals_eight_scalar_order_accessors_and_n11() {
    let snapshot = build(
        CHAIN_ID,
        MANIFEST,
        VALUE_SET,
        EVIDENCE,
        SOURCE_COMMIT,
        RELEASE,
        REVIEW_FILE,
        REVIEW_DIGEST,
    )
    .expect("distinct synthetic scalars must form a snapshot");

    assert_eq!(snapshot.chain_id(), CHAIN_ID);
    assert_eq!(snapshot.manifest_sha256(), MANIFEST);
    assert_eq!(snapshot.value_set_sha256(), VALUE_SET);
    assert_eq!(snapshot.deployment_evidence_sha256(), EVIDENCE);
    assert_eq!(snapshot.source_commit(), SOURCE_COMMIT);
    assert_eq!(snapshot.release_artifact_sha256(), RELEASE);
    assert_eq!(snapshot.deployment_review_file_sha256(), REVIEW_FILE);
    assert_eq!(snapshot.deployment_review_digest(), REVIEW_DIGEST);

    let payload = concat!(
        r#"{"chain_id":8453"#,
        r#","deployment_evidence_sha256":"3333333333333333333333333333333333333333333333333333333333333333""#,
        r#","deployment_review_digest":"7777777777777777777777777777777777777777777777777777777777777777""#,
        r#","deployment_review_sha256":"6666666666666666666666666666666666666666666666666666666666666666""#,
        r#","manifest_sha256":"1111111111111111111111111111111111111111111111111111111111111111""#,
        r#","release_artifact_sha256":"5555555555555555555555555555555555555555555555555555555555555555""#,
        r#","source_commit":"4444444444444444444444444444444444444444""#,
        r#","value_set_sha256":"2222222222222222222222222222222222222222222222222222222222222222"}"#,
    );
    let expected_n11 =
        DomainSeparatedSha256::digest(B5_DORMANT_PROVISIONING_DOMAIN, payload.as_bytes())
            .expect("fixed public domain and bounded payload must frame");
    assert_eq!(snapshot.snapshot_digest(), expected_n11);
}

#[test]
fn all_zero_scalars_are_publicly_forgeable_and_convey_no_authority() {
    let first = build(
        CHAIN_ID,
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
        [0; 20],
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
    )
    .expect("all-zero scalar values are structurally admissible");
    let independently_forged = build(
        CHAIN_ID,
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
        [0; 20],
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
    )
    .expect("any caller can forge the same value");

    assert_eq!(first, independently_forged);
    assert_eq!(first.manifest_sha256(), B256::ZERO);
    assert_eq!(first.value_set_sha256(), B256::ZERO);
    assert_eq!(first.deployment_evidence_sha256(), B256::ZERO);
    assert_eq!(first.source_commit(), [0; 20]);
    assert_eq!(first.release_artifact_sha256(), B256::ZERO);
    assert_eq!(first.deployment_review_file_sha256(), B256::ZERO);
    assert_eq!(first.deployment_review_digest(), B256::ZERO);
}

#[test]
fn rejects_every_synthetic_non_sole_chain() {
    for chain_id in [0, 1, 8452, 8454, u64::MAX] {
        assert_eq!(
            build(
                chain_id,
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                SOURCE_COMMIT,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            Err(ProvisioningSnapshotError::UnsupportedChainId),
            "chain id {chain_id} must be rejected",
        );
    }
}
