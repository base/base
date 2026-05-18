//! Solidity-aligned types for the `INitroEnclaveVerifier` on-chain interface.
//!
//! Inlined from the contracts repo's `INitroEnclaveVerifier.sol`. ABI encoding
//! of these types matches exactly what the on-chain verifier expects.

use alloy_primitives::{B128, B256, Bytes};
use alloy_sol_types::{SolValue, sol};
use serde_bytes::ByteArray;

sol! {
    #![sol(all_derives)]

    /// Supported zero-knowledge proof coprocessor types.
    ///
    /// All variants must be present to match the on-chain `INitroEnclaveVerifier.sol`
    /// enum ordering for correct ABI encoding, even if not all are used.
    enum ZkCoProcessorType {
        /// Unknown / unset.
        Unknown,
        /// RISC Zero zkVM proving system.
        RiscZero,
        /// Succinct SP1 proving system (not currently used — present for ABI compatibility).
        Succinct,
    }

    /// Configuration for a specific zero-knowledge coprocessor.
    struct ZkCoProcessorConfig {
        /// Latest program ID for single attestation verification.
        bytes32 verifierId;
        /// Latest program ID for batch/aggregated verification.
        bytes32 aggregatorId;
        /// Default ZK verifier contract address.
        address zkVerifier;
    }

    /// Input structure for attestation report verification.
    struct VerifierInput {
        /// Number of trusted certificates in the chain.
        uint8 trustedCertsPrefixLen;
        /// Raw AWS Nitro Enclave attestation report (`COSE_Sign1` format).
        bytes attestationReport;
    }

    /// Output structure containing verified attestation data and metadata.
    struct VerifierJournal {
        /// Overall verification result status.
        VerificationResult result;
        /// Number of certificates that were trusted during verification.
        uint8 trustedCertsPrefixLen;
        /// Attestation timestamp (Unix timestamp in milliseconds).
        uint64 timestamp;
        /// Array of certificate hashes in the chain (root to leaf).
        bytes32[] certs;
        /// Array of certificate expiry timestamps (notAfter, seconds since epoch).
        /// One entry per cert, matching the `certs` array ordering.
        uint64[] certExpiries;
        /// User-defined data embedded in the attestation.
        bytes userData;
        /// Cryptographic nonce used for replay protection.
        bytes nonce;
        /// Public key extracted from the attestation.
        bytes publicKey;
        /// Platform Configuration Registers (integrity measurements).
        Pcr[] pcrs;
        /// AWS Nitro Enclave module identifier.
        string moduleId;
    }

    /// Public value (journal) structure for batch verification operations.
    struct BatchVerifierJournal {
        /// Verification key that was used for batch verification.
        bytes32 verifierVk;
        /// Array of verified attestation results.
        VerifierJournal[] outputs;
    }

    /// 48-byte data structure for storing PCR values.
    struct Bytes48 {
        /// First 32 bytes.
        bytes32 first;
        /// Last 16 bytes.
        bytes16 second;
    }

    /// Platform Configuration Register (PCR) entry.
    struct Pcr {
        /// PCR index number (0-23 for AWS Nitro Enclaves).
        uint64 index;
        /// 48-byte PCR measurement value (SHA-384 hash).
        Bytes48 value;
    }

    /// Possible attestation verification results.
    ///
    /// `Unknown` is intentionally placed at index 0 so that uninitialized enum
    /// variables default to a failure state rather than `Success` (fail-closed).
    /// This ordering **must** match `INitroEnclaveVerifier.sol`.
    enum VerificationResult {
        /// Default / uninitialized — treated as a verification failure.
        Unknown,
        /// Attestation successfully verified.
        Success,
        /// Root certificate is not in the trusted set.
        RootCertNotTrusted,
        /// One or more intermediate certificates are not trusted.
        IntermediateCertsNotTrusted,
        /// Attestation timestamp is outside acceptable range.
        InvalidTimestamp,
    }
}

impl Bytes48 {
    /// Returns `true` if both halves are zero.
    pub fn is_zero(&self) -> bool {
        self.first.is_zero() && self.second.is_zero()
    }

    /// Concatenates both halves into a 48-byte `Bytes` value.
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = [0u8; 48];
        buf[..32].copy_from_slice(self.first.as_slice());
        buf[32..].copy_from_slice(self.second.as_slice());
        Bytes::copy_from_slice(&buf)
    }
}

impl From<&ByteArray<48>> for Bytes48 {
    fn from(input: &ByteArray<48>) -> Self {
        Self { first: B256::from_slice(&input[..32]), second: B128::from_slice(&input[32..]) }
    }
}

macro_rules! impl_abi_codec {
    ($($ty:ty),+ $(,)?) => {$(
        impl $ty {
            /// ABI-encodes this value.
            pub fn encode(&self) -> Vec<u8> {
                SolValue::abi_encode(self)
            }

            /// ABI-decodes from raw bytes.
            pub fn decode(buf: &[u8]) -> crate::Result<Self> {
                <Self as SolValue>::abi_decode_validate(buf)
                    .map_err(|e| crate::VerifierError::AttestationFormat(e.to_string()))
            }
        }
    )+};
}

impl_abi_codec!(VerifierInput, VerifierJournal, BatchVerifierJournal);
