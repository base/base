//! EVM storage adapter for the security B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, ContractStorage, Handler, Mapping, Result, StorageCtx};

use super::accounting::SecurityAccounting;
use crate::{TokenAccounting, TokenVariant};

/// EVM-backed storage for a security B-20 token.
///
/// Slots 0–10 mirror [`crate::B20TokenStorage`] exactly so that the factory can
/// initialize common fields through either storage type. Slot 11 holds the
/// share-to-tokens ratio. Slots 12–13 hold security-specific mappings whose keys
/// are `keccak256`-hashed strings (since `String` is not a valid `StorageKey`).
#[contract]
pub struct B20SecurityStorage {
    pub total_supply: U256,                                   // slot 0
    pub supply_cap: U256,                                     // slot 1
    pub balances: Mapping<Address, U256>,                     // slot 2
    pub allowances: Mapping<Address, Mapping<Address, U256>>, // slot 3
    pub paused: U256,                                         // slot 4
    pub nonces: Mapping<Address, U256>,                       // slot 5
    pub name: String,                                         // slot 6
    pub symbol: String,                                       // slot 7
    pub minimum_redeemable: U256,                             // slot 8 (in shares for security)
    pub contract_uri: String,                                 // slot 9
    pub capabilities: U256,                                   // slot 10
    pub shares_to_tokens_ratio: U256,                         // slot 11
    pub security_identifiers: Mapping<B256, String>,          // slot 12  (key = keccak256(type))
    pub announcement_ids_used: Mapping<B256, bool>,           // slot 13  (key = keccak256(id))
}

impl<'a> B20SecurityStorage<'a> {
    /// Creates a `B20SecurityStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// `initial_isin` may be empty; when non-empty it is stored under the
    /// `keccak256("ISIN")` key. Events for the initial ISIN are emitted by the factory.
    pub fn initialize(
        &mut self,
        name: String,
        symbol: String,
        supply_cap: U256,
        capabilities: U256,
        initial_shares_to_tokens_ratio: U256,
        initial_isin: String,
        minimum_redeemable: U256,
    ) -> Result<()> {
        self.name.write(name)?;
        self.symbol.write(symbol)?;
        self.supply_cap.write(supply_cap)?;
        self.capabilities.write(capabilities)?;
        self.shares_to_tokens_ratio.write(initial_shares_to_tokens_ratio)?;
        self.minimum_redeemable.write(minimum_redeemable)?;
        if !initial_isin.is_empty() {
            let key = alloy_primitives::keccak256(b"ISIN");
            self.security_identifiers.at_mut(&key).write(initial_isin)?;
        }
        Ok(())
    }
}

impl TokenAccounting for B20SecurityStorage<'_> {
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

    fn capabilities(&self) -> Result<U256> {
        self.capabilities.read()
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.emit_event(log)
    }
}

impl SecurityAccounting for B20SecurityStorage<'_> {
    fn shares_to_tokens_ratio(&self) -> Result<U256> {
        self.shares_to_tokens_ratio.read()
    }

    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()> {
        self.shares_to_tokens_ratio.write(ratio)
    }

    fn security_identifier(&self, key: B256) -> Result<String> {
        self.security_identifiers.at(&key).read()
    }

    fn set_security_identifier(&mut self, key: B256, value: String) -> Result<()> {
        if value.is_empty() {
            self.security_identifiers.at_mut(&key).delete()
        } else {
            self.security_identifiers.at_mut(&key).write(value)
        }
    }

    fn is_announcement_id_used(&self, id_hash: B256) -> Result<bool> {
        self.announcement_ids_used.at(&id_hash).read()
    }

    fn mark_announcement_id_used(&mut self, id_hash: B256) -> Result<()> {
        self.announcement_ids_used.at_mut(&id_hash).write(true)
    }
}
