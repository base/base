//! Policy trait — the outward-facing interface tokens consult for authorization decisions.

use alloy_primitives::Address;
use base_precompile_storage::Result;

/// Trait for checking whether a given account is authorized under a specific policy.
pub trait Policy {
    /// Returns `true` if `account` is authorized under the given `policy_id`.
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool>;
}
