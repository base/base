//! `TokenAccounting` — the driven port all token storage adapters implement.
use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_storage::Result;

/// Outbound port: all data reads and writes the core business logic requires.
///
/// Each token variant's `#[contract]` storage struct implements this trait.
/// Capability trait default implementations only depend on this interface, never on EVM storage
/// directly.
pub trait TokenAccounting {
    /// Returns the on-chain address backing this token's storage.
    fn token_address(&self) -> Address;

    /// Returns whether marker bytecode is deployed at this token's address.
    fn is_initialized(&self) -> Result<bool>;

    // --- Balances ---

    /// Returns the token balance of `account`.
    fn balance_of(&self, account: Address) -> Result<U256>;
    /// Overwrites the token balance of `account`.
    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()>;

    // --- Allowances ---

    /// Returns the allowance granted by `owner` to `spender`.
    fn allowance(&self, owner: Address, spender: Address) -> Result<U256>;
    /// Overwrites the allowance granted by `owner` to `spender`.
    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()>;

    // --- Supply ---

    /// Returns the total token supply currently in circulation.
    fn total_supply(&self) -> Result<U256>;
    /// Overwrites the total supply.
    fn set_total_supply(&mut self, supply: U256) -> Result<()>;
    /// Returns the maximum total supply enforced on mint.
    fn supply_cap(&self) -> Result<U256>;
    /// Overwrites the supply cap.
    fn set_supply_cap(&mut self, cap: U256) -> Result<()>;

    // --- Metadata ---

    /// Returns the token name.
    fn name(&self) -> Result<String>;
    /// Overwrites the token name.
    fn set_name(&mut self, name: String) -> Result<()>;
    /// Returns the token symbol.
    fn symbol(&self) -> Result<String>;
    /// Overwrites the token symbol.
    fn set_symbol(&mut self, symbol: String) -> Result<()>;
    /// Returns the number of decimal places.
    fn decimals(&self) -> Result<u8>;
    /// Returns the stablecoin currency identifier, or an empty string for non-stablecoin variants.
    fn currency(&self) -> Result<String>;
    /// Returns the security identifier value for `identifier_type`, or an empty string if unset.
    fn security_identifier(&self, identifier_type: &str) -> Result<String>;

    // --- Pause ---

    /// Returns the current paused-vector bitmask.
    fn paused(&self) -> Result<U256>;
    /// Overwrites the paused-vector bitmask.
    fn set_paused(&mut self, vectors: U256) -> Result<()>;

    // --- Permit nonces ---

    /// Returns the current EIP-2612 permit nonce for `owner`.
    fn nonce(&self, owner: Address) -> Result<U256>;
    /// Increments the EIP-2612 permit nonce for `owner` by one.
    fn increment_nonce(&mut self, owner: Address) -> Result<()>;

    // --- Redeem ---

    /// Returns the minimum amount that may be redeemed in a single call.
    fn minimum_redeemable(&self) -> Result<U256>;
    /// Overwrites the minimum redeemable amount.
    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()>;

    // --- Contract URI ---

    /// Returns the off-chain metadata URI for this token (ERC-7572).
    fn contract_uri(&self) -> Result<String>;
    /// Overwrites the contract URI.
    fn set_contract_uri(&mut self, uri: String) -> Result<()>;

    // --- Roles ---

    /// Returns whether `account` has `role`.
    fn has_role(&self, role: B256, account: Address) -> Result<bool>;
    /// Sets whether `account` has `role`.
    fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()>;
    /// Returns the number of accounts holding `role`.
    fn role_member_count(&self, role: B256) -> Result<U256>;
    /// Overwrites the number of accounts holding `role`.
    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()>;
    /// Returns the admin role for `role`.
    fn role_admin(&self, role: B256) -> Result<B256>;
    /// Overwrites the admin role for `role`.
    fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()>;

    // --- Policies ---

    /// Returns the policy ID assigned to `policy_type`.
    fn policy_id(&self, policy_type: B256) -> Result<u64>;
    /// Overwrites the policy ID assigned to `policy_type`.
    fn set_policy_id(&mut self, policy_type: B256, policy_id: u64) -> Result<()>;

    // --- Event emission ---

    /// Publishes a pre-encoded EVM event log from this token's address.
    fn emit_event(&mut self, log: LogData) -> Result<()>;
}
