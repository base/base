//! B5-1a `presign` digest known-answer and mutation vectors.
//!
//! Plan verification lane: `cargo test -p mev-trader-submit --no-default-features
//! --features presign --test b5_digest_vectors --locked --frozen`.
//!
//! Every expected digest below is an independent known-answer vector computed
//! outside this crate (Python `hashlib`, straight from the plan's framing
//! definition `SHA256(domain_ascii || 0x00 || u32_be(payload_len) || payload)`
//! and the exact eight-field RFC 8785 JCS payload) — never by the code under
//! test. Coverage: per-scalar mutation, domain mutation, ASCII case mutation
//! of both the domain and the hex wire, RFC 8785 sorted-key order against a
//! noncanonical reordered-payload mutation, fixed-width lowercase wire against
//! stripped-leading-zero width mutation, NUL-separator boundary shifts, the
//! exact u32 length-conversion boundaries 0, 1, `u32::MAX`, and
//! `u32::MAX + 1` in the companion `presign --lib` lane, argument-order and
//! sensitivity, chain-id gate boundaries, and extreme all-zero/all-ones bit
//! patterns. The sole aggregate exercised is `value_set_sha256`; no other
//! aggregate digest name exists in this surface.
#![cfg(feature = "presign")]

use std::collections::BTreeSet;

use alloy_primitives::{B256, b256};
use mev_trader_submit::{
    AuthenticatedProvisioningSnapshot, B5_DEPLOYMENT_REVIEW_DOMAIN, B5_DORMANT_PROVISIONING_DOMAIN,
    B5_PROVISIONING_VALUE_SET_DOMAIN, DigestFramingError, DomainSeparatedSha256,
    ProvisioningSnapshotError,
};
use sha2::{Digest, Sha256};

/// Base scalar bit patterns behind the N11 known-answer vector: one distinct
/// repeating byte per sealed builder position (N10, N8, N4, N0, N2, N6, N7).
const MANIFEST: B256 = B256::repeat_byte(0x11);
const VALUE_SET: B256 = B256::repeat_byte(0x22);
const EVIDENCE: B256 = B256::repeat_byte(0x33);
const COMMIT: [u8; 20] = [0x44; 20];
const RELEASE: B256 = B256::repeat_byte(0x55);
const REVIEW_FILE: B256 = B256::repeat_byte(0x66);
const REVIEW_DIGEST: B256 = B256::repeat_byte(0x77);

/// Sample finalized-review bytes standing in for exact N5 bytes in the typed
/// N6 (ordinary hash) vs N7 (domain-framed digest) separation vectors.
const SAMPLE_REVIEW_BYTES: &[u8] = b"sample-review-bytes-v1";

/// Review-domain framing vectors at the small payload boundaries.
const REVIEW_EMPTY_PAYLOAD: B256 =
    b256!("166d58aa167f33ae5ccbcd57ac46b315524c72e55fadd05c6e46bfc9e6cc5cca");
const REVIEW_SINGLE_NUL_PAYLOAD: B256 =
    b256!("c3884d2c8d4aa3302c977feb015bb8ca9a67b3ec3d44415e0fadd512388344c4");
const REVIEW_PAYLOAD_A: B256 =
    b256!("9410b917b88e243eb84a23ef65810eb9ab1009eaf354eccaa2b3dc84d729fefa");
const REVIEW_PAYLOAD_AB: B256 =
    b256!("21d2de5045656dfcc4feaac388510bc8b7fde72a3b87f52a67434741da9a2803");

/// Typed digest separation over the identical sample bytes: the ordinary
/// SHA-256 (the N6 form) and the review-domain framed digest (the N7 form).
const SAMPLE_N6_ORDINARY_SHA256: B256 =
    b256!("8541815f5ffd41a29dfc4876dab7f0d7f435bd44f5279d529fd3896a3728d8ad");
const SAMPLE_N7_REVIEW_FRAMED: B256 =
    b256!("99de75ad96e6f3ea01e2b9539700c44e644473a9f58288d9f2a9861496238b95");

/// Domain mutation vectors over the same sample bytes: the dormant N11 domain
/// and the ASCII-uppercased review domain each select a different digest.
const SAMPLE_DORMANT_DOMAIN: B256 =
    b256!("84e8b6177a5186b28105dfce6521bd237d07388b46e466c78d86d24dc5481954");
const SAMPLE_UPPERCASE_REVIEW_DOMAIN: B256 =
    b256!("bf70a0b8b8f0af8620cd6a9c32f135bbdd48d24e3f95820721417733f57514d4");

