//! `NitroValidator` contract bindings.
//!
//! The Registrar discovers `CertManager` through the validator configured on
//! the Registry instead of accepting an independently configured address.

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_sol_types::sol;
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// `NitroValidator` discovery interface.
    #[sol(rpc)]
    interface INitroValidator {
        /// Returns the `CertManager` configured immutably on this validator.
        function certManager() external view returns (address);
    }
}

/// Discovers `CertManager` from the `NitroValidator` selected by the Registry.
#[async_trait]
pub trait NitroValidatorClient: Send + Sync + std::fmt::Debug {
    /// Returns the validator address this client is bound to.
    fn address(&self) -> Address;

    /// Returns the `CertManager` address configured on this validator.
    async fn cert_manager(&self) -> Result<Address, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct NitroValidatorContractClient {
    contract: INitroValidator::INitroValidatorInstance<RootProvider>,
}

impl NitroValidatorContractClient {
    /// Creates a client for the validator returned by the Registry.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = INitroValidator::INitroValidatorInstance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl NitroValidatorClient for NitroValidatorContractClient {
    fn address(&self) -> Address {
        *self.contract.address()
    }

    async fn cert_manager(&self) -> Result<Address, ContractError> {
        contract_call!(self.contract.certManager().call(), "certManager()")
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall as _;

    use super::*;

    #[test]
    fn cert_manager_selector_matches_final_contract_abi() {
        assert_eq!(INitroValidator::certManagerCall::SELECTOR, [0x73, 0x9e, 0x84, 0x84]);
    }
}
