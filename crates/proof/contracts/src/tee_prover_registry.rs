//! `TEEProverRegistry` contract bindings.
//!
//! Used by the registrar to manage signer registration and deregistration,
//! and by the proposer to validate signers before on-chain submission.

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_sol_types::sol;
use async_trait::async_trait;

use crate::ContractError;

// Interface mirrored from the canonical contract source:
// https://github.com/base/contracts/blob/96b132077b86bdc77f3f96dd40e09dad363df32e/src/multiproof/tee/TEEProverRegistry.sol
sol! {
    /// `TEEProverRegistry` contract interface.
    #[sol(rpc)]
    interface ITEEProverRegistry {
        /// Registers a signer using a ZK-proven AWS Nitro attestation.
        function registerSigner(bytes calldata output, bytes calldata proofBytes) external;

        /// Deregisters a signer.
        function deregisterSigner(address signer) external;

        /// Returns `true` if the signer is currently registered.
        function isValidSigner(address signer) external view returns (bool);

        /// Returns all currently registered signer addresses.
        function getRegisteredSigners() external view returns (address[]);
    }
}

/// Reads registration state from the on-chain `TEEProverRegistry`.
#[async_trait]
pub trait TEEProverRegistryClient: Send + Sync {
    /// Returns `true` if `signer` is currently registered on-chain.
    async fn is_valid_signer(&self, signer: Address) -> Result<bool, ContractError>;

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
        self.contract.isValidSigner(signer).call().await.map_err(|e| ContractError::Call {
            context: format!("isValidSigner({signer})"),
            source: e,
        })
    }

    async fn get_registered_signers(&self) -> Result<Vec<Address>, ContractError> {
        self.contract.getRegisteredSigners().call().await.map_err(|e| ContractError::Call {
            context: "getRegisteredSigners()".into(),
            source: e,
        })
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
        assert_ne!(ITEEProverRegistry::isValidSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::getRegisteredSignersCall::SELECTOR, [0u8; 4]);
    }
}
