//! Top-level TDX verifier input types.

use alloy_primitives::{B256, Bytes};

use crate::{TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence};

/// Complete explicit input to the pure TDX verifier.
#[derive(Clone, Debug)]
pub struct TdxVerifierInput {
    /// Raw Intel TDX quote bytes.
    pub quote: Bytes,
    /// Root-to-leaf PCK certificate chain for the quote attestation key.
    pub pck_certificate_chain: Vec<TdxCertificate>,
    /// TCB info collateral and QE identity collateral.
    pub collateral: TdxCollateral,
    /// CRLs or equivalent revocation evidence.
    pub revocation: TdxRevocationEvidence,
    /// Trusted Intel root CA hash expected by the on-chain verifier.
    pub trusted_root_ca_hash: B256,
    /// Expected uncompressed secp256k1 signer public key: `0x04 || x || y`.
    pub expected_public_key: Bytes,
    /// Quote collection timestamp in milliseconds since Unix epoch.
    ///
    /// This value must match the timestamp commitment in `TDREPORT.REPORTDATA`.
    pub quote_timestamp_millis: u64,
    /// Verification time in seconds since Unix epoch.
    pub verification_time: u64,
    /// Maximum accepted quote age in seconds.
    pub max_quote_age_seconds: u64,
    /// Contract TCB statuses accepted by verifier policy.
    pub allowed_tcb_statuses: Vec<TDXTcbStatus>,
}
