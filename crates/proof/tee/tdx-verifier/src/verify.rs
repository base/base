//! Confidential Space token verification and Solidity journal generation.

use alloy_primitives::{Address, B256, hex, keccak256};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use k256::PublicKey;
use rsa::{
    RsaPublicKey,
    pkcs1::DecodeRsaPublicKey,
    pkcs1v15::{Signature as RsaSignature, VerifyingKey as RsaVerifyingKey},
    signature::Verifier,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

use crate::{
    Result, TDXVerificationResult, TDXVerifierJournal, TdxVerifierError, TdxVerifierInput,
};

/// Google Cloud Attestation issuer for Confidential Space tokens.
const GOOGLE_CLOUD_ATTESTATION_ISSUER: &str = "https://confidentialcomputing.googleapis.com";

const TOKEN_NONCE_DOMAIN: &[u8] = b"base-tdx-confidential-space-token-nonce:v1";
const GCP_INTEL_TDX: &str = "GCP_INTEL_TDX";
const CONFIDENTIAL_SPACE: &str = "CONFIDENTIAL_SPACE";
const DEBUG_DISABLED_SINCE_BOOT: &str = "disabled-since-boot";
const STABLE_SUPPORT_ATTRIBUTE: &str = "STABLE";
const SHA256_WITH_RSA_OID: &str = "1.2.840.113549.1.1.11";

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    x5c: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    aud: String,
    dbgstat: String,
    #[serde(default)]
    eat_nonce: Vec<String>,
    exp: u64,
    hwmodel: String,
    iat: u64,
    iss: String,
    nbf: u64,
    secboot: bool,
    swname: String,
    submods: TokenSubmodules,
}

#[derive(Debug, Deserialize)]
struct TokenSubmodules {
    confidential_space: ConfidentialSpaceClaims,
    container: ContainerClaims,
}

#[derive(Debug, Deserialize)]
struct ConfidentialSpaceClaims {
    #[serde(default)]
    support_attributes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ContainerClaims {
    image_digest: String,
    #[serde(default)]
    cmd_override: Option<Value>,
    #[serde(default)]
    env_override: Option<Value>,
}

/// Stateless Confidential Space TDX attestation verifier.
#[derive(Debug)]
pub struct TdxVerifier;

impl TdxVerifier {
    /// Verifies a Google Cloud Attestation PKI token into an onchain journal.
    pub fn verify(input: &TdxVerifierInput) -> Result<TDXVerifierJournal> {
        let (header, claims, signing_input, signature) = Self::decode_token(&input.token)?;
        let leaf_key = Self::verify_token_certificates(
            &header.x5c,
            input.trusted_root_ca_hash,
            input.verification_time,
        )?;
        Self::verify_token_signature(&header, &leaf_key, &signing_input, &signature)?;
        Self::verify_claims(&claims, input)?;

        let public_key_hash = Self::validate_public_key(&input.expected_public_key)?;
        let signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        let image_hash = Self::parse_image_hash(&claims.submods.container.image_digest)?;
        let token_nonce = input.attestation_nonce.ok_or(TdxVerifierError::TokenNonceMismatch)?;
        let expected_token_nonce = Self::token_nonce(
            &input.expected_public_key,
            token_nonce,
            input.chain_id,
            input.registry_address,
        )?;
        let expected_token_nonce_text = hex::encode(expected_token_nonce);
        if !claims.eat_nonce.iter().any(|nonce| nonce == &expected_token_nonce_text) {
            return Err(TdxVerifierError::TokenNonceMismatch);
        }

        let leaf_certificate = header.x5c.first().expect("verified non-empty x5c chain");
        let leaf_certificate = STANDARD
            .decode(leaf_certificate)
            .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))?;

