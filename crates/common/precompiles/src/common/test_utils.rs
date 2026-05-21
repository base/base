//! In-memory fakes of [`TokenAccounting`] and [`Policy`] for unit tests.
//!
//! Use these for capability/ops logic tests (Transferable, Mintable, …).
//! For factory, dispatch, and storage-layout tests keep the EVM harness.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_storage::Result;

use crate::{
    IPolicyRegistry, POLICY_ALWAYS_ALLOW, POLICY_ALWAYS_BLOCK, PolicyRegistry,
    b20::B20Token,
    b20_security::SecurityAccounting,
    b20_stablecoin::{B20StablecoinToken, StablecoinAccounting},
    common::{Policy, TokenAccounting},
};

/// Convenience alias: [`B20Token`] wired with both in-memory fakes.
///
/// Use this in unit tests instead of spelling out the full generic each time.
pub type TestToken = B20Token<InMemoryTokenAccounting, InMemoryPolicy>;

/// Convenience alias: [`B20StablecoinToken`] wired with both in-memory fakes.
///
/// Use this in unit tests instead of spelling out the full generic each time.
pub type TestStablecoinToken = B20StablecoinToken<InMemoryTokenAccounting, InMemoryPolicy>;

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
    /// Stablecoin currency identifier.
    pub currency: String,
    /// Security ISIN identifier (legacy field; prefer `security_identifiers` map for security tokens).
    pub security_isin: String,
    /// Bitmask of active pause vectors.
    pub paused: U256,
    /// Per-account EIP-2612 nonces.
    pub nonces: HashMap<Address, U256>,
    /// Minimum amount required for a redeem operation.
    pub minimum_redeemable: U256,
    /// URI pointing to the contract-level metadata.
    pub contract_uri: String,
    /// Role membership keyed by `(role, account)`.
    pub roles: HashMap<(B256, Address), bool>,
    /// Number of accounts assigned to each role.
    pub role_member_counts: HashMap<B256, U256>,
    /// Admin role for each role.
    pub role_admins: HashMap<B256, B256>,
    /// Policy IDs keyed by policy type.
    pub policy_ids: HashMap<B256, u64>,
    /// Share-to-tokens ratio scaled to WAD (1e18). Security tokens only.
    pub shares_to_tokens_ratio: U256,
    /// Security identifier values keyed by `keccak256(identifier_type)`. Security tokens only.
    pub security_identifiers: HashMap<B256, String>,
    /// Consumed announcement ids (stored as `keccak256(id)`). Security tokens only.
    pub announcement_ids_used: HashSet<B256>,
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
            currency: String::new(),
            security_isin: String::new(),
            paused: U256::ZERO,
            nonces: HashMap::new(),
            minimum_redeemable: U256::ZERO,
            contract_uri: String::new(),
            roles: HashMap::new(),
            role_member_counts: HashMap::new(),
            role_admins: HashMap::new(),
            policy_ids: HashMap::new(),
            shares_to_tokens_ratio: U256::ZERO,
            security_identifiers: HashMap::new(),
            announcement_ids_used: HashSet::new(),
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

    fn currency(&self) -> Result<String> {
        Ok(self.currency.clone())
    }

    fn security_identifier(&self, identifier_type: &str) -> Result<String> {
        let key = alloy_primitives::keccak256(identifier_type.as_bytes());
        if let Some(val) = self.security_identifiers.get(&key) {
            return Ok(val.clone());
        }
        if identifier_type == "ISIN" { Ok(self.security_isin.clone()) } else { Ok(String::new()) }
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

    fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        Ok(*self.roles.get(&(role, account)).unwrap_or(&false))
    }

    fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
        self.roles.insert((role, account), enabled);
        Ok(())
    }

    fn role_member_count(&self, role: B256) -> Result<U256> {
        Ok(*self.role_member_counts.get(&role).unwrap_or(&U256::ZERO))
    }

    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
        self.role_member_counts.insert(role, count);
        Ok(())
    }

    fn role_admin(&self, role: B256) -> Result<B256> {
        Ok(*self.role_admins.get(&role).unwrap_or(&B256::ZERO))
    }

    fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
        self.role_admins.insert(role, admin_role);
        Ok(())
    }

    fn policy_id(&self, policy_type: B256) -> Result<u64> {
        Ok(*self.policy_ids.get(&policy_type).unwrap_or(&POLICY_ALWAYS_ALLOW))
    }

    fn set_policy_id(&mut self, policy_type: B256, policy_id: u64) -> Result<()> {
        self.policy_ids.insert(policy_type, policy_id);
        Ok(())
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.events.push(log);
        Ok(())
    }
}

impl StablecoinAccounting for InMemoryTokenAccounting {
    fn set_currency(&mut self, currency: String) -> Result<()> {
        self.currency = currency;
        Ok(())
    }
}

