//! Shared test fixtures for the `base-proof-tee-tdx-attestation-prover` crate.

use alloy_primitives::{B256, Bytes};
use base_proof_tee_tdx_verifier::{
    TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence, TdxSignedCollateral,
    TdxVerifierInput,
};

/// Valid uncompressed secp256k1 public key used across test fixtures.
pub const VALID_SECP256K1_PUBLIC_KEY: [u8; 65] = [
    0x04, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
    0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8, 0x17,
    0x98, 0x48, 0x3a, 0xda, 0x77, 0x26, 0xa3, 0xc4, 0x65, 0x5d, 0xa4, 0xfb, 0xfc, 0x0e, 0x11, 0x08,
    0xa8, 0xfd, 0x17, 0xb4, 0x48, 0xa6, 0x85, 0x54, 0x19, 0x9c, 0x47, 0xd0, 0x8f, 0xfb, 0x10, 0xd4,
    0xb8,
];

/// Builds a minimal [`TdxCertificate`] filled with repeated `byte` values.
pub fn certificate(byte: u8) -> TdxCertificate {
    TdxCertificate { raw: Bytes::from(vec![byte; 3]) }
}

/// Builds a minimal [`TdxSignedCollateral`] filled with repeated `byte` values.
pub fn signed_collateral(byte: u8) -> TdxSignedCollateral {
    TdxSignedCollateral {
        raw: Bytes::from(vec![byte; 5]),
        signing_chain: vec![certificate(byte)],
        signature: Bytes::from(vec![byte; 64]),
    }
}

/// Builds a complete [`TdxVerifierInput`] with fixed test data.
pub fn verifier_input() -> TdxVerifierInput {
    TdxVerifierInput {
        quote: Bytes::from_static(b"quote"),
        pck_certificate_chain: vec![certificate(0x11), certificate(0x22)],
        collateral: TdxCollateral {
            tcb_info: signed_collateral(0x33),
            qe_identity: signed_collateral(0x44),
        },
        revocation: TdxRevocationEvidence { certificate_crls: vec![Bytes::from_static(b"crl")] },
        trusted_root_ca_hash: B256::repeat_byte(0x55),
        expected_public_key: Bytes::from(VALID_SECP256K1_PUBLIC_KEY.to_vec()),
        quote_timestamp_millis: 1_711_111_111_000,
        verification_time: 1_711_111_222,
        max_quote_age_seconds: 300,
        allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate, TDXTcbStatus::SwHardeningNeeded],
    }
}
