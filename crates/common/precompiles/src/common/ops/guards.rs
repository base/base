//! Shared authorization and policy guards for B-20 token operations.

use alloy_primitives::{Address, B256, U256};
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20PausableFeature, B20PolicyType, B20TokenRole, IB20, Policy, Token, TokenAccounting,
};

/// Authorization and policy guard helpers for B-20 operations.
#[derive(Debug, Clone, Copy)]
pub struct B20Guards;

impl B20Guards {
    /// Ensures `caller` has the B-20 role.
    pub fn ensure_token_role<T: Token + ?Sized>(
        token: &T,
        caller: Address,
        role: B20TokenRole,
    ) -> Result<()> {
        Self::ensure_role(token, caller, role.id())
    }

    /// Ensures `caller` has `role`.
    pub fn ensure_role<T: Token + ?Sized>(token: &T, caller: Address, role: B256) -> Result<()> {
        if token.accounting().has_role(role, caller)? {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: caller,
                neededRole: role,
            }))
        }
    }

    /// Ensures `feature` is not paused.
    pub fn ensure_not_paused<T: Token + ?Sized>(
        token: &T,
        feature: IB20::PausableFeature,
    ) -> Result<()> {
        if (token.accounting().paused()? & B20PausableFeature::mask(feature)) == U256::ZERO {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::ContractPaused { feature }))
        }
    }

    /// Ensures `account` is allowed by `policy_type`.
    pub fn ensure_policy_type<T: Token + ?Sized>(
        token: &T,
        policy_type: B20PolicyType,
        account: Address,
    ) -> Result<()> {
        Self::ensure_policy(token, policy_type.id(), account)
    }

    /// Ensures `account` is allowed by the raw `policy_scope`.
    ///
    /// All policy IDs, including built-ins, are delegated to the configured policy registry.
    pub fn ensure_policy<T: Token + ?Sized>(
        token: &T,
        policy_scope: B256,
        account: Address,
    ) -> Result<()> {
        let policy_id = token.accounting().policy_id(policy_scope)?;
        if token.policy().is_authorized(policy_id, account)? {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: policy_scope,
                policyId: policy_id,
            }))
        }
    }

    /// Ensures `account` is blocked by the current transfer-sender policy.
    ///
    /// Accounts are blocked when the configured registry policy does not authorize them.
    pub fn ensure_blocked<T: Token + ?Sized>(token: &T, account: Address) -> Result<()> {
        let policy_scope = B20PolicyType::TransferSender.id();
        let policy_id = token.accounting().policy_id(policy_scope)?;
        if token.policy().is_authorized(policy_id, account)? {
            Err(BasePrecompileError::revert(IB20::AccountNotBlocked { account }))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;
    use crate::{InMemoryPolicy, InMemoryTokenAccounting, PolicyRegistryStorage, TestToken};

    const EXTERNAL_POLICY_ID: u64 = 7;

    fn token_with_transfer_sender_policy(account: Address) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting.policy_ids.insert(B20PolicyType::TransferSender.id(), EXTERNAL_POLICY_ID);

        let mut policy = InMemoryPolicy::new();
        policy.allow(EXTERNAL_POLICY_ID, account);

        TestToken::with_storage_and_policy(accounting, policy)
    }

    #[test]
    fn test_ensure_policy_delegates_external_policy_ids_to_registry() {
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_transfer_sender_policy(allowed);

        B20Guards::ensure_policy_type(&token, B20PolicyType::TransferSender, allowed).unwrap();

        assert_eq!(
            B20Guards::ensure_policy_type(&token, B20PolicyType::TransferSender, denied)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::TransferSender.id(),
                policyId: EXTERNAL_POLICY_ID,
            })
        );
    }

    #[test]
    fn test_ensure_blocked_uses_external_policy_authorization() {
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_transfer_sender_policy(allowed);

        assert_eq!(
            B20Guards::ensure_blocked(&token, allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account: allowed })
        );
        B20Guards::ensure_blocked(&token, denied).unwrap();
    }

    #[test]
    fn test_ensure_blocked_preserves_global_block_semantics() {
        let account = Address::repeat_byte(0xaa);
        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        B20Guards::ensure_blocked(&token, account).unwrap();

        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        let token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            B20Guards::ensure_blocked(&token, account).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account })
        );
    }
}
