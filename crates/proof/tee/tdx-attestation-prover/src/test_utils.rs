//! Shared test fixtures for the `base-proof-tee-tdx-attestation-prover` crate.

use alloy_primitives::{Address, B256, Bytes};
use base_proof_tee_tdx_verifier::{
    IntelTcbStatus, TDXTcbStatus, TdxCertificate, TdxCollateral, TdxQuotePolicy,
    TdxRevocationEvidence, TdxSignedCollateral, TdxVerifierInput,
};

/// Signer address used across test fixtures.
pub const SIGNER: Address = Address::repeat_byte(0x44);

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
        issue_time: 1_700_000_000,
        next_update: 1_800_000_000,
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
            tcb_status: IntelTcbStatus::UpToDate,
        },
        revocation: TdxRevocationEvidence { certificate_crls: vec![Bytes::from_static(b"crl")] },
        trusted_root_ca_hash: B256::repeat_byte(0x55),
        expected_public_key: Bytes::from(vec![0x04; 65]),
        expected_signer: SIGNER,
        quote_timestamp_millis: 1_711_111_111_000,
        verification_time: 1_711_111_222,
        policy: TdxQuotePolicy { max_quote_age_seconds: 300 },
        allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate, TDXTcbStatus::SwHardeningNeeded],
    }
}
