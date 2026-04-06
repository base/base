//! `NitroEnclaveVerifier` contract bindings.
//!
//! Used by the registrar to revoke intermediate certificates that appear on
//! AWS Nitro CRL distribution lists. Only the `revokeCert` function is
//! needed — all other `NitroEnclaveVerifier` interactions happen through the
//! `TEEProverRegistry` during signer registration.

use alloy_sol_types::sol;

// Interface mirrored from the canonical contract source:
// https://github.com/base/contracts/blob/leopoldjoy/chain-3890-add-revoker-role-to-nitroenclaveverifier-for-automated-cert/src/multiproof/tee/NitroEnclaveVerifier.sol
sol! {
    /// `NitroEnclaveVerifier` contract interface (revocation subset).
    #[sol(rpc)]
    interface INitroEnclaveVerifier {
        /// Revokes a cached intermediate certificate by its accumulated path digest.
        function revokeCert(bytes32 certHash) external;
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{FixedBytes, b256};
    use alloy_sol_types::SolCall;
    use rstest::rstest;

    use super::*;

    /// Known test certificate hash for selector verification.
    const TEST_CERT_HASH: FixedBytes<32> =
        b256!("0000000000000000000000000000000000000000000000000000000000000001");

    /// Expected ABI-encoded length for `revokeCert(bytes32)`:
    /// 4 bytes (selector) + 32 bytes (bytes32 argument).
    const REVOKE_CERT_ENCODED_LEN: usize = 4 + 32;

    #[rstest]
    fn revoke_cert_abi_encodes_correctly() {
        let call = INitroEnclaveVerifier::revokeCertCall { certHash: TEST_CERT_HASH };
        let encoded = call.abi_encode();
        assert_eq!(encoded.len(), REVOKE_CERT_ENCODED_LEN);
        assert_eq!(&encoded[..4], &INitroEnclaveVerifier::revokeCertCall::SELECTOR);
    }

    #[rstest]
    fn selector_is_nonzero() {
        assert_ne!(INitroEnclaveVerifier::revokeCertCall::SELECTOR, [0u8; 4]);
    }
}
