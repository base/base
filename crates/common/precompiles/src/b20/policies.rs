//! Policy helpers for B-20 tokens.

use alloy_primitives::{Address, B256, b256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::token::B20Token;
use crate::{B20Guards, B20TokenRole, IB20, Policy, Token, TokenAccounting};

/// Built-in policy ID that authorizes every account.
pub const POLICY_ALWAYS_ALLOW: u64 = 0;
/// Built-in policy ID that authorizes no account.
pub const POLICY_ALWAYS_BLOCK: u64 = 1;

const TRANSFER_SENDER_POLICY: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");
const TRANSFER_RECEIVER_POLICY: B256 =
    b256!("8a4b3fa2d8b921852bc0089c6ef0958aa6961897be36fd731330fe2cd23f8363");
const TRANSFER_EXECUTOR_POLICY: B256 =
    b256!("10be5173aff2a44e748bd9acd8b19fe34689581398a9db7ba2fb671e786ff7d8");
const MINT_RECEIVER_POLICY: B256 =
    b256!("a0d5ae037e66a09119acf080a1d807abb9b6d03b6b9130eb19f7c1e6bdb8ffc8");

/// Built-in B-20 policy slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20PolicyType {
    /// Policy slot checked against transfer senders.
    TransferSender,
    /// Policy slot checked against transfer receivers.
    TransferReceiver,
    /// Policy slot checked against delegated transfer executors.
    TransferExecutor,
    /// Policy slot checked against mint receivers.
    MintReceiver,
}

impl B20PolicyType {
    /// Returns the built-in policy type for `id`, if it is recognized.
    pub fn from_id(id: B256) -> Option<Self> {
        if id == TRANSFER_SENDER_POLICY {
            Some(Self::TransferSender)
        } else if id == TRANSFER_RECEIVER_POLICY {
            Some(Self::TransferReceiver)
        } else if id == TRANSFER_EXECUTOR_POLICY {
            Some(Self::TransferExecutor)
        } else if id == MINT_RECEIVER_POLICY {
            Some(Self::MintReceiver)
        } else {
            None
        }
    }

    /// Returns the policy type identifier.
    pub const fn id(self) -> B256 {
        match self {
            Self::TransferSender => TRANSFER_SENDER_POLICY,
            Self::TransferReceiver => TRANSFER_RECEIVER_POLICY,
            Self::TransferExecutor => TRANSFER_EXECUTOR_POLICY,
            Self::MintReceiver => MINT_RECEIVER_POLICY,
        }
    }
}

impl<S: TokenAccounting, P: Policy> B20Token<S, P> {
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

    /// Returns the configured policy ID for `policy_type`.
    pub fn policy_id(&self, policy_type: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_type)?;
        self.accounting.policy_id(policy_type)
    }

    /// Updates the configured policy ID for `policy_type`.
    pub fn update_policy(
        &mut self,
        caller: Address,
        policy_type: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role(self, caller, B20TokenRole::DefaultAdmin)?;
        }
        let old_policy_id = self.policy_id(policy_type)?;
        if !self.policy.policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        self.accounting_mut().set_policy_id(policy_type, new_policy_id)?;
        self.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyType: policy_type,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    /// Returns whether `policy_id` is one of the built-in global policies.
    pub const fn is_builtin_policy(policy_id: u64) -> bool {
        policy_id == POLICY_ALWAYS_ALLOW || policy_id == POLICY_ALWAYS_BLOCK
    }

    /// Ensures `policy_type` names a B-20 policy slot.
    pub fn ensure_supported_policy_type(policy_type: B256) -> Result<()> {
        if B20PolicyType::from_id(policy_type).is_some() {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyType: policy_type,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use super::*;
    use crate::{B20TokenRole, InMemoryPolicy, InMemoryTokenAccounting, Token, TokenAccounting};

    const ADMIN: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(0x20);
    const CUSTOM_POLICY_ID: u64 = 7;

    fn token() -> B20Token<InMemoryTokenAccounting, InMemoryPolicy> {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((B20TokenRole::DefaultAdmin.id(), ADMIN), true);
        B20Token::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn policy_id_reverts_for_unsupported_policy_type() {
        let token = token();
        let policy_type = B256::repeat_byte(0x99);

        assert_eq!(
            token.policy_id(policy_type).unwrap_err(),
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyType: policy_type })
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
        token.policy.create_existing_policy(CUSTOM_POLICY_ID);

        token
            .update_policy(ADMIN, B20PolicyType::TransferSender.id(), CUSTOM_POLICY_ID, false)
            .unwrap();

        assert_eq!(
            token.accounting().policy_id(B20PolicyType::TransferSender.id()).unwrap(),
            CUSTOM_POLICY_ID
        );
    }
}
