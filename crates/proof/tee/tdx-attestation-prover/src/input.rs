//! ABI-compatible host and guest input encoding for TDX attestation proving.

use alloy_primitives::Address;
use alloy_sol_types::{SolValue, sol};
use base_proof_tee_tdx_verifier::{
    IntelTcbStatus, TDXTcbStatus, TdxCertificate, TdxCollateral, TdxQuotePolicy,
    TdxRevocationEvidence, TdxSignedCollateral, TdxVerifierInput,
};

use crate::{ProverError, Result};

sol! {
    /// ABI mirror of `TdxCertificate` for deterministic host/guest input encoding.
    struct TdxCertificateInput {
        /// Raw DER certificate bytes.
        bytes raw;
    }

    /// ABI mirror of `TdxSignedCollateral`.
    struct TdxSignedCollateralInput {
        /// Raw collateral document bytes.
        bytes raw;
        /// Root-to-leaf signing certificate chain.
        TdxCertificateInput[] signingChain;
        /// P-256 ECDSA signature over the signed collateral body.
        bytes signature;
        /// Collateral issue time in seconds since Unix epoch.
        uint64 issueTime;
        /// Collateral expiration time in seconds since Unix epoch.
        uint64 nextUpdate;
    }

    /// ABI mirror of `TdxCollateral`.
    struct TdxCollateralInput {
        /// TCB info collateral and signing chain.
        TdxSignedCollateralInput tcbInfo;
        /// QE identity collateral and signing chain.
        TdxSignedCollateralInput qeIdentity;
        /// Intel TCB status hint retained for lossless host-side round trips.
        uint8 tcbStatus;
    }

    /// ABI mirror of `TdxRevocationEvidence`.
    struct TdxRevocationEvidenceInput {
        /// DER X.509 CRLs for all non-root certificate issuers.
        bytes[] certificateCrls;
    }

    /// ABI mirror of `TdxQuotePolicy`.
    struct TdxQuotePolicyInput {
        /// Maximum accepted quote age in seconds.
        uint64 maxQuoteAgeSeconds;
    }

    /// Complete explicit TDX verifier input encoded for a RISC Zero guest.
    struct TdxVerifierInputAbi {
        /// Raw Intel TDX quote bytes.
        bytes quote;
        /// Root-to-leaf PCK certificate chain.
        TdxCertificateInput[] pckCertificateChain;
        /// TCB info and QE identity collateral.
        TdxCollateralInput collateral;
        /// Certificate revocation evidence.
        TdxRevocationEvidenceInput revocation;
        /// Trusted Intel root CA hash.
        bytes32 trustedRootCaHash;
        /// Expected uncompressed secp256k1 signer public key.
        bytes expectedPublicKey;
        /// Expected Ethereum signer address.
        address expectedSigner;
        /// Quote collection timestamp in milliseconds since Unix epoch.
        uint64 quoteTimestampMillis;
        /// Verification time in seconds since Unix epoch.
        uint64 verificationTime;
        /// Quote timestamp policy.
        TdxQuotePolicyInput policy;
        /// Contract TCB statuses accepted by verifier policy.
        uint8[] allowedTcbStatuses;
    }
}

/// Explicit TDX attestation prover input.
#[derive(Debug)]
pub struct TdxAttestationProverInput {
    /// Complete input consumed by `base-proof-tee-tdx-verifier`.
    pub verifier_input: TdxVerifierInput,
}

impl TdxAttestationProverInput {
    /// Creates a prover input from a verifier input.
    pub const fn new(verifier_input: TdxVerifierInput) -> Self {
        Self { verifier_input }
    }

    /// Returns the signer committed by the verifier input.
    pub const fn expected_signer(&self) -> Address {
        self.verifier_input.expected_signer
    }

    /// Returns the quote timestamp committed by the verifier input.
    pub const fn quote_timestamp_millis(&self) -> u64 {
        self.verifier_input.quote_timestamp_millis
    }

    /// Returns a shared reference to the verifier input.
    pub const fn verifier_input(&self) -> &TdxVerifierInput {
        &self.verifier_input
    }

    /// ABI-encodes this input for host-to-guest transport.
    pub fn encode(&self) -> Vec<u8> {
        SolValue::abi_encode(&TdxVerifierInputAbi::from(&self.verifier_input))
    }

