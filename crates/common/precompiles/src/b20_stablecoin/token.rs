//! `B20StablecoinToken` struct — the stablecoin B-20 token type.

use alloy_primitives::{Address, B256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::accounting::StablecoinAccounting;
use crate::{
    B20Guards, B20PolicyType, B20TokenRole, Burnable, Configurable, IB20, Mintable, Pausable,
    Permittable, Policy, RoleManaged, Token, Transferable,
};

/// EVM precompile for the stablecoin B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: StablecoinAccounting`
/// so the dispatch layer can read `currency()` from stablecoin storage. All inherited
/// `IB20` capability traits are wired in identically.
#[derive(Debug, Clone)]
pub struct B20StablecoinToken<S: StablecoinAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
}

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// Creates a `B20StablecoinToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy }
    }
}

impl<S: StablecoinAccounting, P: Policy> Token for B20StablecoinToken<S, P> {
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

impl<S: StablecoinAccounting, P: Policy> Transferable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Mintable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Burnable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Pausable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Configurable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> Permittable for B20StablecoinToken<S, P> {}
impl<S: StablecoinAccounting, P: Policy> RoleManaged for B20StablecoinToken<S, P> {}

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
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
        self.accounting.policy_id(policy_scope)
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
        if !self.policy.policy_exists(new_policy_id)? {
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

    /// Ensures `policy_scope` names a B-20 policy slot.
    pub fn ensure_supported_policy_type(policy_scope: B256) -> Result<()> {
        if B20PolicyType::from_id(policy_scope).is_some() {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }
}
