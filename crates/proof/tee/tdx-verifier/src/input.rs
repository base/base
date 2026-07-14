//! ABI-compatible host and guest input encoding for Confidential Space verification.

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolValue, sol};

use crate::{Result, TdxVerifier, TdxVerifierError};

/// Complete explicit Confidential Space verifier input.
#[derive(Debug)]
pub struct TdxVerifierInput {
    /// Google Cloud Attestation PKI token from the Confidential Space launcher.
    pub token: Bytes,
    /// Keccak256 hash of the accepted Google Confidential Space root certificate.
    pub trusted_root_ca_hash: B256,
    /// Token audience expected by the relying party.
    pub expected_audience: String,
    /// Expected uncompressed secp256k1 signer public key.
    pub expected_public_key: Bytes,
    /// Optional registrar nonce used to derive the expected token nonce.
    pub attestation_nonce: Option<B256>,
    /// L1 chain ID bound into the token nonce.
    pub chain_id: u64,
    /// `TEEProverRegistry` address bound into the token nonce.
    pub registry_address: Address,
    /// Verification time in seconds since Unix epoch.
    pub verification_time: u64,
    /// Maximum accepted token age in seconds.
    pub max_token_age_seconds: u64,
}

sol! {
    /// Complete explicit Confidential Space verifier input encoded for a RISC Zero guest.
    struct TdxVerifierInputAbi {
        /// Google Cloud Attestation PKI token from the Confidential Space launcher.
        bytes token;
        /// Keccak256 hash of the accepted Google Confidential Space root certificate.
        bytes32 trustedRootCaHash;
        /// Token audience expected by the relying party.
        string expectedAudience;
        /// Expected uncompressed secp256k1 signer public key.
        bytes expectedPublicKey;
        /// Whether the token binds a registrar nonce.
        bool hasAttestationNonce;
        /// Registrar nonce used to derive the expected token nonce.
        bytes32 attestationNonce;
        /// L1 chain ID bound into the token nonce.
        uint64 chainId;
        /// `TEEProverRegistry` address bound into the token nonce.
        address registryAddress;
        /// Verification time in seconds since Unix epoch.
        uint64 verificationTime;
        /// Maximum accepted token age in seconds.
        uint64 maxTokenAgeSeconds;
    }
}

impl TdxVerifierInput {
    /// ABI-encodes this input for host-to-guest transport.
    pub fn encode(&self) -> Vec<u8> {
        SolValue::abi_encode(&self.to_abi_input())
    }

    /// ABI-decodes a verifier input.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let abi = <TdxVerifierInputAbi as SolValue>::abi_decode_validate(buf)
            .map_err(|error| TdxVerifierError::InputDecode(error.to_string()))?;
        Ok(Self {
            token: abi.token,
            trusted_root_ca_hash: abi.trustedRootCaHash,
            expected_audience: abi.expectedAudience,
            expected_public_key: abi.expectedPublicKey,
            attestation_nonce: abi.hasAttestationNonce.then_some(abi.attestationNonce),
            chain_id: abi.chainId,
            registry_address: abi.registryAddress,
            verification_time: abi.verificationTime,
            max_token_age_seconds: abi.maxTokenAgeSeconds,
        })
    }

    /// ABI-decodes a verifier input and verifies it targets `signer_address`.
    pub fn decode_for_signer(buf: &[u8], signer_address: Address) -> Result<Self> {
        let input = Self::decode(buf)?;
        let public_key_hash = TdxVerifier::validate_public_key(&input.expected_public_key)?;
        let actual_signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        if actual_signer != signer_address {
            return Err(TdxVerifierError::SignerMismatch {
                expected: signer_address,
                actual: actual_signer,
            });
        }
        Ok(input)
    }

    fn to_abi_input(&self) -> TdxVerifierInputAbi {
        TdxVerifierInputAbi {
            token: self.token.clone(),
            trustedRootCaHash: self.trusted_root_ca_hash,
            expectedAudience: self.expected_audience.clone(),
            expectedPublicKey: self.expected_public_key.clone(),
            hasAttestationNonce: self.attestation_nonce.is_some(),
            attestationNonce: self.attestation_nonce.unwrap_or(B256::ZERO),
            chainId: self.chain_id,
            registryAddress: self.registry_address,
            verificationTime: self.verification_time,
            maxTokenAgeSeconds: self.max_token_age_seconds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_key() -> Bytes {
        Bytes::from_static(&[
            0x04, 0x1b, 0x84, 0xc5, 0x56, 0x7b, 0x12, 0x64, 0x40, 0x99, 0x5d, 0x3e, 0xd5, 0xaa,
            0xba, 0x05, 0x65, 0xd7, 0x1e, 0x18, 0x34, 0x60, 0x48, 0x19, 0xff, 0x9c, 0x17, 0xf5,
            0xe9, 0xd5, 0xdd, 0x07, 0x8f, 0x70, 0xbe, 0xaf, 0x8f, 0x58, 0x8b, 0x54, 0x15, 0x07,
            0xfe, 0xd6, 0xa6, 0x42, 0xc5, 0xab, 0x42, 0xdf, 0xdf, 0x81, 0x20, 0xa7, 0xf6, 0x39,
            0xde, 0x51, 0x22, 0xd4, 0x7a, 0x69, 0xa8, 0xe8, 0xd1,
        ])
    }

    fn verifier_input() -> TdxVerifierInput {
        TdxVerifierInput {
            token: Bytes::from_static(b"header.claims.signature"),
            trusted_root_ca_hash: B256::repeat_byte(0x55),
            expected_audience: "base-tdx-prover".into(),
            expected_public_key: public_key(),
            attestation_nonce: Some(B256::repeat_byte(0x66)),
            chain_id: 11_155_111,
            registry_address: Address::repeat_byte(0x88),
            verification_time: 1_711_111_222,
            max_token_age_seconds: 300,
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
    fn decode_for_signer_rejects_mismatched_signer() {
        let encoded = verifier_input().encode();

        assert!(matches!(
            TdxVerifierInput::decode_for_signer(&encoded, Address::repeat_byte(0x99)),
            Err(TdxVerifierError::SignerMismatch { .. })
        ));
    }
}