    /// ABI-decodes a host-to-guest TDX verifier input.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let abi = <TdxVerifierInputAbi as SolValue>::abi_decode_validate(buf)
            .map_err(|e| ProverError::InputDecode(e.to_string()))?;
        Ok(Self { verifier_input: TdxVerifierInput::try_from(abi)? })
    }

    /// ABI-decodes a prover input and verifies it targets `signer_address`.
    pub fn decode_for_signer(buf: &[u8], signer_address: Address) -> Result<Self> {
        let input = Self::decode(buf)?;
        let actual = input.expected_signer();
        if actual != signer_address {
            return Err(ProverError::SignerMismatch { expected: signer_address, actual });
        }
        Ok(input)
    }
}

impl From<&TdxVerifierInput> for TdxVerifierInputAbi {
    fn from(input: &TdxVerifierInput) -> Self {
        Self {
            quote: input.quote.clone(),
            pckCertificateChain: input.pck_certificate_chain.iter().map(Into::into).collect(),
            collateral: (&input.collateral).into(),
            revocation: (&input.revocation).into(),
            trustedRootCaHash: input.trusted_root_ca_hash,
            expectedPublicKey: input.expected_public_key.clone(),
            expectedSigner: input.expected_signer,
            quoteTimestampMillis: input.quote_timestamp_millis,
            verificationTime: input.verification_time,
            policy: (&input.policy).into(),
            allowedTcbStatuses: input
                .allowed_tcb_statuses
                .iter()
                .map(|status| *status as u8)
                .collect(),
        }
    }
}

impl TryFrom<TdxVerifierInputAbi> for TdxVerifierInput {
    type Error = ProverError;

