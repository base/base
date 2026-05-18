//! `NitroEnclaveVerifier` contract bindings.
//!
//! Used by the registrar to revoke intermediate certificates that appear on
//! AWS Nitro CRL distribution lists, and to consult the on-chain durable
//! revocation sentinel before submitting registrations. All other
//! `NitroEnclaveVerifier` interactions happen through the `TEEProverRegistry`.

use alloy_primitives::{Address, FixedBytes};
use alloy_provider::RootProvider;
use alloy_sol_types::sol;
use async_trait::async_trait;

use crate::ContractError;

// Interface mirrored from the canonical contract source:
// https://github.com/base/contracts/blob/main/src/L1/proofs/tee/NitroEnclaveVerifier.sol
sol! {
    /// `NitroEnclaveVerifier` contract interface (revocation subset).
    #[sol(rpc)]
    interface INitroEnclaveVerifier {
        /// Revokes a cached intermediate certificate by its accumulated path digest.
        function revokeCert(bytes32 certHash) external;

        /// Returns whether the given accumulated-path-digest hash has been
        /// revoked. Persistent across cache overwrites.
        function revokedCerts(bytes32 certHash) external view returns (bool);
    }
}

/// Reads the durable revocation sentinel from the on-chain
/// `NitroEnclaveVerifier` (CHAIN-4194 / Immunefi #75608).
#[async_trait]
pub trait NitroEnclaveVerifierClient: Send + Sync {
    /// Returns the on-chain address of the verifier contract this client
    /// is bound to.
    fn address(&self) -> Address;

    /// Returns `true` if the given accumulated-path-digest hash is currently
    /// marked as revoked on-chain.
    async fn is_revoked(&self, cert_hash: FixedBytes<32>) -> Result<bool, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct NitroEnclaveVerifierContractClient {
    contract: INitroEnclaveVerifier::INitroEnclaveVerifierInstance<RootProvider>,
}

impl NitroEnclaveVerifierContractClient {
    /// Creates a new client for the given verifier address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = INitroEnclaveVerifier::INitroEnclaveVerifierInstance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl NitroEnclaveVerifierClient for NitroEnclaveVerifierContractClient {
    fn address(&self) -> Address {
        *self.contract.address()
    }

    async fn is_revoked(&self, cert_hash: FixedBytes<32>) -> Result<bool, ContractError> {
        contract_call!(
            self.contract.revokedCerts(cert_hash).call(),
            format!("revokedCerts({cert_hash})")
        )
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

    /// Expected ABI-encoded length for a `bytes32`-only call:
    /// 4 bytes (selector) + 32 bytes (bytes32 argument).
    const BYTES32_CALL_ENCODED_LEN: usize = 4 + 32;

    #[rstest]
    #[case::revoke_cert(
        INitroEnclaveVerifier::revokeCertCall { certHash: TEST_CERT_HASH }.abi_encode(),
        INitroEnclaveVerifier::revokeCertCall::SELECTOR,
    )]
    #[case::revoked_certs(
        INitroEnclaveVerifier::revokedCertsCall { certHash: TEST_CERT_HASH }.abi_encode(),
        INitroEnclaveVerifier::revokedCertsCall::SELECTOR,
    )]
    fn bytes32_call_abi_encodes_correctly(#[case] encoded: Vec<u8>, #[case] selector: [u8; 4]) {
        assert_eq!(encoded.len(), BYTES32_CALL_ENCODED_LEN);
        assert_eq!(&encoded[..4], &selector);
    }

    #[rstest]
    fn revocation_selectors_are_nonzero_and_distinct() {
        let selectors = [
            INitroEnclaveVerifier::revokeCertCall::SELECTOR,
            INitroEnclaveVerifier::revokedCertsCall::SELECTOR,
        ];

        for selector in &selectors {
            assert_ne!(selector, &[0u8; 4], "selector must be non-zero: {selector:?}");
        }
        for (i, a) in selectors.iter().enumerate() {
            for b in &selectors[i + 1..] {
                assert_ne!(a, b, "selectors must be distinct: {a:?} vs {b:?}");
            }
        }
    }
}
