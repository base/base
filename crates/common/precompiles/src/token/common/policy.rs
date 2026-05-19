
use alloy_primitives::Address;
use base_precompile_storage::Result;

pub trait Policy {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool>;
}