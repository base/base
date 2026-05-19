use alloc::string::String;

use alloy_primitives::{Address, LogData, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Mapping, Result, StorageCtx};

use crate::token::{common::TokenAccounting, decimals_of};

/// Canonical precompile address for the `B20Token` (placeholder — replace before deployment).
pub const B20_TOKEN_ADDRESS: Address = address!("0000000000000000000000000000000000000900");

#[contract(addr = B20_TOKEN_ADDRESS)]
pub struct B20TokenStorage {
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
}

impl<'a> B20TokenStorage<'a> {
    /// Creates a `B20TokenStorage` instance targeting `addr`.
    ///
    /// Used by the factory to initialize token storage at a dynamically computed address.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }
}

impl TokenAccounting for B20TokenStorage<'_> {
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
        Ok(decimals_of(&self.address))
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

    fn capabilities(&self) -> Result<U256> {
        self.capabilities.read()
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.emit_event(log)
    }
}