/// Boundary-shift vectors: moving one byte across the NUL separator (domain
/// `"ab"`/payload `"c"` vs domain `"a"`/payload `"bc"`) must not collide.
const SHIFTED_DOMAIN_AB_PAYLOAD_C: B256 =
    b256!("e95acb9c464dcdd765943d1728999d4414963f6f4ad032a8f210522bb7183fd4");
const SHIFTED_DOMAIN_A_PAYLOAD_BC: B256 =
    b256!("167dd60fc4d09831e41dccbfff47dc9817779f933dc5ac48e53733a0d46c35df");

/// Sole-aggregate vector: the plan's five-key value-set JCS payload under the
/// N8 domain.
const VALUE_SET_SAMPLE_N8: B256 =
    b256!("c334ace833baf839805d81e82910db1cc74656eed6bab64f0dad5a092d7b51db");

/// Snapshot (N11) vectors for the base scalar set, its unframed payload hash,
/// the typed N6↔N7 argument swap, and the extreme bit patterns.
const BASE_N11: B256 = b256!("bef07422b53ed5e5f4ae39ce0b9e6f4b55ea6f75451bac519d3690a52e529b07");
const BASE_PAYLOAD_UNFRAMED_SHA256: B256 =
    b256!("6e0ef71b5e5f4f30c08c88674abb52d2592f8b230c706303d874e792cd78a224");
const SWAPPED_N6_N7_N11: B256 =
    b256!("6759c2472ac5061842fd1c365091e835cc7900d899c33274e64cec116f59e96d");
const ALL_ZERO_N11: B256 =
    b256!("f9f9386615a52f46bf69507f7b6fbacfed028e8e8cac13eed44041f163712d0b");
const ALL_ONES_N11: B256 =
    b256!("0372795c76f89aac644d846df15208f05d55303f2b12959b713667c74a57e49c");

/// Per-scalar mutation vectors: the base set with the low bit of the first
/// byte of exactly one scalar flipped, in builder-argument order.
const N11_MANIFEST_MUTATION: B256 =
    b256!("8a8138d381d7303ddc49f48092ac00873a0ec473f3c2ef1ed813a8b2914d6265");
const N11_VALUE_SET_MUTATION: B256 =
    b256!("5aa2d3d0835a3c4a1b3c1566a988fffda72a83fcb520fc3abf51949aacecdbcc");
const N11_EVIDENCE_MUTATION: B256 =
    b256!("ce44b52aece3dff826446380a8d249c00fce708c640716debf8d0ea89a09558b");
const N11_COMMIT_MUTATION: B256 =
    b256!("7fff8e72347cfb06bb507103666c4072f2709c303f70fe4ebeedcaf473280eae");
const N11_RELEASE_MUTATION: B256 =
    b256!("02e71a791bbb4961047ad26668b327d2100a2fef4fa2925055ec116dacc303ce");
const N11_REVIEW_FILE_MUTATION: B256 =
    b256!("04626fe127783d164ef870e73c817d2251788e48a491ac168a324b1df46ca45f");
const N11_REVIEW_DIGEST_MUTATION: B256 =
    b256!("7b0f9d80bc4a7ee269f6dc9c9d7237ae9bd12e0b7aab4937ec4538e2351f869e");

/// Builds a snapshot on the sole supported chain; every bit pattern is
/// structurally admissible there, so a failure is itself a seal violation.
fn snapshot(
    manifest: B256,
    value_set: B256,
    evidence: B256,
    commit: [u8; 20],
    release: B256,
    review_file: B256,
    review_digest: B256,
) -> AuthenticatedProvisioningSnapshot {
    AuthenticatedProvisioningSnapshot::from_authenticated_bindings(
        8453,
        manifest,
        value_set,
        evidence,
        commit,
        release,
        review_file,
        review_digest,
    )
    .expect("every scalar bit pattern must be structurally admissible on chain 8453")
}

/// The internally consistent base snapshot behind [`BASE_N11`].
fn base_snapshot() -> AuthenticatedProvisioningSnapshot {
    snapshot(MANIFEST, VALUE_SET, EVIDENCE, COMMIT, RELEASE, REVIEW_FILE, REVIEW_DIGEST)
}

/// Flips the low bit of the first byte — the smallest scalar mutation.
const fn flip_low_bit(scalar: B256) -> B256 {
    let mut bytes = scalar.0;
    bytes[0] ^= 0x01;
    B256::new(bytes)
}