        Ok(TDXVerifierJournal {
            result: TDXVerificationResult::Success,
            issuedAt: claims.iat,
            expiration: claims.exp,
            rootCaHash: input.trusted_root_ca_hash,
            tokenLeafCertHash: keccak256(leaf_certificate),
            publicKey: input.expected_public_key.clone(),
            signer,
            imageHash: image_hash,
            audienceHash: keccak256(input.expected_audience.as_bytes()),
            tokenNonceHash: expected_token_nonce,
            hardwareModelHash: keccak256(claims.hwmodel.as_bytes()),
            secureBoot: claims.secboot,
            debugDisabled: claims.dbgstat == DEBUG_DISABLED_SINCE_BOOT,
            commandOverride: Self::has_override(claims.submods.container.cmd_override.as_ref()),
            environmentOverride: Self::has_override(claims.submods.container.env_override.as_ref()),
            chainId: input.chain_id,
            registryAddress: input.registry_address,
        })
    }

    /// Validates and hashes an uncompressed secp256k1 signer public key.
    pub fn validate_public_key(public_key: &[u8]) -> Result<B256> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxVerifierError::MalformedPublicKey);
        }
        PublicKey::from_sec1_bytes(public_key).map_err(|_| TdxVerifierError::MalformedPublicKey)?;
        Ok(keccak256(&public_key[1..]))
    }

    /// Derives the token nonce that binds a signer to one registrar challenge.
    pub fn token_nonce(
        public_key: &[u8],
        registrar_nonce: B256,
        chain_id: u64,
        registry_address: Address,
    ) -> Result<B256> {
        let public_key_hash = Self::validate_public_key(public_key)?;
        Ok(keccak256(
            [
                TOKEN_NONCE_DOMAIN,
                public_key_hash.as_slice(),
                registrar_nonce.as_slice(),
                &chain_id.to_le_bytes(),
                registry_address.as_slice(),
            ]
            .concat(),
        ))
    }

    /// Extracts the OCI image digest from a token without trusting its signature.
    ///
    /// The prover uses this only to construct proof journals. Signer registration
    /// validates the complete token before the digest can become trusted onchain.
    pub fn image_hash_from_token(token: &[u8]) -> Result<B256> {
        let (_, claims, _, _) = Self::decode_token(token)?;
        Self::parse_image_hash(&claims.submods.container.image_digest)
    }

    fn decode_token(token: &[u8]) -> Result<(JwtHeader, TokenClaims, Vec<u8>, Vec<u8>)> {
        let token = std::str::from_utf8(token)
            .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))?;
        let mut parts = token.split('.');
        let header_segment =
            parts.next().ok_or(TdxVerifierError::TokenMalformed("missing header".into()))?;
        let claims_segment =
            parts.next().ok_or(TdxVerifierError::TokenMalformed("missing claims".into()))?;
        let signature =
            parts.next().ok_or(TdxVerifierError::TokenMalformed("missing signature".into()))?;
        if parts.next().is_some() {
            return Err(TdxVerifierError::TokenMalformed("too many JWT segments".into()));
        }

        let header = serde_json::from_slice(&Self::decode_url_base64(header_segment)?)
            .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))?;
        let claims = serde_json::from_slice(&Self::decode_url_base64(claims_segment)?)
            .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))?;
        let signature = Self::decode_url_base64(signature)?;

        Ok((header, claims, format!("{header_segment}.{claims_segment}").into_bytes(), signature))
    }

    fn decode_url_base64(value: &str) -> Result<Vec<u8>> {
        URL_SAFE_NO_PAD
            .decode(value)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
            .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))
    }

    fn verify_token_certificates(
        certificates: &[String],
        trusted_root_ca_hash: B256,
        verification_time: u64,
    ) -> Result<RsaPublicKey> {
        if certificates.is_empty() {
            return Err(TdxVerifierError::TokenMalformed("token x5c chain is empty".into()));
        }
        let raw_certificates = certificates
            .iter()
            .map(|certificate| {
                STANDARD
                    .decode(certificate)
                    .map_err(|error| TdxVerifierError::TokenMalformed(error.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let root = raw_certificates.last().expect("checked non-empty certificate chain");
        if keccak256(root) != trusted_root_ca_hash {
            return Err(TdxVerifierError::RootCaNotTrusted);
        }

        let certificates = raw_certificates
            .iter()
            .map(|raw| {
                let (remaining, certificate) = X509Certificate::from_der(raw)
                    .map_err(|error| TdxVerifierError::TokenSignatureInvalid(error.to_string()))?;
                if !remaining.is_empty() {
                    return Err(TdxVerifierError::TokenSignatureInvalid(
                        "certificate has trailing bytes".into(),
                    ));
                }
                let not_before = u64::try_from(certificate.validity().not_before.timestamp())
                    .map_err(|_| {
                        TdxVerifierError::TokenSignatureInvalid("negative notBefore".into())
                    })?;
                let not_after = u64::try_from(certificate.validity().not_after.timestamp())
                    .map_err(|_| {
                        TdxVerifierError::TokenSignatureInvalid("negative notAfter".into())
                    })?;
                if verification_time < not_before || verification_time >= not_after {
                    return Err(TdxVerifierError::TokenExpired);
                }
                Ok(certificate)
            })
            .collect::<Result<Vec<_>>>()?;

        for pair in certificates.windows(2) {
            let [certificate, issuer] = pair else {
                unreachable!("windows(2) produces pairs");
            };
            if certificate.issuer() != issuer.subject() {
                return Err(TdxVerifierError::TokenSignatureInvalid(
                    "certificate issuer does not match parent subject".into(),
                ));
            }
            let basic_constraints = issuer
                .basic_constraints()
                .map_err(|error| TdxVerifierError::TokenSignatureInvalid(error.to_string()))?
                .ok_or_else(|| {
                    TdxVerifierError::TokenSignatureInvalid(
                        "token certificate issuer is not a certificate authority".into(),
                    )
                })?;
            if !basic_constraints.value.ca {
                return Err(TdxVerifierError::TokenSignatureInvalid(
                    "token certificate issuer is not a certificate authority".into(),
                ));
            }
            Self::verify_rsa_sha256_signature(
                issuer.public_key().subject_public_key.data.as_ref(),
                certificate.tbs_certificate.as_ref(),
                certificate.signature_value.data.as_ref(),
                certificate.signature_algorithm.algorithm.to_id_string().as_str(),
            )?;
        }

        Self::rsa_public_key(certificates[0].public_key().subject_public_key.data.as_ref())
    }

    fn verify_token_signature(
        header: &JwtHeader,
        key: &RsaPublicKey,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<()> {
        if header.alg != "RS256" {
            return Err(TdxVerifierError::TokenSignatureInvalid(format!(
                "unsupported JWT algorithm {}",
                header.alg
            )));
        }
        Self::verify_rsa_sha256_signature_from_key(key, signing_input, signature)
    }

    fn verify_rsa_sha256_signature(
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
        signature_algorithm: &str,
    ) -> Result<()> {
        if signature_algorithm != SHA256_WITH_RSA_OID {
            return Err(TdxVerifierError::TokenSignatureInvalid(format!(
                "unsupported certificate signature algorithm {signature_algorithm}"
            )));
        }
        let key = Self::rsa_public_key(public_key)?;
        Self::verify_rsa_sha256_signature_from_key(&key, message, signature)
    }

    fn verify_rsa_sha256_signature_from_key(
        key: &RsaPublicKey,
        message: &[u8],
        signature: &[u8],
    ) -> Result<()> {
        let signature = RsaSignature::try_from(signature)
            .map_err(|error| TdxVerifierError::TokenSignatureInvalid(error.to_string()))?;
        RsaVerifyingKey::<Sha256>::new(key.clone())
            .verify(message, &signature)
            .map_err(|error| TdxVerifierError::TokenSignatureInvalid(error.to_string()))
    }

    fn rsa_public_key(public_key: &[u8]) -> Result<RsaPublicKey> {
        RsaPublicKey::from_pkcs1_der(public_key)
            .map_err(|error| TdxVerifierError::TokenSignatureInvalid(error.to_string()))
    }

    fn verify_claims(claims: &TokenClaims, input: &TdxVerifierInput) -> Result<()> {
        if claims.iss != GOOGLE_CLOUD_ATTESTATION_ISSUER
            || claims.aud != input.expected_audience
            || claims.swname != CONFIDENTIAL_SPACE
            || claims.hwmodel != GCP_INTEL_TDX
            || claims.dbgstat != DEBUG_DISABLED_SINCE_BOOT
            || !claims.secboot
            || !claims
                .submods
                .confidential_space
                .support_attributes
                .iter()
                .any(|attribute| attribute == STABLE_SUPPORT_ATTRIBUTE)
            || Self::has_override(claims.submods.container.cmd_override.as_ref())
            || Self::has_override(claims.submods.container.env_override.as_ref())
        {
            return Err(TdxVerifierError::TokenClaimsInvalid);
        }
        if claims.nbf > input.verification_time
            || claims.iat > input.verification_time
            || claims.exp <= input.verification_time
            || input.verification_time.saturating_sub(claims.iat) >= input.max_token_age_seconds
        {
            return Err(TdxVerifierError::TokenExpired);
        }
        Ok(())
    }

    fn parse_image_hash(image_digest: &str) -> Result<B256> {
        let digest = image_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| TdxVerifierError::TokenClaimsInvalid)?;
        let digest = hex::decode(digest).map_err(|_| TdxVerifierError::TokenClaimsInvalid)?;
        if digest.len() != 32 {
            return Err(TdxVerifierError::TokenClaimsInvalid);
        }
        Ok(B256::from_slice(&digest))
    }

    fn has_override(value: Option<&Value>) -> bool {
        match value {
            None | Some(Value::Null) => false,
            Some(Value::Array(values)) => !values.is_empty(),
            Some(Value::Object(values)) => !values.is_empty(),
            Some(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use rsa::{
        RsaPrivateKey,
        pkcs1::DecodeRsaPrivateKey,
        pkcs1v15::SigningKey as RsaSigningKey,
        signature::{SignatureEncoding, Signer},
    };

    use super::*;

    const PUBLIC_KEY: [u8; 65] = [
        0x04, 0x1b, 0x84, 0xc5, 0x56, 0x7b, 0x12, 0x64, 0x40, 0x99, 0x5d, 0x3e, 0xd5, 0xaa, 0xba,
        0x05, 0x65, 0xd7, 0x1e, 0x18, 0x34, 0x60, 0x48, 0x19, 0xff, 0x9c, 0x17, 0xf5, 0xe9, 0xd5,
        0xdd, 0x07, 0x8f, 0x70, 0xbe, 0xaf, 0x8f, 0x58, 0x8b, 0x54, 0x15, 0x07, 0xfe, 0xd6, 0xa6,
        0x42, 0xc5, 0xab, 0x42, 0xdf, 0xdf, 0x81, 0x20, 0xa7, 0xf6, 0x39, 0xde, 0x51, 0x22, 0xd4,
        0x7a, 0x69, 0xa8, 0xe8, 0xd1,
    ];
    const TEST_CERTIFICATE_B64: &str = "MIIDOTCCAiGgAwIBAgIUNYDlLPEcP+YVn4A5ms895irtNtcwDQYJKoZIhvcNAQELBQAwLDEqMCgGA1UEAwwhQmFzZSBDb25maWRlbnRpYWwgU3BhY2UgVGVzdCBSb290MB4XDTI2MDcxNDIwMzIwMloXDTM2MDcxMTIwMzIwMlowLDEqMCgGA1UEAwwhQmFzZSBDb25maWRlbnRpYWwgU3BhY2UgVGVzdCBSb290MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAmHEtBJK4Crft6tWyI0QDgKJ+u88Cf+oItrJ26fhoqYFzhjDlL6wivw7gMhtKFvFwbZWZMqzX6dEiJOVqybItJDeNWWQ+BMPpTw0w73bt+89rlq7Xb2WuoCHEJtua8XzJ2eTjSNYL86jwb3qjo5SIsnA1bBhS2Adxa28At43p7oUjM++gxYF1IQnr8JHDu0QZM2QdXoKUkBNIAEGUM7d4LafkY/qT67CHCxN2fPy7cMAnvPx7t5z4IDCr97AVPiEY+nkmcbE8S10JL01HXnT9UPfL9lzoMmTWqtMEwMWpOzZHmplb0i1b3uTthQ+bQwEt8yD6OzJ9vyK+r0hzeBWAWQIDAQABo1MwUTAdBgNVHQ4EFgQUvQ5FwgXxyFLlR3xkcMXo3LeMAKEwHwYDVR0jBBgwFoAUvQ5FwgXxyFLlR3xkcMXo3LeMAKEwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEADEiMKpzgm/cnfWdC8g5seUGLuc8pUL1DWPUu6QAfWuBh92uZWt7/9vB8xTvT9eychwuZzEW8n4T1c/JXYRJ3gqThDeWKZELYgGPoK62ATCEL1nr6kdAGI7lNGU4yLruzNO8lIzXasIt55Vy2rr/nl2fHNjK8CU1GrmPybmL9nzJfUvInfFcOZttqwc+iRyR2ji07WehS1V8wBRDZ0UC8ZYBjiDWay6TRrrrsImTpdGHur1hMwKeB461xP5+HsuXDDzlKegJ/tdAQhicJGsriTpiglyiahtiGv75LsXedQGk8LuF7Ydf35PwbOBlPSM54a/oftoV42aG2ZfXOSapEWQ==";
    const TEST_ROOT_KEY: &str = include_str!("testdata/confidential_space_test_root.key.pem");
    const VERIFICATION_TIME: u64 = 1_800_000_000;

    #[test]
    fn token_nonce_binds_signer_and_registration_context() {
        let nonce = B256::repeat_byte(0x11);
        let context = (11_155_111, Address::repeat_byte(0x22));
        let expected = TdxVerifier::token_nonce(&PUBLIC_KEY, nonce, context.0, context.1).unwrap();

        assert_ne!(
            expected,
            TdxVerifier::token_nonce(&PUBLIC_KEY, B256::repeat_byte(0x12), context.0, context.1)
                .unwrap()
        );
        assert_ne!(
            expected,
            TdxVerifier::token_nonce(&PUBLIC_KEY, nonce, context.0 + 1, context.1).unwrap()
        );
        assert_ne!(
            expected,
            TdxVerifier::token_nonce(&PUBLIC_KEY, nonce, context.0, Address::repeat_byte(0x23))
                .unwrap()
        );
    }

    #[test]
    fn image_hash_requires_a_sha256_oci_digest() {
        assert_eq!(
            TdxVerifier::parse_image_hash(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .unwrap(),
            B256::repeat_byte(0xaa)
        );
        assert!(TdxVerifier::parse_image_hash("sha512:00").is_err());
    }

    #[test]
    fn verifies_signed_confidential_space_token_and_emits_solidity_journal() {
        let registrar_nonce = B256::repeat_byte(0x11);
        let registry_address = Address::repeat_byte(0x22);
        let token_nonce =
            TdxVerifier::token_nonce(&PUBLIC_KEY, registrar_nonce, 11_155_111, registry_address)
                .unwrap();
        let claims = serde_json::json!({
            "aud": "base-tdx-prover",
            "dbgstat": "disabled-since-boot",
            "eat_nonce": [hex::encode(token_nonce)],
            "exp": VERIFICATION_TIME + 60,
            "hwmodel": "GCP_INTEL_TDX",
            "iat": VERIFICATION_TIME - 1,
            "iss": GOOGLE_CLOUD_ATTESTATION_ISSUER,
            "nbf": VERIFICATION_TIME - 1,
            "secboot": true,
            "swname": "CONFIDENTIAL_SPACE",
            "submods": {
                "confidential_space": { "support_attributes": ["STABLE"] },
                "container": {
                    "image_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }
        });
        let token = signed_test_token(claims);
        let root = STANDARD.decode(TEST_CERTIFICATE_B64).unwrap();
        let journal = TdxVerifier::verify(&TdxVerifierInput {
            token: Bytes::from(token),
            trusted_root_ca_hash: keccak256(root),
            expected_audience: "base-tdx-prover".into(),
            expected_public_key: Bytes::from_static(&PUBLIC_KEY),
            attestation_nonce: Some(registrar_nonce),
            chain_id: 11_155_111,
            registry_address,
            verification_time: VERIFICATION_TIME,
            max_token_age_seconds: 300,
        })
        .unwrap();

        assert_eq!(journal.result, TDXVerificationResult::Success);
        assert_eq!(journal.imageHash, B256::repeat_byte(0xaa));
        assert_eq!(journal.tokenNonceHash, token_nonce);
        assert!(journal.secureBoot);
        assert!(journal.debugDisabled);
        assert!(!journal.commandOverride);
        assert!(!journal.environmentOverride);
    }

    #[test]
    fn rejects_non_production_confidential_space_token() {
        let registrar_nonce = B256::repeat_byte(0x11);
        let registry_address = Address::repeat_byte(0x22);
        let token_nonce =
            TdxVerifier::token_nonce(&PUBLIC_KEY, registrar_nonce, 11_155_111, registry_address)
                .unwrap();
        let claims = serde_json::json!({
            "aud": "base-tdx-prover",
            "dbgstat": "enabled",
            "eat_nonce": [hex::encode(token_nonce)],
            "exp": VERIFICATION_TIME + 60,
            "hwmodel": "GCP_INTEL_TDX",
            "iat": VERIFICATION_TIME - 1,
            "iss": GOOGLE_CLOUD_ATTESTATION_ISSUER,
            "nbf": VERIFICATION_TIME - 1,
            "secboot": true,
            "swname": "CONFIDENTIAL_SPACE",
            "submods": {
                "confidential_space": { "support_attributes": ["STABLE"] },
                "container": {
                    "image_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }
        });
        let root = STANDARD.decode(TEST_CERTIFICATE_B64).unwrap();

        assert!(matches!(
            TdxVerifier::verify(&TdxVerifierInput {
                token: Bytes::from(signed_test_token(claims)),
                trusted_root_ca_hash: keccak256(root),
                expected_audience: "base-tdx-prover".into(),
                expected_public_key: Bytes::from_static(&PUBLIC_KEY),
                attestation_nonce: Some(registrar_nonce),
                chain_id: 11_155_111,
                registry_address,
                verification_time: VERIFICATION_TIME,
                max_token_age_seconds: 300,
            }),
            Err(TdxVerifierError::TokenClaimsInvalid)
        ));
    }

    fn signed_test_token(claims: Value) -> Vec<u8> {
        let header = serde_json::json!({ "alg": "RS256", "x5c": [TEST_CERTIFICATE_B64] });
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{header}.{claims}");
        let key = RsaPrivateKey::from_pkcs1_pem(TEST_ROOT_KEY).unwrap();
        let signature = RsaSigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())).into_bytes()
    }
}
