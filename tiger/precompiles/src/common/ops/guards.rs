//! Authorization and policy guards for B-20 token operations.

use alloy_primitives::{Address, B256, U256};

use crate::{
    B20PausableFeature, B20PolicyType, B20Token, B20TokenRole, BasePrecompileError, IB20,
    PolicyAccounting, Result, TokenAccounting,
};

impl<S: TokenAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// Ensures `caller` has the B-20 `role`.
    pub fn ensure_token_role(&self, caller: Address, role: B20TokenRole) -> Result<()> {
        self.ensure_role(caller, role.id())
    }

    /// Ensures `caller` has `role`.
    pub fn ensure_role(&self, caller: Address, role: B256) -> Result<()> {
        if self.accounting.has_role(role, caller)? {
            return Ok(());
        }
        Err(BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: caller,
            neededRole: role,
        }))
    }

    /// Ensures `caller` holds `role` unless the call is factory-privileged.
    pub fn ensure_role_unless(&self, privileged: bool, caller: Address, role: B256) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, role) }
    }

    /// Ensures `feature` is not paused.
    pub fn ensure_not_paused(&self, feature: IB20::PausableFeature) -> Result<()> {
        if (self.accounting.paused()? & B20PausableFeature::mask(feature)) == U256::ZERO {
            return Ok(());
        }
        Err(BasePrecompileError::revert(IB20::ContractPaused { feature }))
    }

    /// Ensures the policy of `policy_type` authorizes `account`. Every policy id, built-ins
    /// included, goes to the configured policy registry.
    pub fn ensure_policy_type(&self, policy_type: B20PolicyType, account: Address) -> Result<()> {
        let policy_id = self.accounting.policy_id(policy_type.id())?;
        self.ensure_authorized_by_id(policy_type.id(), policy_id, account)
    }

    /// Ensures policy `policy_id`, already read for `policy_scope`, authorizes `account`, so a
    /// batch of checks against ids from one slot pays a single SLOAD.
    pub fn ensure_authorized_by_id(
        &self,
        policy_scope: B256,
        policy_id: u64,
        account: Address,
    ) -> Result<()> {
        if self.registry.is_authorized(&self.policy, policy_id, account)? {
            return Ok(());
        }
        Err(BasePrecompileError::revert(IB20::PolicyForbids {
            policyScope: policy_scope,
            policyId: policy_id,
        }))
    }

    /// Returns whether the policy of `policy_type` authorizes `account`.
    fn authorizes(&self, policy_type: B20PolicyType, account: Address) -> Result<bool> {
        let policy_id = self.accounting.policy_id(policy_type.id())?;
        self.registry.is_authorized(&self.policy, policy_id, account)
    }

    /// Ensures the transfer-sender policy does not authorize (blocks) `account`.
    pub fn ensure_blocked(&self, account: Address) -> Result<()> {
        if self.authorizes(B20PolicyType::TransferSender, account)? {
            return Err(BasePrecompileError::revert(IB20::AccountNotBlocked { account }));
        }
        Ok(())
    }

    /// Ensures `account` is seizable: the seize-exempt policy does not authorize it. The unset
    /// always-allow default keeps seizure closed until an issuer configures the scope. Enforced
    /// unconditionally, including in the factory bootstrap window.
    pub fn ensure_seizable(&self, account: Address) -> Result<()> {
        if self.authorizes(B20PolicyType::SeizeExempt, account)? {
            return Err(BasePrecompileError::revert(IB20::AccountNotSeizable { account }));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use crate::{
        B20PolicyType, BasePrecompileError, FakePolicyAccounting, IB20, InMemoryTokenAccounting,
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
        accounting.policy_ids.insert(B20PolicyType::SeizeExempt.id(), EXTERNAL_POLICY_ID);

        let mut policy = FakePolicyAccounting::new();
        policy.allow(EXTERNAL_POLICY_ID, account);

        TestStablecoinToken::with_storage_and_policy(accounting, policy, PolicyVersion::V1)
    }

    #[test]
    fn test_ensure_policy_delegates_external_policy_ids_to_registry() {
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_transfer_sender_policy(allowed);

        token.ensure_policy_type(B20PolicyType::TransferSender, allowed).unwrap();

        assert_eq!(
            token.ensure_policy_type(B20PolicyType::TransferSender, denied)
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
            token.ensure_blocked(allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account: allowed })
        );
        token.ensure_blocked(denied).unwrap();
    }

    #[test]
    fn test_ensure_seizable_uses_external_policy_authorization() {
        let allowed = Address::repeat_byte(0xaa);
        let denied = Address::repeat_byte(0xbb);
        let token = token_with_seizable_policy(allowed);

        // Authorized (allowed) under the seizable policy => not seizable => reverts.
        assert_eq!(
            token.ensure_seizable(allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotSeizable { account: allowed })
        );
        // Not authorized (denied) => seizable => ok.
        token.ensure_seizable(denied).unwrap();
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
            token.ensure_seizable(allowed).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotSeizable { account: allowed })
        );

        // `denied` is NOT authorized by transfer-sender, but still not seizable because the
        // seizable policy is unset (ALWAYS_ALLOW) — proving the two scopes are independent.
        assert_eq!(
            token.ensure_seizable(denied).unwrap_err(),
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

        token.ensure_blocked(account).unwrap();

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
            token.ensure_blocked(account).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account })
        );
    }
}
