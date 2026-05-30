//! `B20SecurityToken` struct — the security B-20 token type.

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use crate::{
    B20Guards, B20PolicyType, B20TokenRole, Burnable, Configurable, IB20, Mintable, Pausable,
    Permittable, Policy, RoleManaged, SecurityAccounting, SecurityManagement, Token, Transferable,
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

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Policy slot checked against transfer senders.
    pub const fn transfer_sender_policy() -> B256 {
        B20PolicyType::TransferSender.id()
    }

    /// Policy slot checked against transfer receivers.
    pub const fn transfer_receiver_policy() -> B256 {
        B20PolicyType::TransferReceiver.id()
    }

    /// Policy slot checked against delegated transfer executors.
    pub const fn transfer_executor_policy() -> B256 {
        B20PolicyType::TransferExecutor.id()
    }

    /// Policy slot checked against mint receivers.
    pub const fn mint_receiver_policy() -> B256 {
        B20PolicyType::MintReceiver.id()
    }

    /// Returns the configured policy ID for `policy_scope`.
    pub fn policy_id(&self, policy_scope: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        self.accounting().policy_id(policy_scope)
    }

    /// Updates the configured policy ID for `policy_scope`.
    pub fn update_policy(
        &mut self,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(self, caller, B20TokenRole::DefaultAdmin)?;
        }
        let old_policy_id = self.policy_id(policy_scope)?;
        if !self.policy().policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        self.accounting_mut().set_policy_id(policy_scope, new_policy_id)?;
        self.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_scope,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    /// Ensures `policy_scope` names an inherited B-20 policy slot or the security
    /// redeem slot.
    pub fn ensure_supported_policy_type(policy_scope: B256) -> Result<()> {
        if policy_scope == Self::REDEEM_SENDER_POLICY
            || B20PolicyType::from_id(policy_scope).is_some()
        {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use crate::{B20SecurityToken, InMemoryPolicy, InMemoryTokenAccounting, SecurityManagement};

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    #[test]
    fn role_and_policy_ids_match_solidity_hashes() {
        assert_eq!(TestSecurityToken::SECURITY_OPERATOR_ROLE, keccak256("SECURITY_OPERATOR_ROLE"));
        assert_eq!(TestSecurityToken::BURN_FROM_ROLE, keccak256("BURN_FROM_ROLE"));
        assert_eq!(TestSecurityToken::REDEEM_SENDER_POLICY, keccak256("REDEEM_SENDER_POLICY"));
    }
}
