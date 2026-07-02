//! ABI-compatible host and guest input encoding for TDX verification.

use alloy_primitives::Address;
use alloy_sol_types::{SolValue, sol};

use crate::{
    Result, TDXTcbStatus, TdxCertificate, TdxCollateral, TdxRevocationEvidence,
    TdxSignedCollateral, TdxVerifier, TdxVerifierError, TdxVerifierInput,
};

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

impl TdxVerifierInput {
    /// ABI-encodes this input for host-to-guest transport.
    pub fn encode(&self) -> Vec<u8> {
        SolValue::abi_encode(&self.to_abi())
    }

    /// ABI-decodes a host-to-guest TDX verifier input.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let abi = <TdxVerifierInputAbi as SolValue>::abi_decode_validate(buf)
            .map_err(|e| TdxVerifierError::InputDecode(e.to_string()))?;
        Self::try_from_abi(abi)
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

    /// Converts this verifier input to its ABI mirror.
    pub fn to_abi(&self) -> TdxVerifierInputAbi {
        let signed_collateral = |collateral: &TdxSignedCollateral| TdxSignedCollateralInput {
            raw: collateral.raw.clone(),
            signingChain: collateral
                .signing_chain
                .iter()
                .map(|certificate| TdxCertificateInput { raw: certificate.raw.clone() })
                .collect(),
            signature: collateral.signature.clone(),
        };

        TdxVerifierInputAbi {
            quote: self.quote.clone(),
            pckCertificateChain: self
                .pck_certificate_chain
                .iter()
                .map(|certificate| TdxCertificateInput { raw: certificate.raw.clone() })
                .collect(),
            collateral: TdxCollateralInput {
                tcbInfo: signed_collateral(&self.collateral.tcb_info),
                qeIdentity: signed_collateral(&self.collateral.qe_identity),
            },
            revocation: TdxRevocationEvidenceInput {
                certificateCrls: self.revocation.certificate_crls.clone(),
            },
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

    /// Converts an ABI mirror into verifier input.
    pub fn try_from_abi(input: TdxVerifierInputAbi) -> Result<Self> {
        let signed_collateral = |collateral: TdxSignedCollateralInput| TdxSignedCollateral {
            raw: collateral.raw,
            signing_chain: collateral
                .signingChain
                .into_iter()
                .map(|certificate| TdxCertificate { raw: certificate.raw })
                .collect(),
            signature: collateral.signature,
        };
        let collateral = input.collateral;

        Ok(Self {
            quote: input.quote,
            pck_certificate_chain: input
                .pckCertificateChain
                .into_iter()
                .map(|certificate| TdxCertificate { raw: certificate.raw })
                .collect(),
            collateral: TdxCollateral {
                tcb_info: signed_collateral(collateral.tcbInfo),
                qe_identity: signed_collateral(collateral.qeIdentity),
            },
            revocation: TdxRevocationEvidence {
                certificate_crls: input.revocation.certificateCrls,
            },
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

    use super::*;

    const VALID_SECP256K1_PUBLIC_KEY: [u8; 65] = [
        0x04, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98, 0x48, 0x3a, 0xda, 0x77, 0x26, 0xa3, 0xc4, 0x65, 0x5d, 0xa4, 0xfb, 0xfc,
        0x0e, 0x11, 0x08, 0xa8, 0xfd, 0x17, 0xb4, 0x48, 0xa6, 0x85, 0x54, 0x19, 0x9c, 0x47, 0xd0,
        0x8f, 0xfb, 0x10, 0xd4, 0xb8,
    ];

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
            expected_public_key: Bytes::from(VALID_SECP256K1_PUBLIC_KEY.to_vec()),
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
        let mut abi = verifier_input().to_abi();
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
