//! EVM storage adapter for the stablecoin B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::contract;
use base_precompile_storage::{
    BasePrecompileError, ContractStorage, Handler, Mapping, Result, StorageCtx,
};
#[cfg(feature = "std")]
use iso_currency::Currency;

#[cfg(feature = "std")]
use super::IB20Stablecoin;
use super::accounting::StablecoinAccounting;
use crate::{TokenAccounting, TokenVariant};

/// EVM-backed storage for a stablecoin B-20 token.
///
/// Slots 0–10 mirror [`crate::B20TokenStorage`] exactly so that the factory can
/// initialize common fields through either storage type. Slot 11 holds the
/// immutable `currency` identifier written once at creation.
#[contract]
pub struct B20StablecoinStorage {
    pub total_supply: U256,                                   // slot 0
    pub supply_cap: U256,                                     // slot 1
    pub balances: Mapping<Address, U256>,                     // slot 2
    pub allowances: Mapping<Address, Mapping<Address, U256>>, // slot 3
    pub paused: U256,                                         // slot 4
    pub nonces: Mapping<Address, U256>,                       // slot 5
    pub name: String,                                         // slot 6
    pub symbol: String,                                       // slot 7
    pub minimum_redeemable: U256,                             // slot 8
    pub contract_uri: String,                                 // slot 9
    pub capabilities: U256,                                   // slot 10
    pub currency: String,                                     // slot 11
}

impl<'a> B20StablecoinStorage<'a> {
    /// Creates a `B20StablecoinStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// Validates that `currency` is a recognised ISO 4217 code before writing
    /// anything; reverts `IB20Stablecoin::InvalidCurrency` otherwise.
    pub fn initialize(
        &mut self,
        name: String,
        symbol: String,
        supply_cap: U256,
        capabilities: U256,
        currency: String,
    ) -> Result<()> {
        #[cfg(feature = "std")]
        if Currency::from_code(&currency).is_none() {
            return Err(BasePrecompileError::revert(IB20Stablecoin::InvalidCurrency {}));
        }
        self.name.write(name)?;
        self.symbol.write(symbol)?;
        self.supply_cap.write(supply_cap)?;
        self.capabilities.write(capabilities)?;
        self.currency.write(currency)
    }
}

impl TokenAccounting for B20StablecoinStorage<'_> {
    fn token_address(&self) -> Address {
        ContractStorage::address(self)
    }

    fn is_initialized(&self) -> Result<bool> {
        ContractStorage::is_initialized(self)
    }

    fn balance_of(&self, account: Address) -> Result<U256> {
        self.balances.at(&account).read()
    }

    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        self.balances.at_mut(&account).write(balance)
    }

    fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        self.allowances.at(&owner).at(&spender).read()
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        self.allowances.at_mut(&owner).at_mut(&spender).write(amount)
    }

    fn total_supply(&self) -> Result<U256> {
        self.total_supply.read()
    }

    fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.total_supply.write(supply)
    }

    fn supply_cap(&self) -> Result<U256> {
        self.supply_cap.read()
    }

    fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.supply_cap.write(cap)
    }

    fn name(&self) -> Result<String> {
        self.name.read()
    }

    fn set_name(&mut self, name: String) -> Result<()> {
        self.name.write(name)
    }

    fn symbol(&self) -> Result<String> {
        self.symbol.read()
    }

    fn set_symbol(&mut self, symbol: String) -> Result<()> {
        self.symbol.write(symbol)
    }

    fn decimals(&self) -> Result<u8> {
        Ok(TokenVariant::decimals_of(self.address).unwrap_or(0))
    }

    fn paused(&self) -> Result<U256> {
        self.paused.read()
    }

    fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.paused.write(vectors)
    }

    fn nonce(&self, owner: Address) -> Result<U256> {
        self.nonces.at(&owner).read()
    }

    fn increment_nonce(&mut self, owner: Address) -> Result<()> {
        let current = self.nonces.at(&owner).read()?;
        let next =
            current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.nonces.at_mut(&owner).write(next)
    }

    fn minimum_redeemable(&self) -> Result<U256> {
        self.minimum_redeemable.read()
    }

    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()> {
        self.minimum_redeemable.write(minimum)
    }

    fn contract_uri(&self) -> Result<String> {
        self.contract_uri.read()
    }

    fn set_contract_uri(&mut self, uri: String) -> Result<()> {
        self.contract_uri.write(uri)
    }

    fn currency(&self) -> Result<String> {
        self.currency.read()
    }

    fn security_identifier(&self, _identifier_type: &str) -> Result<String> {
        Ok(String::new())
    }

    fn has_role(&self, _role: B256, _account: Address) -> Result<bool> {
        Ok(false)
    }

    fn set_role(&mut self, _role: B256, _account: Address, _enabled: bool) -> Result<()> {
        Ok(())
    }

    fn role_member_count(&self, _role: B256) -> Result<U256> {
        Ok(U256::ZERO)
    }

    fn set_role_member_count(&mut self, _role: B256, _count: U256) -> Result<()> {
        Ok(())
    }

    fn role_admin(&self, _role: B256) -> Result<B256> {
        Ok(B256::ZERO)
    }

    fn set_role_admin(&mut self, _role: B256, _admin_role: B256) -> Result<()> {
        Ok(())
    }

    fn policy_id(&self, _policy_type: B256) -> Result<u64> {
        Ok(0)
    }

    fn set_policy_id(&mut self, _policy_type: B256, _policy_id: u64) -> Result<()> {
        Ok(())
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.emit_event(log)
    }
}

impl StablecoinAccounting for B20StablecoinStorage<'_> {
    fn set_currency(&mut self, currency: String) -> Result<()> {
        self.currency.write(currency)
    }
}
