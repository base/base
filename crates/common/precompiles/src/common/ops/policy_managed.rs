//! Policy scope identifiers and policy-ID administration for B-20 tokens.

use alloy_primitives::{Address, B256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20Guards, B20PolicyType, B20TokenRole, IB20, Policy, Token, TokenAccounting,
};

/// Policy slot accessors and `policyId` / `updatePolicy` administration.
///
/// All methods have default implementations. Implement with an empty body to opt in.
/// Security tokens override [`Self::supports_policy_scope`] to admit security-only slots
/// such as [`B20PolicyType::RedeemSender`].
pub trait PolicyManaged: Token
where
    Self::Accounting: TokenAccounting,
{
    /// Returns whether `policy_scope` is valid for this token variant.
    fn supports_policy_scope(policy_scope: B256) -> bool {
        B20PolicyType::from_id(policy_scope).is_some_and(B20PolicyType::is_core)
    }

    /// Policy slot checked against transfer senders.
    fn transfer_sender_policy() -> B256 {
        B20PolicyType::TransferSender.id()
    }

    /// Policy slot checked against transfer receivers.
    fn transfer_receiver_policy() -> B256 {
        B20PolicyType::TransferReceiver.id()
    }

    /// Policy slot checked against delegated transfer executors.
    fn transfer_executor_policy() -> B256 {
        B20PolicyType::TransferExecutor.id()
    }

    /// Policy slot checked against mint receivers.
    fn mint_receiver_policy() -> B256 {
        B20PolicyType::MintReceiver.id()
    }

    /// Ensures `policy_scope` names a supported policy slot for this variant.
    fn ensure_supported_policy_type(policy_scope: B256) -> Result<()> {
        if Self::supports_policy_scope(policy_scope) {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }

    /// Returns the configured policy ID for `policy_scope`.
    fn policy_id(&self, policy_scope: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        self.accounting().policy_id(policy_scope)
    }

    /// Updates the configured policy ID for `policy_scope`.
    fn update_policy(
        &mut self,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::DefaultAdmin)?;
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
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use base_precompile_storage::BasePrecompileError;

    use crate::{
        B20PolicyType, B20TokenRole, IB20, InMemoryPolicy, InMemoryTokenAccounting, PolicyManaged,
        TestToken, Token, TokenAccounting,
    };

    const ADMIN: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(0x20);
    const CUSTOM_POLICY_ID: u64 = 7;

    fn token() -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((B20TokenRole::DefaultAdmin.id(), ADMIN), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn policy_id_reverts_for_unsupported_policy_type() {
        let token = token();
        let policy_scope = B256::repeat_byte(0x99);

        assert_eq!(
            token.policy_id(policy_scope).unwrap_err(),
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: policy_scope })
        );
    }

    #[test]
    fn update_policy_reverts_for_missing_policy_id() {
        let mut token = token();

        assert_eq!(
            token
                .update_policy(ADMIN, B20PolicyType::TransferSender.id(), CUSTOM_POLICY_ID, false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyNotFound { policyId: CUSTOM_POLICY_ID })
        );
    }

    #[test]
    fn update_policy_accepts_existing_policy_id() {
        let mut token = token();
        token.policy_mut().create_existing_policy(CUSTOM_POLICY_ID);

        token
            .update_policy(ADMIN, B20PolicyType::TransferSender.id(), CUSTOM_POLICY_ID, false)
            .unwrap();

        assert_eq!(
            token.accounting().policy_id(B20PolicyType::TransferSender.id()).unwrap(),
            CUSTOM_POLICY_ID
        );
    }
}
