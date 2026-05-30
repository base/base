//! Security-specific capability trait for B-20 tokens.
//!
//! Mirrors the shared capability traits (`Burnable`, `Mintable`, `Pausable`, etc.)
//! by bundling the security variant's business operations behind a single trait that
//! `B20SecurityToken` opts into. All ABI-driven security logic — share math,
//! redemption, batched mint/burn, announcements, and the operator/burn-from guards —
//! lives here so `dispatch.rs` can stay limited to ABI decode plus thin trait calls.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use crate::{
    B20Guards, B20PolicyType, B20SecurityStorage, B20TokenRole, IB20, IB20Security, Mintable,
    RoleManaged, SecurityAccounting, Token, TokenAccounting,
};

/// Security-variant operations: redemption, batched mint/burn, announcements,
/// and the security/burn-from role guards.
///
/// Required methods cover state that lives on the concrete token (the
/// `in_announcement` flag and the self-dispatch hook for announcement
/// internal calls); everything else has a default implementation.
pub trait SecurityManagement: Token + Mintable + RoleManaged
where
    Self::Accounting: SecurityAccounting,
{
    /// Policy slot checked against redeem senders.
    fn redeem_sender_policy() -> B256 {
        B20PolicyType::RedeemSender.id()
    }

    /// Returns whether the token is currently executing an `announce` block.
    fn is_announcement_active(&self) -> bool;

    /// Marks the token as currently executing an `announce` block.
    fn begin_announcement(&mut self);

    /// Self-dispatches calldata produced inside an `announce` block.
    ///
    /// Concrete tokens forward this to their ABI dispatcher (e.g.
    /// `B20SecurityToken::inner_with_privilege`).
    fn dispatch_internal_call(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> Result<Bytes>;

    /// Ensures `caller` holds `SECURITY_OPERATOR_ROLE` (skipped when `privileged`).
    fn ensure_security_operator(&self, caller: Address, privileged: bool) -> Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, B20TokenRole::SecurityOperator.id()) }
    }

    /// Ensures `caller` holds `BURN_FROM_ROLE`.
    fn ensure_burn_from_role(&self, caller: Address) -> Result<()> {
        self.ensure_role(caller, B20TokenRole::BurnFrom.id())
    }

    /// Converts a token balance to shares: `balance * sharesToTokensRatio / WAD`.
    fn to_shares(&self, balance: U256) -> Result<U256> {
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        let product = balance.checked_mul(ratio).ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / B20SecurityStorage::WAD)
    }

    /// Performs a security-specific redeem: share-based floor check, burn, security `Redeemed` event.
    fn security_redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// [`Self::security_redeem`] with a memo emitted between `Transfer` and `Redeemed`.
    fn security_redeem_with_memo(
        &mut self,
        caller: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// Performs the shared security redeem burn and returns the ratio used for the floor check.
    fn security_redeem_burn(&mut self, caller: Address, amount: U256) -> Result<U256> {
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::REDEEM)?;
        B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::RedeemSender, caller)?;
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        if !amount.is_zero() {
            let shares =
                amount.checked_mul(ratio).ok_or_else(BasePrecompileError::under_overflow)?
                    / B20SecurityStorage::WAD;
            let minimum = self.accounting().minimum_redeemable()?;
            if shares == U256::ZERO || shares < minimum {
                return Err(BasePrecompileError::revert(IB20Security::BelowMinimumRedeemable {
                    shares,
                    minimum,
                }));
            }
        }
        let balance = self.accounting().balance_of(caller)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: caller,
                balance,
                needed: amount,
            }));
        }
        self.accounting_mut().set_balance(caller, balance - amount)?;
        let supply = self.accounting().total_supply()?;
        let new_supply =
            supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_total_supply(new_supply)?;
        self.accounting_mut().emit_event(
            IB20::Transfer { from: caller, to: Address::ZERO, amount }.encode_log_data(),
        )?;
        Ok(ratio)
    }

    /// Emits the security-specific `Redeemed` event.
    fn emit_redeemed(&mut self, caller: Address, amount: U256, ratio: U256) -> Result<()> {
        self.accounting_mut().emit_event(
            IB20Security::Redeemed { from: caller, amt: amount, sharesToTokensRatio: ratio }
                .encode_log_data(),
        )
    }

    /// Mints tokens to multiple recipients. All-or-nothing.
    fn batch_mint(
        &mut self,
        ctx: StorageCtx<'_>,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        if recipients.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(recipients.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if recipients.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        let caller = ctx.caller();
        for (recipient, amount) in recipients.into_iter().zip(amounts) {
            self.mint(caller, recipient, amount, privileged)?;
        }
        Ok(())
    }

    /// Burns tokens from multiple accounts unconditionally. All-or-nothing.
    ///
    /// Unlike `burnBlocked`, this path has no policy precondition. The
    /// `BURN_FROM_ROLE` authorization and burn pause check are the only gates.
    fn batch_burn(
        &mut self,
        ctx: StorageCtx<'_>,
        accounts: Vec<Address>,
        amounts: Vec<U256>,
    ) -> Result<()> {
        let caller = ctx.caller();
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
        self.ensure_burn_from_role(caller)?;
        if accounts.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(accounts.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if accounts.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        for (account, amount) in accounts.into_iter().zip(amounts) {
            let balance = self.accounting().balance_of(account)?;
            if balance < amount {
                return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: account,
                    balance,
                    needed: amount,
                }));
            }
            self.accounting_mut().set_balance(account, balance - amount)?;
            let supply = self.accounting().total_supply()?;
            let new_supply =
                supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
            self.accounting_mut().set_total_supply(new_supply)?;
            self.accounting_mut().emit_event(
                IB20::Transfer { from: account, to: Address::ZERO, amount }.encode_log_data(),
            )?;
        }
        Ok(())
    }

    /// Posts an announcement and atomically executes `internal_calls` via self-dispatch.
    ///
    /// The `in_announcement` flag and selector check prevent recursive invocation.
    fn announce(
        &mut self,
        ctx: StorageCtx<'_>,
        internal_calls: Vec<Bytes>,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        let caller = ctx.caller();
        self.ensure_security_operator(caller, privileged)?;
        if self.is_announcement_active() {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
        }

        if self.accounting().is_announcement_id_used(id.as_str())? {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementIdAlreadyUsed {
                id,
            }));
        }
        self.accounting_mut().mark_announcement_id_used(id.as_str())?;

        self.accounting_mut().emit_event(
            IB20Security::Announcement { caller, id: id.clone(), description, uri }
                .encode_log_data(),
        )?;

        self.begin_announcement();

        for call in &internal_calls {
            let call_bytes: &[u8] = call.as_ref();
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Security::InternalCallMalformed {
                    call: call.clone(),
                }));
            }
            if call_bytes[..4] == IB20Security::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
            }
            self.dispatch_internal_call(ctx, call_bytes, privileged).map_err(|_| {
                BasePrecompileError::revert(IB20Security::InternalCallFailed { call: call.clone() })
            })?;
        }

        self.accounting_mut().emit_event(IB20Security::EndAnnouncement { id }.encode_log_data())
    }
}
