//! Top-level TDX verifier input types.

use std::fmt;

use alloy_primitives::{Address, B256, Bytes};

use crate::{TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence, TdxTcbStatusList};

/// Complete explicit input to the pure TDX verifier.
#[derive(Clone)]
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
    /// Expected Ethereum signer address derived from `expected_public_key`.
    pub expected_signer: Address,
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

impl fmt::Debug for TdxVerifierInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxVerifierInput")
            .field("quote_len", &self.quote.len())
            .field("pck_certificate_chain", &self.pck_certificate_chain)
            .field("collateral", &self.collateral)
            .field("revocation", &self.revocation)
            .field("trusted_root_ca_hash", &self.trusted_root_ca_hash)
            .field("expected_public_key", &self.expected_public_key)
            .field("expected_signer", &self.expected_signer)
            .field("quote_timestamp_millis", &self.quote_timestamp_millis)
            .field("verification_time", &self.verification_time)
            .field("max_quote_age_seconds", &self.max_quote_age_seconds)
            .field("allowed_tcb_statuses", &TdxTcbStatusList(&self.allowed_tcb_statuses))
            .finish()
    }
}
