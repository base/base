//! `PolicyAccounting` — storage port for the `PolicyRegistry` precompile.

use alloy_primitives::{Address, LogData, U256};
use base_precompile_storage::Result;

/// Raw storage port the policy-registry logic operates on.
pub trait PolicyAccounting {
    /// Returns the singleton registry address these slots are rooted at.
    fn registry_address(&self) -> Address;

    /// Returns the current call's caller address.
    fn caller(&self) -> Address;

    /// Reads the raw packed policy word for `policy_id` (`U256::ZERO` if never written).
    fn read_policy_word(&self, policy_id: u64) -> Result<U256>;

    /// Writes the raw packed policy word for `policy_id`.
    fn write_policy_word(&mut self, policy_id: u64, word: U256) -> Result<()>;

    /// Returns whether `account` is a recorded member of `policy_id`'s set.
    fn read_member(&self, policy_id: u64, account: Address) -> Result<bool>;

    /// Records `account` as a member of `policy_id`'s set.
    fn set_member(&mut self, policy_id: u64, account: Address) -> Result<()>;

    /// Removes `account` from `policy_id`'s set (zeroes the slot).
    fn delete_member(&mut self, policy_id: u64, account: Address) -> Result<()>;

    /// Reads the staged pending admin for `policy_id` (`Address::ZERO` if none).
    fn read_pending_admin(&self, policy_id: u64) -> Result<Address>;

    /// Stages `admin` as the pending admin for `policy_id`.
    fn write_pending_admin(&mut self, policy_id: u64, admin: Address) -> Result<()>;

    /// Clears the staged pending admin for `policy_id` (zeroes the slot).
    fn delete_pending_admin(&mut self, policy_id: u64) -> Result<()>;

    /// Reads the global monotonic policy-ID counter.
    fn read_next_counter(&self) -> Result<u64>;

    /// Writes the global monotonic policy-ID counter.
    fn write_next_counter(&mut self, counter: u64) -> Result<()>;

    /// Emits `log` as an EVM event from the registry address.
    fn emit_event(&mut self, log: LogData) -> Result<()>;

    /// Writes the registry's bytecode marker so subsequent storage writes are not pruned.
    fn mark_initialized(&mut self) -> Result<()>;
}