/// Lookup-table-backed [`Policy`] for unit tests.
///
/// Call [`InMemoryPolicy::allow`] to grant authorization before exercising token ops.
/// Missing entries default to `false`.
#[derive(Debug)]
pub struct InMemoryPolicy {
    /// Authorization grants keyed by `(policy_id, account)`.
    pub authorizations: HashMap<(u64, Address), bool>,
    /// Policy IDs that should be treated as existing.
    pub policies: HashSet<u64>,
    /// Next custom policy counter for tests that exercise registry creation.
    pub next_policy_counter: u64,
}

impl Default for InMemoryPolicy {
    fn default() -> Self {
        Self { authorizations: HashMap::new(), policies: HashSet::new(), next_policy_counter: 2 }
    }
}

impl InMemoryPolicy {
    /// Creates an empty policy with no authorizations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `account` as authorized under `policy_id`.
    pub fn allow(&mut self, policy_id: u64, account: Address) {
        self.policies.insert(policy_id);
        self.authorizations.insert((policy_id, account), true);
    }

    /// Marks `policy_id` as an existing policy without granting any account.
    pub fn create_existing_policy(&mut self, policy_id: u64) {
        self.policies.insert(policy_id);
    }
}

impl Policy for InMemoryPolicy {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        match policy_id {
            POLICY_ALWAYS_ALLOW => Ok(true),
            POLICY_ALWAYS_BLOCK => Ok(false),
            _ => Ok(*self.authorizations.get(&(policy_id, account)).unwrap_or(&false)),
        }
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        Ok(policy_id == POLICY_ALWAYS_ALLOW
            || policy_id == POLICY_ALWAYS_BLOCK
            || self.policies.contains(&policy_id))
    }
}

impl PolicyRegistry for InMemoryPolicy {
    fn create_policy(
        &mut self,
        _admin: Address,
        policy_type: IPolicyRegistry::PolicyType,
    ) -> Result<u64> {
        let policy_id = (policy_type as u64) << 56 | self.next_policy_counter;
        self.next_policy_counter += 1;
        self.policies.insert(policy_id);
        Ok(policy_id)
    }

    fn create_policy_with_accounts(
        &mut self,
        admin: Address,
        policy_type: IPolicyRegistry::PolicyType,
        accounts: Vec<Address>,
    ) -> Result<u64> {
        let policy_id = self.create_policy(admin, policy_type)?;
        for account in accounts {
            self.allow(policy_id, account);
        }
        Ok(policy_id)
    }

    fn stage_update_admin(&mut self, _policy_id: u64, _new_admin: Address) -> Result<()> {
        Ok(())
    }

    fn finalize_update_admin(&mut self, _policy_id: u64) -> Result<()> {
        Ok(())
    }

    fn renounce_admin(&mut self, _policy_id: u64) -> Result<()> {
        Ok(())
    }

    fn update_allowlist(
        &mut self,
        policy_id: u64,
        allowed: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        self.policies.insert(policy_id);
        for account in accounts {
            self.authorizations.insert((policy_id, account), allowed);
        }
        Ok(())
    }

    fn update_blocklist(
        &mut self,
        policy_id: u64,
        blocked: bool,
        accounts: Vec<Address>,
    ) -> Result<()> {
        self.policies.insert(policy_id);
        for account in accounts {
            self.authorizations.insert((policy_id, account), !blocked);
        }
        Ok(())
    }

    fn get_policy_type(&self, policy_id: u64) -> Result<IPolicyRegistry::PolicyType> {
        Ok(match policy_id {
            POLICY_ALWAYS_ALLOW => IPolicyRegistry::PolicyType::ALWAYS_ALLOW,
            POLICY_ALWAYS_BLOCK => IPolicyRegistry::PolicyType::ALWAYS_BLOCK,
            _ => IPolicyRegistry::PolicyType::try_from((policy_id >> 56) as u8).map_err(|_| {
                base_precompile_storage::BasePrecompileError::enum_conversion_error()
            })?,
        })
    }

    fn get_policy_admin(&self, _policy_id: u64) -> Result<Address> {
        Ok(Address::ZERO)
    }

    fn pending_policy_admin(&self, _policy_id: u64) -> Result<Address> {
        Ok(Address::ZERO)
    }
}

impl SecurityAccounting for InMemoryTokenAccounting {
    fn shares_to_tokens_ratio(&self) -> Result<U256> {
        Ok(self.shares_to_tokens_ratio)
    }

    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()> {
        self.shares_to_tokens_ratio = ratio;
        Ok(())
    }

    fn set_security_identifier_value(
        &mut self,
        identifier_type: &str,
        value: String,
    ) -> Result<()> {
        let key = alloy_primitives::keccak256(identifier_type.as_bytes());
        if value.is_empty() {
            self.security_identifiers.remove(&key);
        } else {
            self.security_identifiers.insert(key, value);
        }
        Ok(())
    }

    fn is_announcement_id_used(&self, id_hash: B256) -> Result<bool> {
        Ok(self.announcement_ids_used.contains(&id_hash))
    }

    fn mark_announcement_id_used(&mut self, id_hash: B256) -> Result<()> {
        self.announcement_ids_used.insert(id_hash);
        Ok(())
    }
}
