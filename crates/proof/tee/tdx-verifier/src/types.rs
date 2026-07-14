//! Solidity-aligned types for Confidential Space TDX registration journals.

use alloy_sol_types::sol;

sol! {
    #![sol(extra_derives(Debug, PartialEq, Eq))]

    /// Statuses emitted by the Confidential Space token verifier.
    enum TDXVerificationResult {
        /// Unknown / unset.
        Unknown,
        /// Confidential Space token verification succeeded.
        Success,
        /// The token did not have the expected JWT structure.
        TokenMalformed,
        /// The token signature did not verify.
        TokenSignatureInvalid,
        /// The token's certificate chain did not terminate at the trusted root.
        RootCaNotTrusted,
        /// The token claims did not satisfy the workload policy.
        TokenClaimsInvalid,
        /// The token was expired, issued in the future, or too old.
        TokenExpired,
        /// The token nonce did not bind the signer and registration context.
        TokenNonceMismatch,
    }

    /// Public journal emitted by the Confidential Space verifier guest.
    struct TDXVerifierJournal {
        /// Overall verification result after token validation.
        TDXVerificationResult result;
        /// Token issuance time in seconds since Unix epoch.
        uint64 issuedAt;
        /// Token expiration time in seconds since Unix epoch.
        uint64 expiration;
        /// Hash of the Google Confidential Space root CA used for validation.
        bytes32 rootCaHash;
        /// Hash of the token leaf certificate.
        bytes32 tokenLeafCertHash;
        /// Uncompressed secp256k1 public key: `0x04 || x || y`.
        bytes publicKey;
        /// Ethereum address derived from `publicKey`.
        address signer;
        /// OCI manifest SHA-256 digest for the verified prover workload.
        bytes32 imageHash;
        /// Hash of the expected token audience.
        bytes32 audienceHash;
        /// Hash of the signer-bound registrar nonce in the token.
        bytes32 tokenNonceHash;
        /// Hash of the token's hardware model claim.
        bytes32 hardwareModelHash;
        /// Whether Secure Boot was enabled for the Confidential Space VM.
        bool secureBoot;
        /// Whether the Confidential Space image has been debug-disabled since boot.
        bool debugDisabled;
        /// Whether the workload command was overridden.
        bool commandOverride;
        /// Whether the workload environment was overridden.
        bool environmentOverride;
        /// L1 chain ID bound into the token nonce.
        uint64 chainId;
        /// `TEEProverRegistry` address bound into the token nonce.
        address registryAddress;
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_solidity() {
        for (actual, expected) in [
            (TDXVerificationResult::Unknown as u8, 0),
            (TDXVerificationResult::Success as u8, 1),
            (TDXVerificationResult::TokenMalformed as u8, 2),
            (TDXVerificationResult::TokenSignatureInvalid as u8, 3),
            (TDXVerificationResult::RootCaNotTrusted as u8, 4),
            (TDXVerificationResult::TokenClaimsInvalid as u8, 5),
            (TDXVerificationResult::TokenExpired as u8, 6),
            (TDXVerificationResult::TokenNonceMismatch as u8, 7),
        ] {
            assert_eq!(actual, expected);
        }
    }
}
