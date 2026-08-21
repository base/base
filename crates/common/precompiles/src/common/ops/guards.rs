//! Shared authorization and policy guards for B-20 token operations.

use alloy_primitives::{Address, B256, U256};
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{B20PausableFeature, B20PolicyType, B20TokenRole, IB20, Token, TokenAccounting};

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
        if token.policy().is_authorized(token.policy_storage(), policy_id, account)? {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: policy_scope,
                policyId: policy_id,
            }))
        }
    }

    /// Ensures `account` is authorized by `policy_scope` using a pre-read `policy_id`.
    ///
    /// Identical to [`Self::ensure_policy`] except the caller supplies the id (already loaded, e.g.
    /// via [`TokenAccounting::transfer_policy_ids`](crate::TokenAccounting::transfer_policy_ids)),
    /// so a batch of checks against ids from the same slot pays a single SLOAD. Reverts the same
    /// `PolicyForbids { policyScope, policyId }` as `ensure_policy`.
    pub fn ensure_authorized_by_id<T: Token + ?Sized>(
        token: &T,
        policy_scope: B256,
        policy_id: u64,
        account: Address,
    ) -> Result<()> {
        if token.policy().is_authorized(token.policy_storage(), policy_id, account)? {
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
        if token.policy().is_authorized(token.policy_storage(), policy_id, account)? {
            Err(BasePrecompileError::revert(IB20::AccountNotBlocked { account }))
        } else {
            Ok(())
        }
    }

    /// Ensures `account` is seizable, i.e. a member of the current seize-holder policy.
    ///
    /// Mirrors [`Self::ensure_blocked`] but consults `SEIZE_HOLDER_POLICY` instead of the
    /// transfer-sender policy: an account is seizable only when the configured registry policy does
    /// not authorize it. Used by `seizeWithMemo`. Enforced unconditionally, including in the factory
    /// bootstrap window. Reverts `AccountNotSeizable` (distinct from `ensure_blocked`'s
    /// `AccountNotBlocked`) so the seize path and the deprecated `burnBlocked` report separately.
    pub fn ensure_seizable<T: Token + ?Sized>(token: &T, account: Address) -> Result<()> {
        let policy_scope = B20PolicyType::SeizeHolder.id();
        let policy_id = token.accounting().policy_id(policy_scope)?;
        if token.policy().is_authorized(token.policy_storage(), policy_id, account)? {
            Err(BasePrecompileError::revert(IB20::AccountNotSeizable { account }))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_precompile_storage::BasePrecompileError;

    use crate::{
        B20Guards, B20PolicyType, FakePolicyAccounting, IB20, InMemoryTokenAccounting,
        PolicyRegistryStorage, PolicyVersion, TestStablecoinToken,
    };

    const EXTERNAL_POLICY_ID: u64 = (1u64 << 56) | 7; // ALLOWLIST type + counter 7

    fn token_with_transfer_sender_policy(account: Address) -> TestStablecoinToken {
        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting.policy_ids.insert(B20PolicyType::TransferSender.id(), EXTERNAL_POLICY_ID);

        let mut policy = FakePolicyAccounting::new();
        policy.allow(EXTERNAL_POLICY_ID, account);

        TestStablecoinToken::with_storage_and_policy(accounting, policy, PolicyVersion::V1)
    }

    fn token_with_seizable_policy(account: Address) -> TestStablecoinToken {
        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting.policy_ids.insert(B20PolicyType::SeizeHolder.id(), EXTERNAL_POLICY_ID);

        let mut policy = FakePolicyAccounting::new();
        policy.allow(EXTERNAL_POLICY_ID, account);

        TestStablecoinToken::with_storage_and_policy(accounting, policy, PolicyVersion::V1)
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
    fn test_ensure_seizable_uses_external_policy_authorization() {
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_seizable_policy(allowed);

        // Authorized (allowed) under the seizable policy => not seizable => reverts.
        assert_eq!(
            B20Guards::ensure_seizable(&token, allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotSeizable { account: allowed })
        );
        // Not authorized (denied) => seizable => ok.
        B20Guards::ensure_seizable(&token, denied).unwrap();
    }

    #[test]
    fn test_ensure_seizable_is_independent_of_transfer_sender_policy() {
        // A token with the transfer-sender policy set must NOT make an account seizable: the
        // seizable scope is unset (ALWAYS_ALLOW), so every account is authorized => not seizable.
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_transfer_sender_policy(allowed);

        // `allowed` is authorized by transfer-sender AND by unset seizable (ALWAYS_ALLOW).
        assert_eq!(
            B20Guards::ensure_seizable(&token, allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotSeizable { account: allowed })
        );

        // `denied` is NOT authorized by transfer-sender, but still not seizable because the
        // seizable policy is unset (ALWAYS_ALLOW) — proving the two scopes are independent.
        assert_eq!(
            B20Guards::ensure_seizable(&token, denied).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotSeizable { account: denied })
        );
    }

    #[test]
    fn test_ensure_blocked_preserves_global_block_semantics() {
        let account = Address::repeat_byte(0xaa);
        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let token = TestStablecoinToken::with_storage_and_policy(
            accounting,
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        );

        B20Guards::ensure_blocked(&token, account).unwrap();

        let mut accounting = InMemoryTokenAccounting::new(Address::repeat_byte(0x20));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        let token = TestStablecoinToken::with_storage_and_policy(
            accounting,
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        );

        assert_eq!(
            B20Guards::ensure_blocked(&token, account).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account })
        );
    }
}
