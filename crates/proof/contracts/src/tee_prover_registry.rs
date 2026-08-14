//! `TEEProverRegistry` contract bindings.
//!
//! Used by the registrar to manage signer registration and deregistration,
//! and by the proposer to validate signers before on-chain submission.

use alloy_primitives::{Address, Bytes};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// `TEEProverRegistry` contract interface.
    #[sol(rpc)]
    interface ITEEProverRegistry {
        /// Thrown when the attestation timestamp is in the future.
        error AttestationFromFuture();

        /// Thrown when the attestation document is too old.
        error AttestationTooOld();

        /// Thrown when the attestation's PCR0 is malformed or not trusted.
        error InvalidPCR0();

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

        /// Returns the validator configured immutably on this Registry implementation.
        function NITRO_VALIDATOR() external view returns (address);

        /// Registers a signer using a hinted AWS Nitro attestation.
        function registerSigner(
            bytes calldata attestationTbs,
            bytes calldata signature,
            bytes calldata hints
        ) external;

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

/// Reads registration state from the on-chain `TEEProverRegistry`.
#[async_trait]
pub trait TEEProverRegistryClient: Send + Sync {
    /// Returns the Registry address this client is bound to.
    fn address(&self) -> Address;

    /// Returns the `NitroValidator` address configured on the Registry implementation.
    async fn nitro_validator(&self) -> Result<Address, ContractError>;

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
    fn address(&self) -> Address {
        *self.contract.address()
    }

    async fn nitro_validator(&self) -> Result<Address, ContractError> {
        contract_call!(self.contract.NITRO_VALIDATOR().call(), "NITRO_VALIDATOR()")
    }

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

/// Encodes calldata for the final three-argument `registerSigner` call.
pub fn encode_register_signer_calldata(
    attestation_tbs: Bytes,
    signature: Bytes,
    hints: Bytes,
) -> Bytes {
    Bytes::from(
        ITEEProverRegistry::registerSignerCall {
            attestationTbs: attestation_tbs,
            signature,
            hints,
        }
        .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall as _;

    use super::*;

    #[test]
    fn registry_selectors_match_final_contract_abi() {
        assert_eq!(ITEEProverRegistry::NITRO_VALIDATORCall::SELECTOR, [0x96, 0xce, 0x7e, 0x96]);
        assert_eq!(ITEEProverRegistry::registerSignerCall::SELECTOR, [0xb3, 0x9d, 0xc0, 0x9d]);
    }

    #[test]
    fn register_signer_calldata_roundtrips() {
        let attestation_tbs = Bytes::from_static(b"attestation-tbs");
        let signature = Bytes::from(vec![0x11; 96]);
        let hints = Bytes::from(vec![0x22; 144]);

        let calldata = encode_register_signer_calldata(
            attestation_tbs.clone(),
            signature.clone(),
            hints.clone(),
        );
        let decoded = ITEEProverRegistry::registerSignerCall::abi_decode(&calldata)
            .expect("registerSigner calldata should decode");

        assert_eq!(decoded.attestationTbs, attestation_tbs);
        assert_eq!(decoded.signature, signature);
        assert_eq!(decoded.hints, hints);
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
        assert_ne!(ITEEProverRegistry::NITRO_VALIDATORCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::registerSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::deregisterSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::isValidSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::isRegisteredSignerCall::SELECTOR, [0u8; 4]);
        assert_ne!(ITEEProverRegistry::getRegisteredSignersCall::SELECTOR, [0u8; 4]);
    }
}
