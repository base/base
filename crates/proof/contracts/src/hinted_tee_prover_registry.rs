//! Final hinted `TEEProverRegistry` contract bindings.
//!
//! This binding remains separate from the legacy two-argument Registry binding
//! until the Boundless registration backend is removed.

use alloy_primitives::{Address, Bytes};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// Registrar subset of the final hinted `TEEProverRegistry` contract interface.
    #[sol(rpc)]
    interface IHintedTEEProverRegistry {
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

        /// Returns the validator configured immutably on this Registry implementation.
        function NITRO_VALIDATOR() external view returns (address);

        /// Registers a signer using a hinted AWS Nitro attestation.
        function registerSigner(
            bytes calldata attestationTbs,
            bytes calldata signature,
            bytes calldata hints
        ) external;
    }
}

/// Reads the immutable validator address from the final hinted Registry implementation.
#[async_trait]
pub trait HintedTEEProverRegistryClient: Send + Sync + std::fmt::Debug {
    /// Returns the Registry address this client is bound to.
    fn address(&self) -> Address;

    /// Returns the `NitroValidator` address configured on the Registry implementation.
    async fn nitro_validator(&self) -> Result<Address, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct HintedTEEProverRegistryContractClient {
    contract: IHintedTEEProverRegistry::IHintedTEEProverRegistryInstance<RootProvider>,
}

impl HintedTEEProverRegistryContractClient {
    /// Creates a client for the final hinted Registry ABI.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract =
            IHintedTEEProverRegistry::IHintedTEEProverRegistryInstance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl HintedTEEProverRegistryClient for HintedTEEProverRegistryContractClient {
    fn address(&self) -> Address {
        *self.contract.address()
    }

    async fn nitro_validator(&self) -> Result<Address, ContractError> {
        contract_call!(self.contract.NITRO_VALIDATOR().call(), "NITRO_VALIDATOR()")
    }
}

/// Encodes calldata for the final three-argument `registerSigner` call.
pub fn encode_hinted_register_signer_calldata(
    attestation_tbs: Bytes,
    signature: Bytes,
    hints: Bytes,
) -> Bytes {
    Bytes::from(
        IHintedTEEProverRegistry::registerSignerCall {
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
    fn hinted_registry_selectors_match_final_contract_abi() {
        assert_eq!(
            IHintedTEEProverRegistry::NITRO_VALIDATORCall::SELECTOR,
            [0x96, 0xce, 0x7e, 0x96]
        );
        assert_eq!(
            IHintedTEEProverRegistry::registerSignerCall::SELECTOR,
            [0xb3, 0x9d, 0xc0, 0x9d]
        );
    }

    #[test]
    fn hinted_register_signer_calldata_roundtrips() {
        let attestation_tbs = Bytes::from_static(b"attestation-tbs");
        let signature = Bytes::from(vec![0x11; 96]);
        let hints = Bytes::from(vec![0x22; 144]);

        let calldata = encode_hinted_register_signer_calldata(
            attestation_tbs.clone(),
            signature.clone(),
            hints.clone(),
        );
        let decoded = IHintedTEEProverRegistry::registerSignerCall::abi_decode(&calldata)
            .expect("hinted registerSigner calldata should decode");

        assert_eq!(decoded.attestationTbs, attestation_tbs);
        assert_eq!(decoded.signature, signature);
        assert_eq!(decoded.hints, hints);
    }
}
