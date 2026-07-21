//! In-memory fakes of [`TokenAccounting`] and [`PolicyAccounting`] for unit tests.
//!
//! Use these for capability/ops logic tests (Transferable, Mintable, …).
//! For factory, dispatch, and storage-layout tests keep the EVM harness.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::{Address, Address as TokenAddress, B256, LogData, U256};
use base_precompile_storage::Result;

use crate::{
    Burnable, Configurable, Mintable, PackedPolicy, Pausable, Permittable, PolicyAccounting,
    PolicyRegistryLogic, PolicyRegistryStorage, PolicyVersion, RoleManaged, Token, Transferable,
    b20_asset::{AssetAccounting, B20AssetStorage},
    b20_stablecoin::{B20StablecoinToken, StablecoinAccounting},
    common::{B20_MAX_SUPPLY_CAP, TokenAccounting},
};

/// Convenience alias: [`B20StablecoinToken`] wired with both in-memory fakes.
///
/// The stablecoin holder is a minimal storage+policy holder (its behavior lives in `logic/vN`), so
/// this alias is used by dispatch/`inner` tests, not for calling capability-trait methods directly.
pub type TestStablecoinToken = B20StablecoinToken<InMemoryTokenAccounting, FakePolicyAccounting>;

/// Concrete test token that opts into the shared capability traits over the in-memory fakes.
///
/// The production holders ([`crate::B20AssetToken`], [`crate::B20StablecoinToken`]) are now minimal
/// storage+policy holders whose behavior lives entirely in their versioned `logic/vN`
/// implementations, so they no longer implement the [`Transferable`]/[`Mintable`]/… capability
/// traits. This type keeps those shared traits exercised by the `common::ops` unit tests without
/// depending on any token variant.
#[derive(Debug)]
pub struct TestToken {
    accounting: InMemoryTokenAccounting,
    policy: FakePolicyAccounting,
    policy_version: PolicyVersion,
}

impl TestToken {
    /// Creates a test token backed by the provided in-memory fakes at [`PolicyVersion::V1`].
    pub const fn with_storage_and_policy(
        accounting: InMemoryTokenAccounting,
        policy: FakePolicyAccounting,
    ) -> Self {
        Self { accounting, policy, policy_version: PolicyVersion::V1 }
    }
}

impl Token for TestToken {
    type Accounting = InMemoryTokenAccounting;
    type PolicyAccounting = FakePolicyAccounting;

    fn accounting(&self) -> &InMemoryTokenAccounting {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut InMemoryTokenAccounting {
        &mut self.accounting
    }

    fn policy(&self) -> &dyn PolicyRegistryLogic<FakePolicyAccounting> {
        self.policy_version.implementation()
    }

    fn policy_storage(&self) -> &FakePolicyAccounting {
        &self.policy
    }

    fn policy_storage_mut(&mut self) -> &mut FakePolicyAccounting {
        &mut self.policy
    }

    fn token_address(&self) -> TokenAddress {
        self.accounting.token_address()
    }
}

impl Transferable for TestToken {}
impl Mintable for TestToken {}
impl Burnable for TestToken {}
impl Pausable for TestToken {}
impl Configurable for TestToken {}
impl Permittable for TestToken {}
impl RoleManaged for TestToken {}

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
    /// Defaults to [`B20_MAX_SUPPLY_CAP`] so mint tests don't need to set a cap explicitly.
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
            supply_cap: B20_MAX_SUPPLY_CAP,
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

/// In-memory [`PolicyAccounting`] for unit tests.
///
/// Pair with [`PolicyVersion::V1`] on tokens so authorization goes through
/// [`crate::PolicyRegistryLogic`]. Call [`FakePolicyAccounting::allow`] to grant membership
/// (ALLOWLIST semantics under V1) before exercising token ops that need a custom policy.
#[derive(Debug)]
pub struct FakePolicyAccounting {
    caller: Address,
    initialized: bool,
    policies: BTreeMap<u64, U256>,
    members: BTreeMap<(u64, Address), bool>,
    pending_admins: BTreeMap<u64, Address>,
    next_counter: u64,
    events: Vec<LogData>,
}

impl Default for FakePolicyAccounting {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePolicyAccounting {
    /// Creates empty policy-registry storage (no built-ins seeded).
    pub const fn new() -> Self {
        Self {
            caller: Address::ZERO,
            initialized: false,
            policies: BTreeMap::new(),
            members: BTreeMap::new(),
            pending_admins: BTreeMap::new(),
            next_counter: 0,
            events: Vec::new(),
        }
    }

    /// Marks `account` as a member of `policy_id` and records the policy as existing.
    ///
    /// For V1 ALLOWLIST policy IDs this authorizes `account`; for BLOCKLIST IDs it blocks them.
    pub fn allow(&mut self, policy_id: u64, account: Address) {
        self.create_existing_policy(policy_id);
        self.members.insert((policy_id, account), true);
    }

    /// Marks `policy_id` as an existing policy without granting any account.
    pub fn create_existing_policy(&mut self, policy_id: u64) {
        self.policies.insert(policy_id, PackedPolicy::new(Address::ZERO).into_u256());
    }
}

impl PolicyAccounting for FakePolicyAccounting {
    fn registry_address(&self) -> Address {
        Address::repeat_byte(0x02)
    }

    fn caller(&self) -> Address {
        self.caller
    }

    fn read_policy_word(&self, policy_id: u64) -> Result<U256> {
        Ok(self.policies.get(&policy_id).copied().unwrap_or(U256::ZERO))
    }

    fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()> {
        self.policies.insert(policy_id, word);
        Ok(())
    }

    fn read_member(&self, policy_id: u64, account: Address) -> Result<bool> {
        Ok(self.members.get(&(policy_id, account)).copied().unwrap_or(false))
    }

    fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.members.insert((policy_id, account), true);
        Ok(())
    }

    fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()> {
        self.members.remove(&(policy_id, account));
        Ok(())
    }

    fn read_pending_admin(&self, policy_id: u64) -> Result<Address> {
        Ok(self.pending_admins.get(&policy_id).copied().unwrap_or(Address::ZERO))
    }

    fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()> {
        self.pending_admins.insert(policy_id, admin);
        Ok(())
    }

    fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()> {
        self.pending_admins.remove(&policy_id);
        Ok(())
    }

    fn read_next_counter(&self) -> Result<u64> {
        Ok(self.next_counter)
    }

    fn write_next_counter(&mut self, counter: u64) -> Result<()> {
        self.next_counter = counter;
        Ok(())
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.events.push(log);
        Ok(())
    }

    fn mark_initialized(&mut self) -> Result<()> {
        self.initialized = true;
        Ok(())
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