/// Frames a payload under the dormant N11 domain.
fn frame_dormant(payload: &str) -> B256 {
    DomainSeparatedSha256::digest(B5_DORMANT_PROVISIONING_DOMAIN, payload.as_bytes())
        .expect("an ASCII payload under the dormant domain must frame")
}

/// The exact canonical RFC 8785 JCS payload for the base scalar set,
/// reconstructed independently from string literals: sorted keys, no
/// whitespace, `chain_id` as a bare number, fixed-width lowercase hex wire.
fn base_canonical_payload() -> String {
    format!(
        concat!(
            r#"{{"chain_id":8453"#,
            r#","deployment_evidence_sha256":"{evidence}""#,
            r#","deployment_review_digest":"{review_digest}""#,
            r#","deployment_review_sha256":"{review_file}""#,
            r#","manifest_sha256":"{manifest}""#,
            r#","release_artifact_sha256":"{release}""#,
            r#","source_commit":"{commit}""#,
            r#","value_set_sha256":"{value_set}"}}"#,
        ),
        evidence = "33".repeat(32),
        review_digest = "77".repeat(32),
        review_file = "66".repeat(32),
        manifest = "11".repeat(32),
        release = "55".repeat(32),
        commit = "44".repeat(20),
        value_set = "22".repeat(32),
    )
}

/// Renders the canonical-key-order payload with `manifest` in the manifest
/// slot, `commit` in the source-commit slot, and `filler` as every other hex
/// wire value, so a single wire mutation stays isolated to one field.
fn mixed_wire_payload(manifest: &str, filler: &str, commit: &str) -> String {
    format!(
        concat!(
            r#"{{"chain_id":8453"#,
            r#","deployment_evidence_sha256":"{filler}""#,
            r#","deployment_review_digest":"{filler}""#,
            r#","deployment_review_sha256":"{filler}""#,
            r#","manifest_sha256":"{manifest}""#,
            r#","release_artifact_sha256":"{filler}""#,
            r#","source_commit":"{commit}""#,
            r#","value_set_sha256":"{filler}"}}"#,
        ),
        manifest = manifest,
        filler = filler,
        commit = commit,
    )
}

#[test]
fn framing_matches_the_independent_vectors_at_the_small_payload_boundaries() {
    let digest =
        |payload: &[u8]| DomainSeparatedSha256::digest(B5_DEPLOYMENT_REVIEW_DOMAIN, payload);
    assert_eq!(digest(b""), Ok(REVIEW_EMPTY_PAYLOAD));
    // A NUL is forbidden in the domain but is ordinary payload data.
    assert_eq!(digest(b"\x00"), Ok(REVIEW_SINGLE_NUL_PAYLOAD));
    assert_eq!(digest(b"a"), Ok(REVIEW_PAYLOAD_A));
    // The length prefix separates a payload from its extensions.
    assert_eq!(digest(b"ab"), Ok(REVIEW_PAYLOAD_AB));
    assert_ne!(REVIEW_PAYLOAD_A, REVIEW_PAYLOAD_AB);
}

#[test]
fn n6_and_n7_over_the_same_exact_bytes_are_distinct_typed_digests() {
    let ordinary = B256::from_slice(Sha256::digest(SAMPLE_REVIEW_BYTES).as_slice());
    assert_eq!(ordinary, SAMPLE_N6_ORDINARY_SHA256);

    let framed = DomainSeparatedSha256::digest(B5_DEPLOYMENT_REVIEW_DOMAIN, SAMPLE_REVIEW_BYTES)
        .expect("the review domain and sample bytes must frame");
    assert_eq!(framed, SAMPLE_N7_REVIEW_FRAMED);

    // Identical input bytes, permanently distinct typed authorities.
    assert_ne!(ordinary, framed);
}

#[test]
fn domain_identity_and_ascii_case_select_the_digest() {
    let dormant =
        DomainSeparatedSha256::digest(B5_DORMANT_PROVISIONING_DOMAIN, SAMPLE_REVIEW_BYTES)
            .expect("the dormant domain and sample bytes must frame");
    assert_eq!(dormant, SAMPLE_DORMANT_DOMAIN);

    let uppercase_domain = B5_DEPLOYMENT_REVIEW_DOMAIN.to_ascii_uppercase();
    let uppercase = DomainSeparatedSha256::digest(&uppercase_domain, SAMPLE_REVIEW_BYTES)
        .expect("an uppercased ASCII domain still frames");
    assert_eq!(uppercase, SAMPLE_UPPERCASE_REVIEW_DOMAIN);

    let observed = [SAMPLE_N7_REVIEW_FRAMED, dormant, uppercase];
    let distinct: BTreeSet<B256> = observed.into_iter().collect();
    assert_eq!(distinct.len(), 3, "domain identity and case must be digest-separating");
}