    fn try_from(input: TdxVerifierInputAbi) -> Result<Self> {
        Ok(Self {
            quote: input.quote,
            pck_certificate_chain: input
                .pckCertificateChain
                .into_iter()
                .map(TdxCertificate::from)
                .collect(),
            collateral: TdxCollateral::try_from(input.collateral)?,
            revocation: TdxRevocationEvidence::from(input.revocation),
            trusted_root_ca_hash: input.trustedRootCaHash,
            expected_public_key: input.expectedPublicKey,
            expected_signer: input.expectedSigner,
            quote_timestamp_millis: input.quoteTimestampMillis,
            verification_time: input.verificationTime,
            policy: TdxQuotePolicy::from(input.policy),
            allowed_tcb_statuses: input
                .allowedTcbStatuses
                .into_iter()
                .map(tdx_tcb_status_from_u8)
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

impl From<&TdxCertificate> for TdxCertificateInput {
    fn from(certificate: &TdxCertificate) -> Self {
        Self { raw: certificate.raw.clone() }
    }
}

impl From<TdxCertificateInput> for TdxCertificate {
    fn from(certificate: TdxCertificateInput) -> Self {
        Self { raw: certificate.raw }
    }
}

impl From<&TdxSignedCollateral> for TdxSignedCollateralInput {
    fn from(collateral: &TdxSignedCollateral) -> Self {
        Self {
            raw: collateral.raw.clone(),
            signingChain: collateral.signing_chain.iter().map(Into::into).collect(),
            signature: collateral.signature.clone(),
            issueTime: collateral.issue_time,
            nextUpdate: collateral.next_update,
        }
    }
}

impl From<TdxSignedCollateralInput> for TdxSignedCollateral {
    fn from(collateral: TdxSignedCollateralInput) -> Self {
        Self {
            raw: collateral.raw,
            signing_chain: collateral.signingChain.into_iter().map(TdxCertificate::from).collect(),
            signature: collateral.signature,
            issue_time: collateral.issueTime,
            next_update: collateral.nextUpdate,
        }
    }
}

impl From<&TdxCollateral> for TdxCollateralInput {
    fn from(collateral: &TdxCollateral) -> Self {
        Self {
            tcbInfo: (&collateral.tcb_info).into(),
            qeIdentity: (&collateral.qe_identity).into(),
            tcbStatus: intel_tcb_status_to_u8(collateral.tcb_status),
        }
    }
}

impl TryFrom<TdxCollateralInput> for TdxCollateral {
    type Error = ProverError;

    fn try_from(collateral: TdxCollateralInput) -> Result<Self> {
        Ok(Self {
            tcb_info: collateral.tcbInfo.into(),
            qe_identity: collateral.qeIdentity.into(),
            tcb_status: intel_tcb_status_from_u8(collateral.tcbStatus)?,
        })
    }
}

impl From<&TdxRevocationEvidence> for TdxRevocationEvidenceInput {
    fn from(evidence: &TdxRevocationEvidence) -> Self {
        Self { certificateCrls: evidence.certificate_crls.clone() }
    }
}

impl From<TdxRevocationEvidenceInput> for TdxRevocationEvidence {
    fn from(evidence: TdxRevocationEvidenceInput) -> Self {
        Self { certificate_crls: evidence.certificateCrls }
    }
}

impl From<&TdxQuotePolicy> for TdxQuotePolicyInput {
    fn from(policy: &TdxQuotePolicy) -> Self {
        Self { maxQuoteAgeSeconds: policy.max_quote_age_seconds }
    }
}

impl From<TdxQuotePolicyInput> for TdxQuotePolicy {
    fn from(policy: TdxQuotePolicyInput) -> Self {
        Self { max_quote_age_seconds: policy.maxQuoteAgeSeconds }
    }
}

/// Converts a contract TDX TCB status discriminant into a typed status.
pub fn tdx_tcb_status_from_u8(status: u8) -> Result<TDXTcbStatus> {
    TDXTcbStatus::try_from(status).map_err(|e| ProverError::InputDecode(e.to_string()))
}

/// Converts an Intel TCB status into a stable input discriminant.
pub const fn intel_tcb_status_to_u8(status: IntelTcbStatus) -> u8 {
    match status {
        IntelTcbStatus::UpToDate => 1,
        IntelTcbStatus::SwHardeningNeeded => 2,
        IntelTcbStatus::ConfigurationNeeded => 3,
        IntelTcbStatus::ConfigurationAndSwHardeningNeeded => 4,
        IntelTcbStatus::OutOfDate => 5,
        IntelTcbStatus::OutOfDateConfigurationNeeded => 6,
        IntelTcbStatus::Revoked => 7,
        IntelTcbStatus::Unsupported => 255,
    }
}

/// Converts an input discriminant into an Intel TCB status.
pub fn intel_tcb_status_from_u8(status: u8) -> Result<IntelTcbStatus> {
    match status {
        1 => Ok(IntelTcbStatus::UpToDate),
        2 => Ok(IntelTcbStatus::SwHardeningNeeded),
        3 => Ok(IntelTcbStatus::ConfigurationNeeded),
        4 => Ok(IntelTcbStatus::ConfigurationAndSwHardeningNeeded),
        5 => Ok(IntelTcbStatus::OutOfDate),
        6 => Ok(IntelTcbStatus::OutOfDateConfigurationNeeded),
        7 => Ok(IntelTcbStatus::Revoked),
        255 => Ok(IntelTcbStatus::Unsupported),
        value => {
            Err(ProverError::InputDecode(format!("invalid Intel TCB status discriminant: {value}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::test_utils::verifier_input;

    #[rstest]
    fn prover_input_abi_round_trips() {
        let input = TdxAttestationProverInput::new(verifier_input());
        let encoded = input.encode();
        let decoded = TdxAttestationProverInput::decode(&encoded).unwrap();

        assert_eq!(decoded.encode(), encoded);
    }

    #[rstest]
    fn decode_rejects_invalid_status() {
        let mut abi = TdxVerifierInputAbi::from(&verifier_input());
        abi.allowedTcbStatuses = vec![200];
        let encoded = SolValue::abi_encode(&abi);

        assert!(matches!(
            TdxAttestationProverInput::decode(&encoded),
            Err(ProverError::InputDecode(_))
        ));
    }

    #[rstest]
    #[case(IntelTcbStatus::UpToDate, 1)]
    #[case(IntelTcbStatus::Revoked, 7)]
    #[case(IntelTcbStatus::Unsupported, 255)]
    fn intel_status_discriminants_round_trip(#[case] status: IntelTcbStatus, #[case] expected: u8) {
        assert_eq!(intel_tcb_status_to_u8(status), expected);
        assert_eq!(intel_tcb_status_from_u8(expected).unwrap(), status);
    }
}
