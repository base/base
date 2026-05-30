//! `B20SecurityToken` struct — the security B-20 token type.

use alloy_primitives::{Address, B256, Bytes};
use base_precompile_storage::{Result, StorageCtx};

use crate::{
    B20PolicyType, Burnable, Configurable, Mintable, Pausable, Permittable, Policy, PolicyManaged,
    RoleManaged, SecurityAccounting, SecurityManagement, Token, Transferable,
};

/// EVM precompile for the security B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: SecurityAccounting`
/// so the dispatch layer can read and write security-specific storage (share ratio,
/// security identifiers, announcement IDs). The `in_announcement` flag guards against
/// recursive `announce` calls within a single precompile invocation.
#[derive(Debug, Clone)]
pub struct B20SecurityToken<S: SecurityAccounting, P: Policy> {
    accounting: S,
    policy: P,
    in_announcement: bool,
}

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Creates a `B20SecurityToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy, in_announcement: false }
    }
}

impl<S: SecurityAccounting, P: Policy> Token for B20SecurityToken<S, P> {
    type Accounting = S;
    type Policy = P;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &P {
        &self.policy
    }

    fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}

impl<S: SecurityAccounting, P: Policy> Transferable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Mintable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Burnable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Pausable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Configurable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Permittable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> RoleManaged for B20SecurityToken<S, P> {}

impl<S: SecurityAccounting, P: Policy> PolicyManaged for B20SecurityToken<S, P> {
    fn supports_policy_scope(policy_scope: B256) -> bool {
        B20PolicyType::from_id(policy_scope).is_some()
    }
}

impl<S: SecurityAccounting, P: Policy> SecurityManagement for B20SecurityToken<S, P> {
    fn is_announcement_active(&self) -> bool {
        self.in_announcement
    }

    fn begin_announcement(&mut self) {
        self.in_announcement = true;
    }

    fn dispatch_internal_call(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> Result<Bytes> {
        self.inner_with_privilege(ctx, calldata, privileged)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use crate::{B20PolicyType, B20TokenRole};

    #[test]
    fn role_and_policy_ids_match_solidity_hashes() {
        assert_eq!(B20TokenRole::SecurityOperator.id(), keccak256("SECURITY_OPERATOR_ROLE"));
        assert_eq!(B20TokenRole::BurnFrom.id(), keccak256("BURN_FROM_ROLE"));
        assert_eq!(B20PolicyType::RedeemSender.id(), keccak256("REDEEM_SENDER_POLICY"));
    }
}
