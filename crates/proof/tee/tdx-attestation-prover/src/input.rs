//! ABI-compatible host and guest input encoding for TDX attestation proving.

use alloy_primitives::Address;
use alloy_sol_types::{SolValue, sol};
use base_proof_tee_tdx_verifier::{
    TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence, TdxSignedCollateral,
    TdxVerifier, TdxVerifierInput,
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
    }

    /// ABI mirror of `TdxCollateral`.
    struct TdxCollateralInput {
        /// TCB info collateral and signing chain.
        TdxSignedCollateralInput tcbInfo;
        /// QE identity collateral and signing chain.
        TdxSignedCollateralInput qeIdentity;
    }

    /// ABI mirror of `TdxRevocationEvidence`.
    struct TdxRevocationEvidenceInput {
        /// DER X.509 CRLs for all non-root certificate issuers.
        bytes[] certificateCrls;
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
        /// Quote collection timestamp in milliseconds since Unix epoch.
        uint64 quoteTimestampMillis;
        /// Verification time in seconds since Unix epoch.
        uint64 verificationTime;
        /// Maximum accepted quote age in seconds.
        uint64 maxQuoteAgeSeconds;
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
    pub fn expected_signer(&self) -> Result<Address> {
        let hash = TdxVerifier::validate_public_key(&self.verifier_input.expected_public_key)?;
        Ok(Address::from_slice(&hash.as_slice()[12..]))
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
        let actual = input.expected_signer()?;
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
            quoteTimestampMillis: input.quote_timestamp_millis,
            verificationTime: input.verification_time,
            maxQuoteAgeSeconds: input.max_quote_age_seconds,
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
            quote_timestamp_millis: input.quoteTimestampMillis,
            verification_time: input.verificationTime,
            max_quote_age_seconds: input.maxQuoteAgeSeconds,
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
        }
    }
}

impl From<TdxSignedCollateralInput> for TdxSignedCollateral {
    fn from(collateral: TdxSignedCollateralInput) -> Self {
        Self {
            raw: collateral.raw,
            signing_chain: collateral.signingChain.into_iter().map(TdxCertificate::from).collect(),
            signature: collateral.signature,
        }
    }
}

impl From<&TdxCollateral> for TdxCollateralInput {
    fn from(collateral: &TdxCollateral) -> Self {
        Self {
            tcbInfo: (&collateral.tcb_info).into(),
            qeIdentity: (&collateral.qe_identity).into(),
        }
    }
}

impl TryFrom<TdxCollateralInput> for TdxCollateral {
    type Error = ProverError;

    fn try_from(collateral: TdxCollateralInput) -> Result<Self> {
        Ok(Self { tcb_info: collateral.tcbInfo.into(), qe_identity: collateral.qeIdentity.into() })
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

/// Converts a contract TDX TCB status discriminant into a typed status.
pub fn tdx_tcb_status_from_u8(status: u8) -> Result<TDXTcbStatus> {
    TDXTcbStatus::try_from(status).map_err(|e| ProverError::InputDecode(e.to_string()))
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
}
