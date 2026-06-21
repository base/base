//! `TEEProverRegistry` contract bindings.
//!
//! Used by the registrar to manage signer registration and deregistration,
//! and by the proposer to validate signers before on-chain submission.

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_sol_types::{SolError, sol};
use async_trait::async_trait;

use crate::ContractError;

// Interface mirrored from the canonical contract source:
// https://github.com/base/contracts/blob/96b132077b86bdc77f3f96dd40e09dad363df32e/src/multiproof/tee/TEEProverRegistry.sol
sol! {
    /// `TEEProverRegistry` contract interface.
    #[sol(rpc)]
    interface ITEEProverRegistry {
        /// Thrown when the attestation document is too old.
        error AttestationTooOld();

        /// Thrown when the ZK attestation verification fails.
        error AttestationVerificationFailed();

        /// Thrown when the attestation's public key is malformed.
        error InvalidPublicKey();

        /// Thrown when PCR0 is not found in the attestation's PCR list.
        error PCR0NotFound();

        /// Thrown when the dispute game factory is not configured.
        error DisputeGameFactoryNotSet();

        /// Thrown when reading `TEE_IMAGE_HASH` from the `AggregateVerifier` fails.
        error ImageHashReadFailed();

        /// Thrown when the selected game type has no `TEE_IMAGE_HASH`.
        error InvalidGameType();

        /// Registers a signer using a ZK-proven AWS Nitro attestation.
        function registerSigner(bytes calldata output, bytes calldata proofBytes) external;

        /// Deregisters a signer.
        function deregisterSigner(address signer) external;

        /// Returns `true` if the signer is registered AND its image hash matches
        /// the contract's current expected image hash.
        function isValidSigner(address signer) external view returns (bool);

        /// Returns `true` if the signer has been registered, regardless of
        /// whether its image hash matches the current expected value.
        function isRegisteredSigner(address signer) external view returns (bool);

        /// Returns all currently registered signer addresses.
        function getRegisteredSigners() external view returns (address[]);
    }
}

/// Returns a human-readable name for known `TEEProverRegistry` custom-error
/// revert data.
pub fn decode_tee_prover_registry_revert(data: &[u8]) -> Option<&'static str> {
    let selector = data.get(..4)?;
    match selector {
        s if s == ITEEProverRegistry::AttestationTooOld::SELECTOR => Some("AttestationTooOld"),
        s if s == ITEEProverRegistry::AttestationVerificationFailed::SELECTOR => {
            Some("AttestationVerificationFailed")
        }
        s if s == ITEEProverRegistry::InvalidPublicKey::SELECTOR => Some("InvalidPublicKey"),
        s if s == ITEEProverRegistry::PCR0NotFound::SELECTOR => Some("PCR0NotFound"),
        s if s == ITEEProverRegistry::DisputeGameFactoryNotSet::SELECTOR => {
            Some("DisputeGameFactoryNotSet")
        }
        s if s == ITEEProverRegistry::ImageHashReadFailed::SELECTOR => Some("ImageHashReadFailed"),
        s if s == ITEEProverRegistry::InvalidGameType::SELECTOR => Some("InvalidGameType"),
        _ => None,
    }
}

/// Reads registration state from the on-chain `TEEProverRegistry`.
#[async_trait]
pub trait TEEProverRegistryClient: Send + Sync {
    /// Returns `true` if `signer` is registered AND its image hash matches
    /// the contract's current expected image hash.
    async fn is_valid_signer(&self, signer: Address) -> Result<bool, ContractError>;

    /// Returns `true` if `signer` has been registered, regardless of whether
    /// its image hash matches the current expected value.
    async fn is_registered_signer(&self, signer: Address) -> Result<bool, ContractError>;

    /// Fetches the complete set of registered signer addresses.
    async fn get_registered_signers(&self) -> Result<Vec<Address>, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct TEEProverRegistryContractClient {
    contract: ITEEProverRegistry::ITEEProverRegistryInstance<RootProvider>,
}

impl TEEProverRegistryContractClient {
    /// Creates a new client for the given registry address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = ITEEProverRegistry::ITEEProverRegistryInstance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl TEEProverRegistryClient for TEEProverRegistryContractClient {
    async fn is_valid_signer(&self, signer: Address) -> Result<bool, ContractError> {
        contract_call!(
            self.contract.isValidSigner(signer).call(),
            format!("isValidSigner({signer})")
        )
    }

    async fn is_registered_signer(&self, signer: Address) -> Result<bool, ContractError> {
        contract_call!(
            self.contract.isRegisteredSigner(signer).call(),
            format!("isRegisteredSigner({signer})")
        )
    }

    async fn get_registered_signers(&self) -> Result<Vec<Address>, ContractError> {
        contract_call!(self.contract.getRegisteredSigners().call(), "getRegisteredSigners()")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes};
    use alloy_sol_types::{SolCall, SolError};

    use super::*;

    #[test]
    fn register_signer_abi_encodes_correctly() {
        let call = ITEEProverRegistry::registerSignerCall {
            output: Bytes::new(),
            proofBytes: Bytes::new(),
        };
        let encoded = call.abi_encode();
        // 4 (selector) + 2×32 (offsets) + 2×32 (lengths) + 0 (data) = 132
        assert_eq!(encoded.len(), 132);
        assert_eq!(&encoded[..4], &ITEEProverRegistry::registerSignerCall::SELECTOR);
    }

    #[test]
    fn deregister_signer_abi_encodes_correctly() {
        let call = ITEEProverRegistry::deregisterSignerCall { signer: Address::ZERO };
        let encoded = call.abi_encode();
        // 4 (selector) + 32 (padded address) = 36
        assert_eq!(encoded.len(), 36);
        assert_eq!(&encoded[..4], &ITEEProverRegistry::deregisterSignerCall::SELECTOR);
    }

    #[test]
    fn all_selectors_are_nonzero() {
        assert_ne!(ITEEProverRegistry::registerSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::deregisterSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::isValidSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::isRegisteredSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::getRegisteredSignersCall::SELECTOR, [0u8; 4]);
    }

    #[test]
    fn known_custom_errors_decode() {
        for (selector, name) in [
            (ITEEProverRegistry::AttestationTooOld::SELECTOR, "AttestationTooOld"),
            (
                ITEEProverRegistry::AttestationVerificationFailed::SELECTOR,
                "AttestationVerificationFailed",
            ),
            (ITEEProverRegistry::InvalidPublicKey::SELECTOR, "InvalidPublicKey"),
            (ITEEProverRegistry::PCR0NotFound::SELECTOR, "PCR0NotFound"),
            (ITEEProverRegistry::DisputeGameFactoryNotSet::SELECTOR, "DisputeGameFactoryNotSet"),
            (ITEEProverRegistry::ImageHashReadFailed::SELECTOR, "ImageHashReadFailed"),
            (ITEEProverRegistry::InvalidGameType::SELECTOR, "InvalidGameType"),
        ] {
            assert_eq!(decode_tee_prover_registry_revert(&selector), Some(name));

            let mut data = selector.to_vec();
            data.extend_from_slice(&[0xAA; 32]);
            assert_eq!(decode_tee_prover_registry_revert(&data), Some(name));
        }
    }

    #[test]
    fn unknown_custom_errors_do_not_decode() {
        assert_eq!(decode_tee_prover_registry_revert(&[]), None);
        assert_eq!(decode_tee_prover_registry_revert(&[0x00, 0x01, 0x02]), None);
        assert_eq!(decode_tee_prover_registry_revert(&[0xFF; 4]), None);
    }
}
