//! In-memory fakes of [`TokenAccounting`] and [`Policy`] for unit tests.
//!
//! Use these for capability/ops logic tests (Transferable, Mintable, …).
//! For factory, dispatch, and storage-layout tests keep the EVM harness.

use std::collections::HashMap;

use alloy_primitives::{Address, LogData, U256};
use base_precompile_storage::Result;

use crate::{
    b20::B20Token,
    common::{Policy, TokenAccounting},
};

/// Convenience alias: [`B20Token`] wired with both in-memory fakes.
///
/// Use this in unit tests instead of spelling out the full generic each time.
pub type TestToken = B20Token<InMemoryTokenAccounting, InMemoryPolicy>;

/// HashMap-backed [`TokenAccounting`] for unit tests.
///
/// Collect emitted events via the public `events` field after calling token ops.
#[derive(Debug)]
pub struct InMemoryTokenAccounting {
    address: Address,
    /// Whether `is_initialized` returns `true`.
    pub initialized: bool,
    /// Per-account token balances.
    pub balances: HashMap<Address, U256>,
    /// Approved spending allowances keyed by `(owner, spender)`.
    pub allowances: HashMap<(Address, Address), U256>,
    /// Current total token supply.
    pub total_supply: U256,
    /// Defaults to `U256::MAX` so mint tests don't need to set a cap explicitly.
    pub supply_cap: U256,
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Number of decimal places.
    pub decimals: u8,
    /// Bitmask of active pause vectors.
    pub paused: U256,
    /// Per-account EIP-2612 nonces.
    pub nonces: HashMap<Address, U256>,
    /// Minimum amount required for a redeem operation.
    pub minimum_redeemable: U256,
    /// URI pointing to the contract-level metadata.
    pub contract_uri: String,
    /// Capability bitfield.
    pub capabilities: U256,
    /// Events collected by `emit_event`; does not produce real EVM logs.
    pub events: Vec<LogData>,
}

impl InMemoryTokenAccounting {
    /// Creates an initialized accounting instance at `address` with sensible defaults.
    pub fn new(address: Address) -> Self {
        Self {
            address,
            initialized: true,
            balances: HashMap::new(),
            allowances: HashMap::new(),
            total_supply: U256::ZERO,
            supply_cap: U256::MAX,
            name: String::new(),
            symbol: String::new(),
            decimals: 18,
            paused: U256::ZERO,
            nonces: HashMap::new(),
            minimum_redeemable: U256::ZERO,
            contract_uri: String::new(),
            capabilities: U256::ZERO,
            events: Vec::new(),
        }
    }
}

impl TokenAccounting for InMemoryTokenAccounting {
    fn token_address(&self) -> Address {
        self.address
    }

    fn is_initialized(&self) -> Result<bool> {
        Ok(self.initialized)
    }

    fn balance_of(&self, account: Address) -> Result<U256> {
        Ok(*self.balances.get(&account).unwrap_or(&U256::ZERO))
    }

    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        self.balances.insert(account, balance);
        Ok(())
    }

    fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        Ok(*self.allowances.get(&(owner, spender)).unwrap_or(&U256::ZERO))
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        self.allowances.insert((owner, spender), amount);
        Ok(())
    }

    fn total_supply(&self) -> Result<U256> {
        Ok(self.total_supply)
    }

    fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.total_supply = supply;
        Ok(())
    }

    fn supply_cap(&self) -> Result<U256> {
        Ok(self.supply_cap)
    }

    fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.supply_cap = cap;
        Ok(())
    }

    fn name(&self) -> Result<String> {
        Ok(self.name.clone())
    }

    fn set_name(&mut self, name: String) -> Result<()> {
        self.name = name;
        Ok(())
    }

    fn symbol(&self) -> Result<String> {
        Ok(self.symbol.clone())
    }

    fn set_symbol(&mut self, symbol: String) -> Result<()> {
        self.symbol = symbol;
        Ok(())
    }

    fn decimals(&self) -> Result<u8> {
        Ok(self.decimals)
    }

    fn paused(&self) -> Result<U256> {
        Ok(self.paused)
    }

    fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.paused = vectors;
        Ok(())
    }

    fn nonce(&self, owner: Address) -> Result<U256> {
        Ok(*self.nonces.get(&owner).unwrap_or(&U256::ZERO))
    }

    fn increment_nonce(&mut self, owner: Address) -> Result<()> {
        let n = self.nonces.entry(owner).or_default();
        *n += U256::from(1u64);
        Ok(())
    }

    fn minimum_redeemable(&self) -> Result<U256> {
        Ok(self.minimum_redeemable)
    }

    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()> {
        self.minimum_redeemable = minimum;
        Ok(())
    }

    fn contract_uri(&self) -> Result<String> {
        Ok(self.contract_uri.clone())
    }

    fn set_contract_uri(&mut self, uri: String) -> Result<()> {
        self.contract_uri = uri;
        Ok(())
    }

    fn capabilities(&self) -> Result<U256> {
        Ok(self.capabilities)
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.events.push(log);
        Ok(())
    }
}

/// Lookup-table-backed [`Policy`] for unit tests.
///
/// Call [`InMemoryPolicy::allow`] to grant authorization before exercising token ops.
/// Missing entries default to `false`.
#[derive(Debug, Default)]
pub struct InMemoryPolicy {
    /// Authorization grants keyed by `(policy_id, account)`.
    pub authorizations: HashMap<(u64, Address), bool>,
}

impl InMemoryPolicy {
    /// Creates an empty policy with no authorizations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `account` as authorized under `policy_id`.
    pub fn allow(&mut self, policy_id: u64, account: Address) {
        self.authorizations.insert((policy_id, account), true);
    }
}

impl Policy for InMemoryPolicy {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        Ok(*self.authorizations.get(&(policy_id, account)).unwrap_or(&false))
    }
}
