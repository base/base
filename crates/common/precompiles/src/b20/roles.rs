//! Role helpers for B-20 tokens.

use alloy_primitives::{Address, B256, U256, b256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::token::B20Token;
use crate::{IB20, Policy, Token, TokenAccounting};

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

impl<S: TokenAccounting, P: Policy> B20Token<S, P> {
    /// The default top-level admin role.
    pub const fn default_admin_role() -> B256 {
        B20TokenRole::DefaultAdmin.id()
    }

    /// Role required for `mint` and `mintWithMemo`.
    pub const fn mint_role() -> B256 {
        B20TokenRole::Mint.id()
    }

    /// Role required for `burn` and `burnWithMemo`.
    pub const fn burn_role() -> B256 {
        B20TokenRole::Burn.id()
    }

    /// Role required for `burnBlocked`; permits burning from blocked accounts without `BURN_ROLE`.
    pub const fn burn_blocked_role() -> B256 {
        B20TokenRole::BurnBlocked.id()
    }

    /// Role required for `pause`.
    pub const fn pause_role() -> B256 {
        B20TokenRole::Pause.id()
    }

    /// Role required for `unpause`.
    pub const fn unpause_role() -> B256 {
        B20TokenRole::Unpause.id()
    }

    /// Role required for `setName` and `setSymbol`.
    pub const fn metadata_role() -> B256 {
        B20TokenRole::Metadata.id()
    }

    /// Returns the admin role for `role`.
    pub fn role_admin(&self, role: B256) -> Result<B256> {
        self.accounting.role_admin(role)
    }

    /// Returns whether `account` has `role`.
    pub fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        self.accounting.has_role(role, account)
    }

    /// Grants `role` to `account` without checking caller authorization.
    pub fn grant_role_unchecked(
        &mut self,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if self.accounting.has_role(role, account)? {
            return Ok(());
        }
        let current = self.accounting.role_member_count(role)?;
        let next =
            current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_role(role, account, true)?;
        self.accounting_mut().set_role_member_count(role, next)?;
        self.accounting_mut()
            .emit_event(IB20::RoleGranted { role, account, sender }.encode_log_data())
    }

    /// Revokes `role` from `account` without checking caller authorization.
    pub fn revoke_role_unchecked(
        &mut self,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()> {
        if !self.accounting.has_role(role, account)? {
            return Ok(());
        }
        let current = self.accounting.role_member_count(role)?;
        let next =
            current.checked_sub(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_role(role, account, false)?;
        self.accounting_mut().set_role_member_count(role, next)?;
        self.accounting_mut()
            .emit_event(IB20::RoleRevoked { role, account, sender }.encode_log_data())
    }

    /// Ensures `caller` has `role`.
    pub fn ensure_role(&self, caller: Address, role: B256) -> Result<()> {
        if self.accounting.has_role(role, caller)? {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: caller,
                neededRole: role,
            }))
        }
    }

    /// Grants `role` to `account`, optionally bypassing authorization during factory init.
    pub fn grant_role(
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
    pub fn revoke_role(
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
    pub fn renounce_role(
        &mut self,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()> {
        if confirmation != caller {
            return Err(BasePrecompileError::revert(IB20::AccessControlBadConfirmation {}));
        }
        if role == Self::default_admin_role()
            && self.accounting.has_role(role, caller)?
            && self.accounting.role_member_count(role)? == U256::ONE
        {
            return Err(BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
        }
        self.revoke_role_unchecked(role, caller, caller)
    }

    /// Permanently removes the final default admin.
    pub fn renounce_last_admin(&mut self, caller: Address) -> Result<()> {
        let admin_role = Self::default_admin_role();
        self.ensure_role(caller, admin_role)?;
        if self.accounting.role_member_count(admin_role)? != U256::ONE {
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
    pub fn set_role_admin(
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
