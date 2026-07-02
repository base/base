//! ABI-compatible host and guest input encoding for TDX verification.

use alloy_primitives::Address;
use alloy_sol_types::{SolValue, sol};

use crate::{
    Result, TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence,
    TdxSignedCollateral, TdxVerifier, TdxVerifierError, TdxVerifierInput,
};

sol! {
    /// ABI mirror of `TdxSignedCollateral`.
    struct TdxSignedCollateralInput {
        /// Raw collateral document bytes.
        bytes raw;
        /// Root-to-leaf signing certificate chain.
        bytes[] signingChain;
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

    /// Complete explicit TDX verifier input encoded for a RISC Zero guest.
    struct TdxVerifierInputAbi {
        /// Raw Intel TDX quote bytes.
        bytes quote;
        /// Root-to-leaf PCK certificate chain.
        bytes[] pckCertificateChain;
        /// TCB info and QE identity collateral.
        TdxCollateralInput collateral;
        /// DER X.509 CRLs for all non-root certificate issuers.
        bytes[] certificateCrls;
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

impl TdxVerifierInput {
    /// ABI-encodes this input for host-to-guest transport.
    pub fn encode(&self) -> Vec<u8> {
        SolValue::abi_encode(&self.to_abi_input())
    }

    /// ABI-decodes a host-to-guest TDX verifier input.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let abi = <TdxVerifierInputAbi as SolValue>::abi_decode_validate(buf)
            .map_err(|e| TdxVerifierError::InputDecode(e.to_string()))?;
        Self::try_from_abi_input(abi)
    }

    /// ABI-decodes a verifier input and verifies it targets `signer_address`.
    pub fn decode_for_signer(buf: &[u8], signer_address: Address) -> Result<Self> {
        let input = Self::decode(buf)?;
        let hash = TdxVerifier::validate_public_key(&input.expected_public_key)?;
        let actual = Address::from_slice(&hash.as_slice()[12..]);
        if actual != signer_address {
            return Err(TdxVerifierError::SignerMismatch { expected: signer_address, actual });
        }
        Ok(input)
    }

    fn to_abi_input(&self) -> TdxVerifierInputAbi {
        let signed_collateral = |collateral: &TdxSignedCollateral| TdxSignedCollateralInput {
            raw: collateral.raw.clone(),
            signingChain: collateral
                .signing_chain
                .iter()
                .map(|certificate| certificate.raw.clone())
                .collect(),
            signature: collateral.signature.clone(),
        };

        TdxVerifierInputAbi {
            quote: self.quote.clone(),
            pckCertificateChain: self
                .pck_certificate_chain
                .iter()
                .map(|certificate| certificate.raw.clone())
                .collect(),
            collateral: TdxCollateralInput {
                tcbInfo: signed_collateral(&self.collateral.tcb_info),
                qeIdentity: signed_collateral(&self.collateral.qe_identity),
            },
            certificateCrls: self.revocation.certificate_crls.clone(),
            trustedRootCaHash: self.trusted_root_ca_hash,
            expectedPublicKey: self.expected_public_key.clone(),
            quoteTimestampMillis: self.quote_timestamp_millis,
            verificationTime: self.verification_time,
            maxQuoteAgeSeconds: self.max_quote_age_seconds,
            allowedTcbStatuses: self
                .allowed_tcb_statuses
                .iter()
                .map(|status| *status as u8)
                .collect(),
        }
    }

    fn try_from_abi_input(input: TdxVerifierInputAbi) -> Result<Self> {
        let signed_collateral = |collateral: TdxSignedCollateralInput| TdxSignedCollateral {
            raw: collateral.raw,
            signing_chain: collateral
                .signingChain
                .into_iter()
                .map(|raw| TdxCertificate { raw })
                .collect(),
            signature: collateral.signature,
        };
        let collateral = input.collateral;

        Ok(Self {
            quote: input.quote,
            pck_certificate_chain: input
                .pckCertificateChain
                .into_iter()
                .map(|raw| TdxCertificate { raw })
                .collect(),
            collateral: TdxCollateral {
                tcb_info: signed_collateral(collateral.tcbInfo),
                qe_identity: signed_collateral(collateral.qeIdentity),
            },
            revocation: TdxRevocationEvidence { certificate_crls: input.certificateCrls },
            trusted_root_ca_hash: input.trustedRootCaHash,
            expected_public_key: input.expectedPublicKey,
            quote_timestamp_millis: input.quoteTimestampMillis,
            verification_time: input.verificationTime,
            max_quote_age_seconds: input.maxQuoteAgeSeconds,
            allowed_tcb_statuses: input
                .allowedTcbStatuses
                .into_iter()
                .map(|status| {
                    TDXTcbStatus::try_from(status)
                        .map_err(|e| TdxVerifierError::InputDecode(e.to_string()))
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes};
    use k256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};

    use super::*;

    fn certificate(byte: u8) -> TdxCertificate {
        TdxCertificate { raw: Bytes::from(vec![byte; 3]) }
    }

    fn signed_collateral(byte: u8) -> TdxSignedCollateral {
        TdxSignedCollateral {
            raw: Bytes::from(vec![byte; 5]),
            signing_chain: vec![certificate(byte)],
            signature: Bytes::from(vec![byte; 64]),
        }
    }

    fn public_key() -> Bytes {
        let secret_key = SecretKey::from_slice(&[1; 32]).expect("fixture key must be valid");
        Bytes::copy_from_slice(secret_key.public_key().to_encoded_point(false).as_bytes())
    }

    fn verifier_input() -> TdxVerifierInput {
        TdxVerifierInput {
            quote: Bytes::from_static(b"quote"),
            pck_certificate_chain: vec![certificate(0x11), certificate(0x22)],
            collateral: TdxCollateral {
                tcb_info: signed_collateral(0x33),
                qe_identity: signed_collateral(0x44),
            },
            revocation: TdxRevocationEvidence {
                certificate_crls: vec![Bytes::from_static(b"crl")],
            },
            trusted_root_ca_hash: B256::repeat_byte(0x55),
            expected_public_key: public_key(),
            quote_timestamp_millis: 1_711_111_111_000,
            verification_time: 1_711_111_222,
            max_quote_age_seconds: 300,
            allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate, TDXTcbStatus::SwHardeningNeeded],
        }
    }

    #[test]
    fn verifier_input_abi_round_trips() {
        let input = verifier_input();
        let encoded = input.encode();
        let decoded = TdxVerifierInput::decode(&encoded).unwrap();

        assert_eq!(decoded.encode(), encoded);
    }

    #[test]
    fn decode_rejects_invalid_status() {
        let mut abi = verifier_input().to_abi_input();
        abi.allowedTcbStatuses = vec![200];
        let encoded = SolValue::abi_encode(&abi);

        assert!(matches!(
            TdxVerifierInput::decode(&encoded),
            Err(TdxVerifierError::InputDecode(_))
        ));
    }

    #[test]
    fn decode_for_signer_rejects_mismatched_signer() {
        let encoded = verifier_input().encode();

        assert!(matches!(
            TdxVerifierInput::decode_for_signer(&encoded, Address::repeat_byte(0x99)),
            Err(TdxVerifierError::SignerMismatch { .. })
        ));
    }
}