#[test]
fn the_nul_separator_and_length_prefix_pin_the_domain_payload_boundary() {
    assert_eq!(DomainSeparatedSha256::digest("ab", b"c"), Ok(SHIFTED_DOMAIN_AB_PAYLOAD_C));
    assert_eq!(DomainSeparatedSha256::digest("a", b"bc"), Ok(SHIFTED_DOMAIN_A_PAYLOAD_BC));
    assert_ne!(SHIFTED_DOMAIN_AB_PAYLOAD_C, SHIFTED_DOMAIN_A_PAYLOAD_BC);
}

#[test]
fn invalid_domains_fail_closed_before_hashing() {
    for domain in ["", "base-mev\0:b5", "base-mév:b5"] {
        assert_eq!(
            DomainSeparatedSha256::digest(domain, SAMPLE_REVIEW_BYTES),
            Err(DigestFramingError::InvalidDomain)
        );
    }
}

#[test]
fn the_sole_value_set_aggregate_matches_its_vector() {
    let payload: &[u8] = br#"{"chain_id":8453,"coordination_lock":{},"r9":{},"suppression_hw":{},"suppression_json":{}}"#;
    assert_eq!(
        DomainSeparatedSha256::digest(B5_PROVISIONING_VALUE_SET_DOMAIN, payload),
        Ok(VALUE_SET_SAMPLE_N8)
    );
}

#[test]
fn snapshot_n11_matches_its_independent_vector_and_is_domain_framed() {
    let observed = base_snapshot();
    assert_eq!(observed.snapshot_digest(), BASE_N11);
    // Domain framing, not a bare hash of the JCS payload.
    assert_ne!(observed.snapshot_digest(), BASE_PAYLOAD_UNFRAMED_SHA256);

    // The eight scalars stay separately retained and unmerged.
    assert_eq!(observed.chain_id(), 8453);
    assert_eq!(observed.manifest_sha256(), MANIFEST);
    assert_eq!(observed.value_set_sha256(), VALUE_SET);
    assert_eq!(observed.deployment_evidence_sha256(), EVIDENCE);
    assert_eq!(observed.source_commit(), COMMIT);
    assert_eq!(observed.release_artifact_sha256(), RELEASE);
    assert_eq!(observed.deployment_review_file_sha256(), REVIEW_FILE);
    assert_eq!(observed.deployment_review_digest(), REVIEW_DIGEST);

    // Deterministic: rebuilding from the same scalars is value-equal.
    assert_eq!(base_snapshot(), observed);
}

#[test]
fn only_chain_8453_builds_a_snapshot() {
    for chain_id in [0, 1, 8452, 8454, u64::MAX] {
        assert_eq!(
            AuthenticatedProvisioningSnapshot::from_authenticated_bindings(
                chain_id,
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                COMMIT,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            Err(ProvisioningSnapshotError::UnsupportedChainId)
        );
    }
    assert_eq!(base_snapshot().chain_id(), 8453);
}

#[test]
fn swapping_the_typed_n6_and_n7_arguments_changes_n11() {
    let swapped =
        snapshot(MANIFEST, VALUE_SET, EVIDENCE, COMMIT, RELEASE, REVIEW_DIGEST, REVIEW_FILE);
    assert_eq!(swapped.snapshot_digest(), SWAPPED_N6_N7_N11);
    assert_ne!(swapped.snapshot_digest(), BASE_N11);
}

#[test]
fn extreme_bit_patterns_are_structurally_admissible() {
    let zero = B256::ZERO;
    let all_zero = snapshot(zero, zero, zero, [0x00; 20], zero, zero, zero);
    assert_eq!(all_zero.snapshot_digest(), ALL_ZERO_N11);

    let ones = B256::repeat_byte(0xff);
    let all_ones = snapshot(ones, ones, ones, [0xff; 20], ones, ones, ones);
    assert_eq!(all_ones.snapshot_digest(), ALL_ONES_N11);

    assert_ne!(all_zero.snapshot_digest(), all_ones.snapshot_digest());
}

#[test]
fn every_scalar_mutation_moves_n11_to_its_own_vector() {
    let mut mutated_commit = COMMIT;
    mutated_commit[0] ^= 0x01;
    let cases = [
        (
            snapshot(
                flip_low_bit(MANIFEST),
                VALUE_SET,
                EVIDENCE,
                COMMIT,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            N11_MANIFEST_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                flip_low_bit(VALUE_SET),
                EVIDENCE,
                COMMIT,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            N11_VALUE_SET_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                VALUE_SET,
                flip_low_bit(EVIDENCE),
                COMMIT,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            N11_EVIDENCE_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                mutated_commit,
                RELEASE,
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            N11_COMMIT_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                COMMIT,
                flip_low_bit(RELEASE),
                REVIEW_FILE,
                REVIEW_DIGEST,
            ),
            N11_RELEASE_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                COMMIT,
                RELEASE,
                flip_low_bit(REVIEW_FILE),
                REVIEW_DIGEST,
            ),
            N11_REVIEW_FILE_MUTATION,
        ),
        (
            snapshot(
                MANIFEST,
                VALUE_SET,
                EVIDENCE,
                COMMIT,
                RELEASE,
                REVIEW_FILE,
                flip_low_bit(REVIEW_DIGEST),
            ),
            N11_REVIEW_DIGEST_MUTATION,
        ),
    ];

    let mut observed = BTreeSet::from([BASE_N11]);
    for (mutated, expected) in cases {
        assert_eq!(mutated.snapshot_digest(), expected);
        observed.insert(mutated.snapshot_digest());
    }
    assert_eq!(observed.len(), 8, "the base digest and all seven mutations must be distinct");
}

