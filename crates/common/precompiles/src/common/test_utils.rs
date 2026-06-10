//! In-memory fakes of [`TokenAccounting`] and [`Policy`] for unit tests.
//!
//! Use these for capability/ops logic tests (Transferable, Mintable, …).
//! For factory, dispatch, and storage-layout tests keep the EVM harness.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_storage::Result;

use crate::{
    IPolicyRegistry, PolicyRegistry, PolicyRegistryStorage,
    b20_asset::{AssetAccounting, B20AssetStorage, B20AssetToken},
    b20_stablecoin::{B20StablecoinToken, StablecoinAccounting},
    common::{Policy, TokenAccounting},
};

/// Convenience alias: [`B20AssetToken`] wired with both in-memory fakes.
pub type TestToken = B20AssetToken<InMemoryTokenAccounting, InMemoryPolicy>;

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
    /// Bitmask of active pause vectors.
    pub paused: U256,
    /// Per-account EIP-2612 nonces.
    pub nonces: HashMap<Address, U256>,
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
    /// Multiplier scaled to WAD (1e18). Asset tokens only.
    pub multiplier: U256,
    /// Extra-metadata values keyed by raw metadata `key`. Asset tokens only.
    pub extra_metadata: HashMap<String, String>,
    /// Consumed announcement ids keyed by raw announcement id. Asset tokens only.
    pub announcement_ids_used: HashSet<String>,
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
            paused: U256::ZERO,
            nonces: HashMap::new(),
            contract_uri: String::new(),
            roles: HashMap::new(),
            role_member_counts: HashMap::new(),
            role_admins: HashMap::new(),
            policy_ids: HashMap::new(),
            multiplier: U256::ZERO,
            extra_metadata: HashMap::new(),
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

    fn policy_id(&self, policy_scope: B256) -> Result<u64> {
        Ok(*self.policy_ids.get(&policy_scope).unwrap_or(&PolicyRegistryStorage::ALWAYS_ALLOW_ID))
    }

    fn set_policy_id(&mut self, policy_scope: B256, policy_id: u64) -> Result<()> {
        self.policy_ids.insert(policy_scope, policy_id);
        Ok(())
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.events.push(log);
        Ok(())
    }
}

impl StablecoinAccounting for InMemoryTokenAccounting {
    fn currency(&self) -> Result<String> {
        Ok(self.currency.clone())
    }

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
            PolicyRegistryStorage::ALWAYS_ALLOW_ID => Ok(true),
            PolicyRegistryStorage::ALWAYS_BLOCK_ID => Ok(false),
            _ => Ok(*self.authorizations.get(&(policy_id, account)).unwrap_or(&false)),
        }
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool> {
        Ok(policy_id == PolicyRegistryStorage::ALWAYS_ALLOW_ID
            || policy_id == PolicyRegistryStorage::ALWAYS_BLOCK_ID
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

    fn get_policy_admin(&self, _policy_id: u64) -> Result<Address> {
        Ok(Address::ZERO)
    }

    fn pending_policy_admin(&self, _policy_id: u64) -> Result<Address> {
        Ok(Address::ZERO)
    }
}

impl AssetAccounting for InMemoryTokenAccounting {
    fn multiplier(&self) -> Result<U256> {
        Ok(if self.multiplier.is_zero() { B20AssetStorage::WAD } else { self.multiplier })
    }

    fn set_multiplier(&mut self, ratio: U256) -> Result<()> {
        self.multiplier = ratio;
        Ok(())
    }

    fn extra_metadata(&self, key: &str) -> Result<String> {
        Ok(self.extra_metadata.get(key).cloned().unwrap_or_default())
    }

    fn set_extra_metadata_value(&mut self, key: &str, value: String) -> Result<()> {
        if value.is_empty() {
            self.extra_metadata.remove(key);
        } else {
            self.extra_metadata.insert(key.to_owned(), value);
        }
        Ok(())
    }

    fn is_announcement_id_used(&self, id: &str) -> Result<bool> {
        Ok(self.announcement_ids_used.contains(id))
    }

    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()> {
        self.announcement_ids_used.insert(id.to_owned());
        Ok(())
    }

    fn decimals(&self) -> Result<u8> {
        Ok(if self.decimals == 0 { B20AssetStorage::MIN_DECIMALS } else { self.decimals })
    }
}
