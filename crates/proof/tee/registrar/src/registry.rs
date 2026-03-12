//! `TEEProverRegistry` contract bindings and registry checker.

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_sol_types::sol;
use url::Url;

use crate::{RegistrarError, Result};

sol! {
    /// `TEEProverRegistry` contract interface.
    #[sol(rpc)]
    interface ITEEProverRegistry {
        /// Registers a signer using a ZK-proven AWS Nitro attestation.
        function registerSigner(bytes calldata output, bytes calldata proofBytes) external;

        /// Deregisters a signer.
        function deregisterSigner(address signer) external;

        /// Returns the PCR0 hash a signer was registered with, or `bytes32(0)` if not registered.
        function signerPCR0(address signer) external view returns (bytes32);

        /// Returns `true` if the signer is currently registered.
        function isValidSigner(address signer) external view returns (bool);

        /// Returns all currently registered signer addresses.
        function getRegisteredSigners() external view returns (address[]);
    }
}

/// Reads registration state from the on-chain `TEEProverRegistry`.
#[derive(Debug)]
pub struct RegistryChecker {
    contract: ITEEProverRegistry::ITEEProverRegistryInstance<RootProvider>,
}

impl RegistryChecker {
    /// Creates a new checker for the given registry address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = ITEEProverRegistry::ITEEProverRegistryInstance::new(address, provider);
        Self { contract }
    }

    /// Returns `true` if `signer` is currently registered on-chain.
    pub async fn is_registered(&self, signer: Address) -> Result<bool> {
        self.contract
            .isValidSigner(signer)
            .call()
            .await
            .map_err(|e| RegistrarError::Registry(Box::new(e)))
    }

    /// Fetches the complete set of registered signer addresses in a single view call.
    pub async fn get_registered_signers(&self) -> Result<Vec<Address>> {
        self.contract
            .getRegisteredSigners()
            .call()
            .await
            .map_err(|e| RegistrarError::Registry(Box::new(e)))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes};
    use alloy_sol_types::SolCall;

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
        assert_ne!(ITEEProverRegistry::signerPCR0Call::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::isValidSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::getRegisteredSignersCall::SELECTOR, [0u8; 4]);
    }
}
