//! Role-management operations for B-20 tokens.

use alloy_primitives::{Address, B256, U256, b256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::guards::B20Guards;
use crate::{IB20, Token, TokenAccounting};

const MINT_ROLE: B256 = b256!("154c00819833dac601ee5ddded6fda79d9d8b506b911b3dbd54cdb95fe6c3686");
const BURN_ROLE: B256 = b256!("e97b137254058bd94f28d2f3eb79e2d34074ffb488d042e3bc958e0a57d2fa22");
const BURN_BLOCKED_ROLE: B256 =
    b256!("7408fdc0d31c7bcb349eab611f5d1168acd4303574993f8cdc98b1cd18c41cae");
const PAUSE_ROLE: B256 = b256!("139c2898040ef16910dc9f44dc697df79363da767d8bc92f2e310312b816e46d");
const UNPAUSE_ROLE: B256 =
    b256!("265b220c5a8891efdd9e1b1b7fa72f257bd5169f8d87e319cf3dad6ff52b94ae");
const METADATA_ROLE: B256 =
    b256!("6bd6b5318a46e5fff572d5e4258a20774aab40cc35ac7680654b9081fcc82f80");

/// Built-in B-20 roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20TokenRole {
    /// The default top-level admin role.
    DefaultAdmin,
    /// Role required for `mint` and `mintWithMemo`.
    Mint,
    /// Role required for `burn` and `burnWithMemo`.
    Burn,
    /// Role required for `burnBlocked`; permits burning from blocked accounts without `BURN_ROLE`.
    BurnBlocked,
    /// Role required for `pause`.
    Pause,
    /// Role required for `unpause`.
    Unpause,
    /// Role required for `setName` and `setSymbol`.
    Metadata,
}

impl B20TokenRole {
    /// Returns the `AccessControl` role identifier.
    pub const fn id(self) -> B256 {
        match self {
            Self::DefaultAdmin => B256::ZERO,
            Self::Mint => MINT_ROLE,
            Self::Burn => BURN_ROLE,
            Self::BurnBlocked => BURN_BLOCKED_ROLE,
            Self::Pause => PAUSE_ROLE,
            Self::Unpause => UNPAUSE_ROLE,
            Self::Metadata => METADATA_ROLE,
        }
    }
}

/// Role-management operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement with an empty body to opt in.
pub trait RoleManaged: Token {
    /// The default top-level admin role.
    fn default_admin_role() -> B256 {
        B20TokenRole::DefaultAdmin.id()
    }

    /// Role required for `mint` and `mintWithMemo`.
    fn mint_role() -> B256 {
        B20TokenRole::Mint.id()
    }

    /// Role required for `burn` and `burnWithMemo`.
    fn burn_role() -> B256 {
        B20TokenRole::Burn.id()
    }

    /// Role required for `burnBlocked`; permits burning from blocked accounts without `BURN_ROLE`.
    fn burn_blocked_role() -> B256 {
        B20TokenRole::BurnBlocked.id()
    }

    /// Role required for `pause`.
    fn pause_role() -> B256 {
        B20TokenRole::Pause.id()
    }

    /// Role required for `unpause`.
    fn unpause_role() -> B256 {
        B20TokenRole::Unpause.id()
    }

    /// Role required for `setName` and `setSymbol`.
    fn metadata_role() -> B256 {
        B20TokenRole::Metadata.id()
    }

    /// Returns the admin role for `role`.
    fn role_admin(&self, role: B256) -> Result<B256> {
        self.accounting().role_admin(role)
    }