#[test]
fn n11_binds_the_exact_sorted_jcs_bytes_and_rejects_reordered_keys() {
    // Positive seal: the independently reconstructed canonical byte sequence
    // — sorted keys, no whitespace, bare chain-id number, fixed-width
    // lowercase hex — frames to both the independent vector and the live
    // builder digest, so the builder payload is byte-exactly this JCS form.
    let canonical = base_canonical_payload();
    let framed = frame_dormant(&canonical);
    assert_eq!(framed, BASE_N11);
    assert_eq!(framed, base_snapshot().snapshot_digest());

    // Noncanonical mutation: the same eight members with one adjacent key
    // pair out of RFC 8785 sorted order ("…review_sha256" serialized before
    // "…review_digest") must never reproduce N11.
    let reordered = format!(
        concat!(
            r#"{{"chain_id":8453"#,
            r#","deployment_evidence_sha256":"{evidence}""#,
            r#","deployment_review_sha256":"{review_file}""#,
            r#","deployment_review_digest":"{review_digest}""#,
            r#","manifest_sha256":"{manifest}""#,
            r#","release_artifact_sha256":"{release}""#,
            r#","source_commit":"{commit}""#,
            r#","value_set_sha256":"{value_set}"}}"#,
        ),
        evidence = "33".repeat(32),
        review_digest = "77".repeat(32),
        review_file = "66".repeat(32),
        manifest = "11".repeat(32),
        release = "55".repeat(32),
        commit = "44".repeat(20),
        value_set = "22".repeat(32),
    );
    assert_eq!(reordered.len(), canonical.len(), "the mutation must reorder, not resize");
    assert_ne!(frame_dormant(&reordered), BASE_N11);
}

#[test]
fn wire_hex_is_fixed_width_lowercase() {
    // Letter-bearing and leading-zero patterns make case and width mutations
    // observable on the wire ("0a…", "ab…", "cd…").
    let letters = B256::repeat_byte(0xab);
    let zero_led = B256::repeat_byte(0x0a);
    let observed = snapshot(zero_led, letters, letters, [0xcd; 20], letters, letters, letters);

    // Positive seal: the fixed-width lowercase reconstruction is the exact
    // wire the builder hashed.
    let lowercase = mixed_wire_payload(&"0a".repeat(32), &"ab".repeat(32), &"cd".repeat(20));
    assert_eq!(frame_dormant(&lowercase), observed.snapshot_digest());

    // Case mutation: uppercased hex values under identical keys and order.
    let uppercase = mixed_wire_payload(&"0A".repeat(32), &"AB".repeat(32), &"CD".repeat(20));
    assert_ne!(frame_dormant(&uppercase), observed.snapshot_digest());

    // Width mutation: the manifest wire with its leading zero stripped
    // (63 hex chars instead of the fixed 64).
    let full_manifest = "0a".repeat(32);
    let stripped = mixed_wire_payload(&full_manifest[1..], &"ab".repeat(32), &"cd".repeat(20));
    assert_ne!(frame_dormant(&stripped), observed.snapshot_digest());
}
