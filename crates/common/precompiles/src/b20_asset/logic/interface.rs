//! Append-only business-logic interface for the asset B-20 precompile.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, FixedBytes, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    AssetAccounting, B20AssetStorage, B20AssetToken, B20Guards, Eip712Domain, IB20, IB20Asset,
    PermitArgs, PolicyAccounting, Token,
};

/// The asset logic interface.
pub trait Asset<S: AssetAccounting, A: PolicyAccounting> {
    /// ERC-20 `transfer`.
    fn transfer(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `transferFrom`.
    fn transfer_from(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// ERC-20 `approve`.
    fn approve(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()>;

    /// Emits a `Memo` event attributed to `caller`.
    ///
    /// The memo-decorated ABI calls (`transferWithMemo`, `mintWithMemo`, …) are composed
    /// by the dispatcher as the base operation followed by this event, so the memo semantics
    /// stay version-defined without widening every operation's signature.
    fn emit_memo(&self, token: &mut B20AssetToken<S, A>, caller: Address, memo: B256)
    -> Result<()>;

    /// Mints `amount` to `to`.
    fn mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Self-burn: the caller destroys `amount` of its own balance.
    fn burn(&self, token: &mut B20AssetToken<S, A>, caller: Address, amount: U256) -> Result<()>;

    /// Destroys `amount` from a policy-blocked `from` account.
    fn burn_blocked(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Pauses the given features.
    fn pause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Unpauses the given features.
    fn unpause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the maximum total supply.
    fn update_supply_cap(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token name.
    fn update_name(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the token symbol.
    fn update_symbol(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the contract URI.
    fn update_contract_uri(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()>;

    /// Grants `role` to `account`.
    fn grant_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Revokes `role` from `account`.
    fn revoke_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()>;

    /// Renounces `role` for the caller.
    fn renounce_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()>;

    /// Permanently removes the final default admin.
    fn renounce_last_admin(&self, token: &mut B20AssetToken<S, A>, caller: Address) -> Result<()>;

    /// Sets the admin role for `role`.
    fn set_role_admin(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()>;

    /// Updates the policy ID configured for `policy_scope`.
    fn update_policy(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()>;

    /// EIP-2612 `permit`.
    fn permit(
        &self,
        token: &mut B20AssetToken<S, A>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()>;

    // --- Asset-specific mutations ---

    /// Instant failsafe: sets the current multiplier immediately. Requires the operator role
    /// unless privileged.
    fn update_multiplier(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_multiplier: U256,
        now: U256,
        privileged: bool,
    ) -> Result<()>;

    /// Sets, updates, or removes an extra-metadata entry. Requires the metadata role.
    fn update_extra_metadata(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        key: String,
        value: String,
        privileged: bool,
    ) -> Result<()>;

    /// Mints `amounts[i]` to `recipients[i]`. Requires the mint role. All-or-nothing.
    fn batch_mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()>;

    /// Opens an announcement: authorizes `caller`, consumes `id`, and emits `Announcement`.
    ///
    /// The dispatcher owns the atomic execution of the announcement's internal calls (routing is
    /// its responsibility); this method and [`Self::end_announce`] carry the version-defined
    /// business steps that bracket that loop.
    fn begin_announce(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()>;

    /// Closes an announcement after its internal calls have executed: emits `EndAnnouncement`.
    fn end_announce(&self, token: &mut B20AssetToken<S, A>, id: String) -> Result<()>;

    // --- Direct reads: version-invariant pass-throughs to the storage port, so the
    //     dispatcher never touches token storage directly. Defaulted here and shared by
    //     every version; a version overrides one only if its read semantics change. ---

    /// Returns whether marker bytecode is deployed at this token's address.
    fn is_initialized(&self, token: &B20AssetToken<S, A>) -> Result<bool> {
        token.accounting().is_initialized()
    }

    /// Returns the token name.
    fn name(&self, token: &B20AssetToken<S, A>) -> Result<String> {
        token.accounting().name()
    }

    /// Returns the token symbol.
    fn symbol(&self, token: &B20AssetToken<S, A>) -> Result<String> {
        token.accounting().symbol()
    }

    /// Returns the custom decimal precision configured for this asset token.
    fn decimals(&self, token: &B20AssetToken<S, A>) -> Result<u8> {
        AssetAccounting::decimals(token.accounting())
    }

    /// Returns the total token supply currently in circulation.
    fn total_supply(&self, token: &B20AssetToken<S, A>) -> Result<U256> {
        token.accounting().total_supply()
    }

    /// Returns the token balance of `account`.
    fn balance_of(&self, token: &B20AssetToken<S, A>, account: Address) -> Result<U256> {
        token.accounting().balance_of(account)
    }

    /// Returns the allowance granted by `owner` to `spender`.
    fn allowance(
        &self,
        token: &B20AssetToken<S, A>,
        owner: Address,
        spender: Address,
    ) -> Result<U256> {
        token.accounting().allowance(owner, spender)
    }

    /// Returns the maximum total supply enforced on mint.
    fn supply_cap(&self, token: &B20AssetToken<S, A>) -> Result<U256> {
        token.accounting().supply_cap()
    }

    /// Returns the current EIP-2612 permit nonce for `owner`.
    fn nonce(&self, token: &B20AssetToken<S, A>, owner: Address) -> Result<U256> {
        token.accounting().nonce(owner)
    }

    /// Returns the off-chain metadata URI for this token (ERC-7572).
    fn contract_uri(&self, token: &B20AssetToken<S, A>) -> Result<String> {
        token.accounting().contract_uri()
    }

    /// Returns whether `account` has `role`.
    fn has_role(&self, token: &B20AssetToken<S, A>, role: B256, account: Address) -> Result<bool> {
        token.accounting().has_role(role, account)
    }

    /// Returns the admin role for `role`.
    fn role_admin(&self, token: &B20AssetToken<S, A>, role: B256) -> Result<B256> {
        token.accounting().role_admin(role)
    }

    /// Returns the current multiplier, scaled to WAD.
    ///
    /// The default returns the stored (materialized) multiplier and ignores `now`; versions with
    /// lazy scheduling (`AssetV2`+) override this to return the effective value at `now`.
    fn multiplier(&self, token: &B20AssetToken<S, A>, _now: U256) -> Result<U256> {
        token.accounting().multiplier()
    }

    /// Returns the extra-metadata value for `key`, or the empty string if unset.
    fn extra_metadata(&self, token: &B20AssetToken<S, A>, key: &str) -> Result<String> {
        token.accounting().extra_metadata(key)
    }

    /// Returns whether announcement `id` has already been consumed.
    fn is_announcement_id_used(&self, token: &B20AssetToken<S, A>, id: &str) -> Result<bool> {
        token.accounting().is_announcement_id_used(id)
    }

    // --- Computed reads: derive from storage but encode version-defined semantics ---

    /// Returns whether the given pause `feature` is currently set.
    fn is_paused(
        &self,
        token: &B20AssetToken<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool>;

    /// Returns all currently paused features.
    fn paused_features(&self, token: &B20AssetToken<S, A>) -> Result<Vec<IB20::PausableFeature>>;

    /// Returns the configured policy ID for `policy_scope`, validating the scope.
    fn policy_id(&self, token: &B20AssetToken<S, A>, policy_scope: B256) -> Result<u64>;

    /// Computes the EIP-712 domain separator for this token.
    fn domain_separator(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<B256>;

    /// Returns the ERC-5267 `eip712Domain()` tuple for this token.
    fn eip712_domain(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<Eip712Domain>;

    /// Converts a raw balance to its scaled view: `rawBalance * multiplier / WAD` at `now`.
    fn to_scaled_balance(
        &self,
        token: &B20AssetToken<S, A>,
        balance: U256,
        now: U256,
    ) -> Result<U256>;

    /// Converts a scaled balance back to its raw representation: `scaledBalance * WAD / multiplier`
    fn to_raw_balance(&self, token: &B20AssetToken<S, A>, balance: U256, now: U256)
    -> Result<U256>;

    /// Returns the scaled balance for `account` at `now`.
    fn scaled_balance_of(
        &self,
        token: &B20AssetToken<S, A>,
        account: Address,
        now: U256,
    ) -> Result<U256>;

    /// Returns the asset operator role identifier (required for `announce` / `updateMultiplier`).
    fn operator_role(&self) -> B256;

    // --- ERC-8056 scheduled multiplier (introduced at `AssetV2`, Cobalt) ---
    //
    // These are defaulted here with the full scheduling semantics; every version that advertises
    // the ERC-8056 surface (currently only `AssetV2`) shares them. `AssetV1` never reaches these:
    // the dispatcher gates the ERC-8056 selectors to versions that support them, so on Beryl the
    // selectors stay unknown (identical revert to before the enum grew).

    /// The effective multiplier at `now`: the pending target once matured (`now >= effectiveAt`),
    /// otherwise the current (materialized) multiplier. This is the ERC-8056 `_multiplier()` and the
    /// value every scaled read routes through in scheduling-aware versions.
    fn effective_multiplier(&self, token: &B20AssetToken<S, A>, now: U256) -> Result<U256> {
        let effective_at = token.accounting().pending_effective_at()?;
        if effective_at != 0 && now >= U256::from(effective_at) {
            let pending = token.accounting().pending_multiplier()?;
            // Invariant: both setters reject a zero (or `> u128::MAX`) multiplier, so a stored
            // pending is never zero. There is deliberately NO zero-to-WAD fallback here — the
            // Solidity reference returns the raw `pending.multiplier` too, so adding one would
            // diverge from it (the Rust would succeed where the reference reverts on a corrupt
            // zero divisor). The debug assertion pins the invariant in debug/test builds without
            // altering release/consensus behavior.
            debug_assert!(pending != 0, "matured pending multiplier must be non-zero");
            return Ok(U256::from(pending));
        }
        token.accounting().multiplier()
    }

    /// ERC-8056 `uiMultiplier()` — alias of the (effective) `multiplier()`.
    fn ui_multiplier(&self, token: &B20AssetToken<S, A>, now: U256) -> Result<U256> {
        self.multiplier(token, now)
    }

    /// ERC-8056 `newUIMultiplier()` — the live pending target, or the effective multiplier when no
    /// live pending exists.
    fn new_ui_multiplier(&self, token: &B20AssetToken<S, A>, now: U256) -> Result<U256> {
        let effective_at = token.accounting().pending_effective_at()?;
        if U256::from(effective_at) > now {
            return Ok(U256::from(token.accounting().pending_multiplier()?));
        }
        self.multiplier(token, now)
    }

    /// ERC-8056 `effectiveAt()` — the raw stored pending timestamp (a matured pending keeps its
    /// past value until the next set/cancel materializes it; cancel resets it to 0).
    fn effective_at(&self, token: &B20AssetToken<S, A>) -> Result<U256> {
        Ok(U256::from(token.accounting().pending_effective_at()?))
    }

    /// ERC-8056 Balances extension `balanceOfUI()` — alias of `scaled_balance_of`.
    fn balance_of_ui(
        &self,
        token: &B20AssetToken<S, A>,
        account: Address,
        now: U256,
    ) -> Result<U256> {
        self.scaled_balance_of(token, account, now)
    }

    /// ERC-8056 Balances extension `totalSupplyUI()` — `totalSupply * multiplier / WAD` at `now`.
    fn total_supply_ui(&self, token: &B20AssetToken<S, A>, now: U256) -> Result<U256> {
        let multiplier = self.effective_multiplier(token, now)?;
        let supply = token.accounting().total_supply()?;
        let product =
            supply.checked_mul(multiplier).ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / B20AssetStorage::WAD)
    }

    /// Schedules a single pending multiplier update effective at `effective_at`.
    ///
    /// Reverts `InvalidMultiplier` (zero or `> u128::MAX`), `EffectiveAtInPast`
    /// (`effective_at <= now`), `EffectiveAtTooFar` (`effective_at > u64::MAX`), or `ScheduleOverlap`
    /// (a live pending already exists). A *matured* pending is first folded into the current
    /// multiplier so a scheduled change is never silently lost. Emits `UIMultiplierUpdated`.
    fn set_ui_multiplier(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_multiplier: U256,
        effective_at: U256,
        now: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role(token, caller, self.operator_role())?;
        }
        if new_multiplier.is_zero() || new_multiplier > U256::from(u128::MAX) {
            return Err(BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
        }
        if effective_at <= now {
            return Err(BasePrecompileError::revert(IB20Asset::EffectiveAtInPast {
                effectiveAt: effective_at,
            }));
        }
        if effective_at > U256::from(u64::MAX) {
            return Err(BasePrecompileError::revert(IB20Asset::EffectiveAtTooFar {
                effectiveAt: effective_at,
            }));
        }

        let pending_effective_at = token.accounting().pending_effective_at()?;
        // A live pending blocks a new schedule.
        if U256::from(pending_effective_at) > now {
            return Err(BasePrecompileError::revert(IB20Asset::ScheduleOverlap {
                pendingEffectiveAt: U256::from(pending_effective_at),
            }));
        }
        // Fold a matured-but-uncancelled pending into the current multiplier before overwriting it,
        // so it is never lost.
        if pending_effective_at != 0 {
            let matured = U256::from(token.accounting().pending_multiplier()?);
            token.accounting_mut().set_multiplier(matured)?;
        }

        let old = token.accounting().multiplier()?;
        // Narrowing is safe: the guards above enforce `new_multiplier <= u128::MAX` and
        // `effective_at <= u64::MAX`, so neither `to::<..>()` can overflow.
        token
            .accounting_mut()
            .set_pending(new_multiplier.to::<u128>(), effective_at.to::<u64>())?;
        token.accounting_mut().emit_event(
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: old,
                newMultiplier: new_multiplier,
                effectiveAtTimestamp: effective_at,
            }
            .encode_log_data(),
        )
    }

    /// Cancels the single live pending update, restoring the no-pending state. Reverts
    /// `NoScheduledMultiplier` when no live pending exists. Emits `MultiplierUpdateCancelled`.
    ///
    /// A *live* pending is `pending_effective_at > now`. A pending that matures at exactly `now`
    /// (`pending_effective_at == now`) has already taken effect and is therefore NOT cancellable —
    /// it reverts `NoScheduledMultiplier`. This matches the `effective_at <= now` maturity boundary
    /// used by the read path (`effective_multiplier`) and by `set_ui_multiplier`'s `EffectiveAtInPast`
    /// guard, so the boundary is consistent across the whole surface.
    fn cancel_scheduled_multiplier(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        now: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role(token, caller, self.operator_role())?;
        }
        let pending_multiplier = U256::from(token.accounting().pending_multiplier()?);
        let pending_effective_at = U256::from(token.accounting().pending_effective_at()?);
        // Only a live pending can be cancelled.
        if pending_effective_at <= now {
            return Err(BasePrecompileError::revert(IB20Asset::NoScheduledMultiplier {}));
        }
        token.accounting_mut().clear_pending()?;
        token.accounting_mut().emit_event(
            IB20Asset::MultiplierUpdateCancelled {
                cancelledMultiplier: pending_multiplier,
                cancelledEffectiveAt: pending_effective_at,
            }
            .encode_log_data(),
        )
    }

    /// ERC-165; advertises ERC-165 itself plus three claimed ERC-8056 interfaces
    fn supports_interface(&self, interface_id: FixedBytes<4>) -> bool {
        crate::ERC8056_INTERFACE_IDS.contains(&interface_id)
    }
}