    /// Returns whether `account` has `role`.
    fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        self.accounting().has_role(role, account)
    }

    /// Grants `role` to `account` without checking caller authorization.
    fn grant_role_unchecked(
        &mut self,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if self.accounting().has_role(role, account)? {
            return Ok(());
        }
        self.accounting_mut().set_role(role, account, true)?;
        if role == Self::default_admin_role() {
            let current = self.accounting().role_member_count(role)?;
            let next =
                current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            self.accounting_mut().set_role_member_count(role, next)?;
        }
        self.accounting_mut()
            .emit_event(IB20::RoleGranted { role, account, sender }.encode_log_data())
    }

    /// Revokes `role` from `account` without checking caller authorization.
    fn revoke_role_unchecked(
        &mut self,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if !self.accounting().has_role(role, account)? {
            return Ok(());
        }
        self.accounting_mut().set_role(role, account, false)?;
        if role == Self::default_admin_role() {
            let current = self.accounting().role_member_count(role)?;
            let next =
                current.checked_sub(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
            self.accounting_mut().set_role_member_count(role, next)?;
        }
        self.accounting_mut()
            .emit_event(IB20::RoleRevoked { role, account, sender }.encode_log_data())
    }

    /// Ensures `caller` has `role`.
    fn ensure_role(&self, caller: Address, role: B256) -> Result<()> {
        B20Guards::ensure_role(self, caller, role)
    }

    /// Grants `role` to `account`, optionally bypassing authorization during factory init.
    fn grant_role(
        &mut self,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            self.ensure_role(caller, self.role_admin(role)?)?;
        }
        self.grant_role_unchecked(role, account, caller)
    }

    /// Revokes `role` from `account`, optionally bypassing authorization during factory init.
    fn revoke_role(
        &mut self,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            self.ensure_role(caller, self.role_admin(role)?)?;
        }
        self.revoke_role_unchecked(role, account, caller)
    }

    /// Renounces `role` for `caller`.
    ///
    /// Matches `AccessControl` no-op semantics for accounts that do not hold `role`: the call
    /// succeeds and emits no `RoleRevoked` event. The only stricter path is the final
    /// `DEFAULT_ADMIN_ROLE` holder, which must use [`Self::renounce_last_admin`].
    fn renounce_role(&mut self, caller: Address, role: B256, confirmation: Address) -> Result<()> {
        if confirmation != caller {
            return Err(BasePrecompileError::revert(IB20::AccessControlBadConfirmation {}));
        }
        if role == Self::default_admin_role()
            && self.accounting().has_role(role, caller)?
            && self.accounting().role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(role, caller, caller)
    }

    /// Permanently removes the final default admin.
    fn renounce_last_admin(&mut self, caller: Address) -> Result<()> {
        let admin_role = Self::default_admin_role();
        self.ensure_role(caller, admin_role)?;
        if self.accounting().role_member_count(admin_role)? != U256::ONE {
            return Err(BasePrecompileError::revert(IB20::NotSoleAdmin {}));
        }
        self.revoke_role_unchecked(admin_role, caller, caller)?;
        self.accounting_mut()
            .emit_event(IB20::LastAdminRenounced { previousAdmin: caller }.encode_log_data())
    }

    /// Sets the admin role for `role`.
    ///
    /// This intentionally follows `AccessControl` semantics, including for
    /// `DEFAULT_ADMIN_ROLE`. Setting its admin to a role with no members can make ordinary admin
    /// recovery impossible; [`Self::renounce_last_admin`] remains the explicit terminal path for
    /// burning the final admin.
    fn set_role_admin(
        &mut self,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()> {
        let previous_admin_role = self.role_admin(role)?;
        if !privileged {
            self.ensure_role(caller, previous_admin_role)?;
        }
        self.accounting_mut().set_role_admin(role, new_admin_role)?;
        self.accounting_mut().emit_event(
            IB20::RoleAdminChanged {
                role,
                previousAdminRole: previous_admin_role,
                newAdminRole: new_admin_role,
            }
            .encode_log_data(),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::BasePrecompileError;

    use super::{B20TokenRole, RoleManaged};
    use crate::{
        IB20, Token, TokenAccounting,
        common::test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const ADMIN: Address = Address::repeat_byte(0xaa);
    const ALICE: Address = Address::repeat_byte(0xbb);
    const TOKEN_ADDR: Address = Address::repeat_byte(0x11);
    const CUSTOM_ROLE: B256 = B256::repeat_byte(0x42);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn token_with_default_admin() -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((B20TokenRole::DefaultAdmin.id(), ADMIN), true);
        accounting.role_member_counts.insert(B20TokenRole::DefaultAdmin.id(), U256::ONE);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn grant_role_authorizes_against_role_admin_and_emits_event() {
        let mut token = token_with_default_admin();

        token.grant_role(ADMIN, B20TokenRole::Mint.id(), ALICE, false).unwrap();

        assert!(token.has_role(B20TokenRole::Mint.id(), ALICE).unwrap());
        assert_eq!(
            token.accounting().events[0],
            IB20::RoleGranted { role: B20TokenRole::Mint.id(), account: ALICE, sender: ADMIN }
                .encode_log_data()
        );
    }

    #[test]
    fn grant_role_without_admin_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.grant_role(ADMIN, B20TokenRole::Mint.id(), ALICE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ADMIN,
                neededRole: B256::ZERO,
            })
        );
    }

    #[test]
    fn renounce_role_rejects_final_default_admin() {
        let mut token = token_with_default_admin();

        assert_eq!(
            token.renounce_role(ADMIN, B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap_err(),
            BasePrecompileError::revert(IB20::LastAdminCannotRenounce {})
        );
    }

    #[test]
    fn renounce_last_admin_revokes_and_emits_terminal_event() {
        let mut token = token_with_default_admin();

        token.renounce_last_admin(ADMIN).unwrap();

        assert!(!token.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
        assert_eq!(
            token.accounting().role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(),
            U256::ZERO
        );
        assert_eq!(
            token.accounting().events.last().unwrap(),
            &IB20::LastAdminRenounced { previousAdmin: ADMIN }.encode_log_data()
        );
    }

    #[test]
    fn set_role_admin_emits_previous_and_new_admin_roles() {
        let mut token = token_with_default_admin();

        token.set_role_admin(ADMIN, CUSTOM_ROLE, B20TokenRole::Mint.id(), false).unwrap();

        assert_eq!(token.role_admin(CUSTOM_ROLE).unwrap(), B20TokenRole::Mint.id());
        assert_eq!(
            token.accounting().events[0],
            IB20::RoleAdminChanged {
                role: CUSTOM_ROLE,
                previousAdminRole: B256::ZERO,
                newAdminRole: B20TokenRole::Mint.id(),
            }
            .encode_log_data()
        );
    }
}
