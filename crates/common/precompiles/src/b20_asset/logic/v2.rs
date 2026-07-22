//! Version 2 of the asset B-20 precompile logic, activated at Cobalt.
//!
//! V2 adds the ERC-8056 "Scaled UI Amount" scheduled-multiplier surface on top of the frozen
//! [`AssetV1`] behavior: the current multiplier becomes *lazy*, a pending update can be
//! scheduled and cancelled, the instant `update_multiplier` failsafe is rewired to the pending storage,
//! and ERC-165 / ERC-8056 aliases are advertised. Storage is append-only,
//! so a token created under V1 upgrades in place with no migration.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    Asset, AssetAccounting, AssetV1, B20AssetStorage, B20AssetToken, B20Guards, Eip712Domain, IB20,
    IB20Asset, PermitArgs, PolicyAccounting, Token,
};

/// Second B-20 Asset precompile implementation. Introduced at Cobalt; adds ERC-8056 scheduled multipliers.
#[derive(Debug, Default, Clone, Copy)]
pub struct AssetV2;

impl<S: AssetAccounting, A: PolicyAccounting> Asset<S, A> for AssetV2 {
    // ============================================================
    //          Multiplier surface — V2-specific behavior
    // ============================================================

    /// Lazy effective multiplier: flips to a matured pending target once `now >= effectiveAt`.
    fn multiplier(&self, token: &B20AssetToken<S, A>, now: U256) -> Result<U256> {
        self.effective_multiplier(token, now)
    }

    fn to_scaled_balance(
        &self,
        token: &B20AssetToken<S, A>,
        balance: U256,
        now: U256,
    ) -> Result<U256> {
        let multiplier = self.effective_multiplier(token, now)?;
        let product =
            balance.checked_mul(multiplier).ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / B20AssetStorage::WAD)
    }

    fn to_raw_balance(
        &self,
        token: &B20AssetToken<S, A>,
        balance: U256,
        now: U256,
    ) -> Result<U256> {
        let multiplier = self.effective_multiplier(token, now)?;
        let product = balance
            .checked_mul(B20AssetStorage::WAD)
            .ok_or_else(BasePrecompileError::under_overflow)?;
        Ok(product / multiplier)
    }

    fn scaled_balance_of(
        &self,
        token: &B20AssetToken<S, A>,
        account: Address,
        now: U256,
    ) -> Result<U256> {
        let balance = token.accounting().balance_of(account)?;
        self.to_scaled_balance(token, balance, now)
    }

    /// Instantaneous failsafe. Writes the current multiplier immediately, clearing any pending update
    fn update_multiplier(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_multiplier: U256,
        now: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role(token, caller, AssetV1::OPERATOR_ROLE)?;
        }
        if new_multiplier.is_zero() || new_multiplier > U256::from(u128::MAX) {
            return Err(BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
        }
        let pending_multiplier = U256::from(token.accounting().pending_multiplier()?);
        let pending_effective_at = U256::from(token.accounting().pending_effective_at()?);
        let live_pending = pending_effective_at > now;

        let old = self.effective_multiplier(token, now)?;
        token.accounting_mut().set_multiplier(new_multiplier)?;
        if pending_effective_at != U256::ZERO {
            token.accounting_mut().clear_pending()?;
        }
        if live_pending {
            token.accounting_mut().emit_event(
                IB20Asset::MultiplierUpdateCancelled {
                    cancelledMultiplier: pending_multiplier,
                    cancelledEffectiveAt: pending_effective_at,
                }
                .encode_log_data(),
            )?;
        }
        token.accounting_mut().emit_event(
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: old,
                newMultiplier: new_multiplier,
                effectiveAtTimestamp: now,
            }
            .encode_log_data(),
        )
    }

    // The ERC-8056 pending/alias/ERC-165 methods use the trait's shared default implementations;
    // V2 needs no override for them.

    // ============================================================
    //   Everything else delegates verbatim to the frozen AssetV1
    // ============================================================

    fn transfer(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.transfer(token, caller, to, amount, privileged)
    }

    fn transfer_from(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.transfer_from(token, caller, from, to, amount, privileged)
    }

    fn approve(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        spender: Address,
        amount: U256,
    ) -> Result<()> {
        AssetV1.approve(token, caller, spender, amount)
    }

    fn emit_memo(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        memo: B256,
    ) -> Result<()> {
        AssetV1.emit_memo(token, caller, memo)
    }

    fn mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.mint(token, caller, to, amount, privileged)
    }

    fn burn(&self, token: &mut B20AssetToken<S, A>, caller: Address, amount: U256) -> Result<()> {
        AssetV1.burn(token, caller, amount)
    }

    fn burn_blocked(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.burn_blocked(token, caller, from, amount, privileged)
    }

    fn pause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.pause(token, caller, features, privileged)
    }

    fn unpause(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        features: Vec<IB20::PausableFeature>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.unpause(token, caller, features, privileged)
    }

    fn update_supply_cap(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_supply_cap(token, caller, new_cap, privileged)
    }

    fn update_name(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        name: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_name(token, caller, name, privileged)
    }

    fn update_symbol(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        symbol: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_symbol(token, caller, symbol, privileged)
    }

    fn update_contract_uri(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_contract_uri(token, caller, uri, privileged)
    }

    fn grant_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.grant_role(token, caller, role, account, privileged)
    }

    fn revoke_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        account: Address,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.revoke_role(token, caller, role, account, privileged)
    }

    fn renounce_role(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        confirmation: Address,
    ) -> Result<()> {
        AssetV1.renounce_role(token, caller, role, confirmation)
    }

    fn renounce_last_admin(&self, token: &mut B20AssetToken<S, A>, caller: Address) -> Result<()> {
        AssetV1.renounce_last_admin(token, caller)
    }

    fn set_role_admin(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        role: B256,
        new_admin_role: B256,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.set_role_admin(token, caller, role, new_admin_role, privileged)
    }

    fn update_policy(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_policy(token, caller, policy_scope, new_policy_id, privileged)
    }

    fn permit(
        &self,
        token: &mut B20AssetToken<S, A>,
        chain_id: u64,
        now: U256,
        args: PermitArgs,
    ) -> Result<()> {
        AssetV1.permit(token, chain_id, now, args)
    }

    fn update_extra_metadata(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        key: String,
        value: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.update_extra_metadata(token, caller, key, value, privileged)
    }

    fn batch_mint(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.batch_mint(token, caller, recipients, amounts, privileged)
    }

    fn begin_announce(
        &self,
        token: &mut B20AssetToken<S, A>,
        caller: Address,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        AssetV1.begin_announce(token, caller, id, description, uri, privileged)
    }

    fn end_announce(&self, token: &mut B20AssetToken<S, A>, id: String) -> Result<()> {
        AssetV1.end_announce(token, id)
    }

    fn is_paused(
        &self,
        token: &B20AssetToken<S, A>,
        feature: IB20::PausableFeature,
    ) -> Result<bool> {
        AssetV1.is_paused(token, feature)
    }

    fn paused_features(&self, token: &B20AssetToken<S, A>) -> Result<Vec<IB20::PausableFeature>> {
        AssetV1.paused_features(token)
    }

    fn policy_id(&self, token: &B20AssetToken<S, A>, policy_scope: B256) -> Result<u64> {
        AssetV1.policy_id(token, policy_scope)
    }

    fn domain_separator(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<B256> {
        AssetV1.domain_separator(token, chain_id)
    }

    fn eip712_domain(&self, token: &B20AssetToken<S, A>, chain_id: u64) -> Result<Eip712Domain> {
        AssetV1.eip712_domain(token, chain_id)
    }

    fn operator_role(&self) -> B256 {
        AssetV1::OPERATOR_ROLE
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, FixedBytes, LogData, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::BasePrecompileError;

    use crate::{
        Asset, AssetAccounting, AssetV1, AssetV2, B20AssetStorage, B20AssetToken,
        FakePolicyAccounting, IB20, IB20Asset, InMemoryTokenAccounting, PolicyVersion, Token,
        TokenAccounting,
    };

    const TOKEN: Address = Address::repeat_byte(0x21);
    const ALICE: Address = Address::repeat_byte(0xA1);
    const BOB: Address = Address::repeat_byte(0xB0);
    const LOGIC: AssetV2 = AssetV2;

    type Tok = B20AssetToken<InMemoryTokenAccounting, FakePolicyAccounting>;

    fn token() -> Tok {
        // `InMemoryTokenAccounting::new` leaves `multiplier == 0`, which the read surface resolves
        // to WAD — a fresh 1:1 token, matching the mock's factory default.
        B20AssetToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
    }

    fn wad() -> U256 {
        B20AssetStorage::WAD
    }

    fn grant_operator(tok: &mut Tok, who: Address) {
        tok.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, who), true);
    }

    fn last_event(tok: &Tok) -> LogData {
        tok.accounting().events.last().unwrap().clone()
    }

    // --- lazy multiplier ---

    #[test]
    fn multiplier_defaults_to_wad() {
        let tok = token();
        assert_eq!(LOGIC.multiplier(&tok, U256::ZERO).unwrap(), wad());
    }

    #[test]
    fn multiplier_flips_lazily_at_effective_at_boundary() {
        let mut tok = token();
        let target = wad() * U256::from(3u64);
        let effective_at = U256::from(100u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, target, effective_at, U256::from(10u64), true)
            .unwrap();

        // T-1: still the old (current) multiplier.
        assert_eq!(LOGIC.multiplier(&tok, U256::from(99u64)).unwrap(), wad());
        // T: flips to the pending target.
        assert_eq!(LOGIC.multiplier(&tok, U256::from(100u64)).unwrap(), target);
        // T+1: stays flipped.
        assert_eq!(LOGIC.multiplier(&tok, U256::from(101u64)).unwrap(), target);
        // ui_multiplier is an alias.
        assert_eq!(LOGIC.ui_multiplier(&tok, U256::from(100u64)).unwrap(), target);
    }

    // --- setUIMultiplier ---

    #[test]
    fn set_ui_multiplier_emits_event_and_records_pending() {
        let mut tok = token();
        let target = wad() * U256::from(2u64);
        let effective_at = U256::from(500u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, target, effective_at, U256::from(1u64), true)
            .unwrap();

        assert_eq!(
            last_event(&tok),
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: wad(),
                newMultiplier: target,
                effectiveAtTimestamp: effective_at,
            }
            .encode_log_data()
        );
        assert_eq!(LOGIC.new_ui_multiplier(&tok, U256::from(1u64)).unwrap(), target);
        assert_eq!(LOGIC.effective_at(&tok).unwrap(), effective_at);
    }

    #[test]
    fn set_ui_multiplier_requires_operator_role() {
        let mut tok = token();
        let err = LOGIC
            .set_ui_multiplier(&mut tok, ALICE, wad(), U256::from(2u64), U256::from(1u64), false)
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: AssetV1::OPERATOR_ROLE,
            })
        );
        // Once granted, the same call succeeds.
        grant_operator(&mut tok, ALICE);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, wad(), U256::from(2u64), U256::from(1u64), false)
            .unwrap();
    }

    #[test]
    fn set_ui_multiplier_rejects_zero_and_above_uint128() {
        let mut tok = token();
        let zero = LOGIC
            .set_ui_multiplier(
                &mut tok,
                ALICE,
                U256::ZERO,
                U256::from(2u64),
                U256::from(1u64),
                true,
            )
            .unwrap_err();
        assert_eq!(zero, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));

        let too_big = U256::from(u128::MAX) + U256::ONE;
        let over = LOGIC
            .set_ui_multiplier(&mut tok, ALICE, too_big, U256::from(2u64), U256::from(1u64), true)
            .unwrap_err();
        assert_eq!(over, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
    }

    #[test]
    fn set_ui_multiplier_rejects_effective_at_in_past_and_too_far() {
        let mut tok = token();
        let now = U256::from(100u64);
        // effectiveAt == now is not in the future.
        let past = LOGIC.set_ui_multiplier(&mut tok, ALICE, wad(), now, now, true).unwrap_err();
        assert_eq!(
            past,
            BasePrecompileError::revert(IB20Asset::EffectiveAtInPast { effectiveAt: now })
        );

        let too_far = U256::from(u64::MAX) + U256::ONE;
        let far = LOGIC.set_ui_multiplier(&mut tok, ALICE, wad(), too_far, now, true).unwrap_err();
        assert_eq!(
            far,
            BasePrecompileError::revert(IB20Asset::EffectiveAtTooFar { effectiveAt: too_far })
        );
    }

    #[test]
    fn set_ui_multiplier_reverts_on_live_overlap() {
        let mut tok = token();
        let first_effective_at = U256::from(1_000u64);
        LOGIC
            .set_ui_multiplier(
                &mut tok,
                ALICE,
                wad() * U256::from(2u64),
                first_effective_at,
                U256::from(1u64),
                true,
            )
            .unwrap();
        let err = LOGIC
            .set_ui_multiplier(
                &mut tok,
                ALICE,
                wad() * U256::from(3u64),
                U256::from(2_000u64),
                U256::from(1u64),
                true,
            )
            .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20Asset::ScheduleOverlap {
                pendingEffectiveAt: first_effective_at,
            })
        );
    }

    #[test]
    fn set_ui_multiplier_materializes_matured_pending() {
        let mut tok = token();
        let first = wad() * U256::from(2u64);
        let first_effective_at = U256::from(100u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, first, first_effective_at, U256::from(1u64), true)
            .unwrap();

        // After maturity, schedule a second: the matured first must fold into the current slot.
        let now = U256::from(150u64);
        assert_eq!(LOGIC.multiplier(&tok, now).unwrap(), first, "first has matured");
        let second = wad() * U256::from(3u64);
        let second_effective_at = U256::from(300u64);
        LOGIC.set_ui_multiplier(&mut tok, ALICE, second, second_effective_at, now, true).unwrap();

        // `old` in the emitted event is the folded (matured) value, not the pre-fold WAD.
        assert_eq!(
            last_event(&tok),
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: first,
                newMultiplier: second,
                effectiveAtTimestamp: second_effective_at,
            }
            .encode_log_data()
        );
        // Slot 1 (current) now holds the folded matured multiplier and is still effective.
        assert_eq!(tok.accounting().multiplier().unwrap(), first);
        assert_eq!(LOGIC.multiplier(&tok, now).unwrap(), first);
        // The second flips in on maturity.
        assert_eq!(LOGIC.multiplier(&tok, second_effective_at).unwrap(), second);
    }

    // --- cancelScheduledMultiplier ---

    #[test]
    fn cancel_clears_pending_and_emits() {
        let mut tok = token();
        let target = wad() * U256::from(2u64);
        let effective_at = U256::from(1_000u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, target, effective_at, U256::from(1u64), true)
            .unwrap();

        LOGIC.cancel_scheduled_multiplier(&mut tok, ALICE, U256::from(1u64), true).unwrap();

        assert_eq!(
            last_event(&tok),
            IB20Asset::MultiplierUpdateCancelled {
                cancelledMultiplier: target,
                cancelledEffectiveAt: effective_at,
            }
            .encode_log_data()
        );
        assert_eq!(LOGIC.effective_at(&tok).unwrap(), U256::ZERO);
        // No-live-pending invariant.
        assert_eq!(
            LOGIC.new_ui_multiplier(&tok, U256::from(1u64)).unwrap(),
            LOGIC.ui_multiplier(&tok, U256::from(1u64)).unwrap()
        );
    }

    #[test]
    fn cancel_reverts_without_live_pending() {
        let mut tok = token();
        let none =
            LOGIC.cancel_scheduled_multiplier(&mut tok, ALICE, U256::from(1u64), true).unwrap_err();
        assert_eq!(none, BasePrecompileError::revert(IB20Asset::NoScheduledMultiplier {}));

        // A matured pending is no longer "live", so cancel still reverts.
        LOGIC
            .set_ui_multiplier(
                &mut tok,
                ALICE,
                wad() * U256::from(2u64),
                U256::from(100u64),
                U256::from(1u64),
                true,
            )
            .unwrap();
        let matured = LOGIC
            .cancel_scheduled_multiplier(&mut tok, ALICE, U256::from(100u64), true)
            .unwrap_err();
        assert_eq!(matured, BasePrecompileError::revert(IB20Asset::NoScheduledMultiplier {}));
    }

    // --- updateMultiplier (instant failsafe) ---

    #[test]
    fn update_multiplier_emits_ui_event_at_now() {
        let mut tok = token();
        let now = U256::from(42u64);
        let target = wad() * U256::from(5u64);
        LOGIC.update_multiplier(&mut tok, ALICE, target, now, true).unwrap();
        assert_eq!(
            last_event(&tok),
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: wad(),
                newMultiplier: target,
                effectiveAtTimestamp: now,
            }
            .encode_log_data()
        );
        assert_eq!(LOGIC.multiplier(&tok, now).unwrap(), target);
    }

    #[test]
    fn update_multiplier_clears_live_pending_with_cancel_event() {
        let mut tok = token();
        let pending = wad() * U256::from(2u64);
        let pending_effective_at = U256::from(1_000u64);
        LOGIC
            .set_ui_multiplier(
                &mut tok,
                ALICE,
                pending,
                pending_effective_at,
                U256::from(1u64),
                true,
            )
            .unwrap();

        let now = U256::from(10u64);
        let instant = wad() * U256::from(5u64);
        LOGIC.update_multiplier(&mut tok, ALICE, instant, now, true).unwrap();

        let events = &tok.accounting().events;
        assert_eq!(
            events[events.len() - 2],
            IB20Asset::MultiplierUpdateCancelled {
                cancelledMultiplier: pending,
                cancelledEffectiveAt: pending_effective_at,
            }
            .encode_log_data()
        );
        assert_eq!(
            events[events.len() - 1],
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: wad(),
                newMultiplier: instant,
                effectiveAtTimestamp: now,
            }
            .encode_log_data()
        );
        assert_eq!(LOGIC.effective_at(&tok).unwrap(), U256::ZERO);
        assert_eq!(LOGIC.multiplier(&tok, now).unwrap(), instant);
    }

    #[test]
    fn update_multiplier_clears_matured_pending_without_cancel_event() {
        let mut tok = token();
        let matured = wad() * U256::from(2u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, matured, U256::from(100u64), U256::from(1u64), true)
            .unwrap();

        let now = U256::from(150u64); // matured
        let instant = wad() * U256::from(5u64);
        LOGIC.update_multiplier(&mut tok, ALICE, instant, now, true).unwrap();

        // Only UIMultiplierUpdated is emitted (matured folds into `old` silently); `old` is the
        // matured effective value.
        assert_eq!(
            last_event(&tok),
            IB20Asset::UIMultiplierUpdated {
                oldMultiplier: matured,
                newMultiplier: instant,
                effectiveAtTimestamp: now,
            }
            .encode_log_data()
        );
        let cancelled_sig = IB20Asset::MultiplierUpdateCancelled::SIGNATURE_HASH;
        assert!(
            !tok.accounting().events.iter().any(|log| log.topics()[0] == cancelled_sig),
            "no cancellation event for a matured pending"
        );
        assert_eq!(LOGIC.effective_at(&tok).unwrap(), U256::ZERO);
    }

    #[test]
    fn update_multiplier_rejects_zero_and_above_uint128() {
        let mut tok = token();
        let zero = LOGIC
            .update_multiplier(&mut tok, ALICE, U256::ZERO, U256::from(1u64), true)
            .unwrap_err();
        assert_eq!(zero, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
        let over = LOGIC
            .update_multiplier(
                &mut tok,
                ALICE,
                U256::from(u128::MAX) + U256::ONE,
                U256::from(1u64),
                true,
            )
            .unwrap_err();
        assert_eq!(over, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
    }

    // --- newUIMultiplier / effectiveAt no-pending & matured semantics ---

    #[test]
    fn new_ui_multiplier_matured_mirrors_ui_multiplier_and_keeps_past_effective_at() {
        let mut tok = token();
        let target = wad() * U256::from(2u64);
        let effective_at = U256::from(100u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, target, effective_at, U256::from(1u64), true)
            .unwrap();

        let now = U256::from(150u64);
        assert_eq!(
            LOGIC.new_ui_multiplier(&tok, now).unwrap(),
            LOGIC.ui_multiplier(&tok, now).unwrap()
        );
        assert_eq!(LOGIC.new_ui_multiplier(&tok, now).unwrap(), target);
        // A matured pending keeps its (now past) effectiveAt until a set/cancel materializes it.
        assert_eq!(LOGIC.effective_at(&tok).unwrap(), effective_at);
    }

    // --- scaled reads route through the effective multiplier ---

    #[test]
    fn scaled_reads_use_effective_multiplier() {
        let mut tok = token();
        tok.accounting_mut().set_balance(ALICE, U256::from(100u64)).unwrap();
        tok.accounting_mut().set_total_supply(U256::from(100u64)).unwrap();
        let target = wad() * U256::from(2u64);
        let effective_at = U256::from(100u64);
        LOGIC
            .set_ui_multiplier(&mut tok, ALICE, target, effective_at, U256::from(1u64), true)
            .unwrap();

        // Before maturity: 1:1.
        let before = U256::from(50u64);
        assert_eq!(
            LOGIC.to_scaled_balance(&tok, U256::from(10u64), before).unwrap(),
            U256::from(10u64)
        );
        assert_eq!(LOGIC.scaled_balance_of(&tok, ALICE, before).unwrap(), U256::from(100u64));
        assert_eq!(LOGIC.balance_of_ui(&tok, ALICE, before).unwrap(), U256::from(100u64));
        assert_eq!(LOGIC.total_supply_ui(&tok, before).unwrap(), U256::from(100u64));

        // After maturity: doubled.
        let after = U256::from(100u64);
        assert_eq!(
            LOGIC.to_scaled_balance(&tok, U256::from(10u64), after).unwrap(),
            U256::from(20u64)
        );
        assert_eq!(
            LOGIC.to_raw_balance(&tok, U256::from(20u64), after).unwrap(),
            U256::from(10u64)
        );
        assert_eq!(LOGIC.scaled_balance_of(&tok, ALICE, after).unwrap(), U256::from(200u64));
        assert_eq!(LOGIC.balance_of_ui(&tok, ALICE, after).unwrap(), U256::from(200u64));
        assert_eq!(LOGIC.total_supply_ui(&tok, after).unwrap(), U256::from(200u64));
    }

    // --- ERC-165 ---

    #[test]
    fn supports_interface_advertises_claimed_ids_only() {
        // IERC165, IScaledUIAmount, IScaledUIAmountNewUIMultiplier, IScaledUIAmountBalances.
        for id in [
            [0x01, 0xff, 0xc9, 0xa7],
            [0xa6, 0x0b, 0xf1, 0x3d],
            [0x4b, 0xd2, 0x76, 0x48],
            [0xd8, 0x90, 0xfd, 0x71],
        ] {
            assert!(<AssetV2 as Asset<InMemoryTokenAccounting, FakePolicyAccounting>>::supports_interface(
                &LOGIC,
                FixedBytes::new(id)
            ));
        }
        // Conversion extension is NOT claimed; nor is an arbitrary id.
        for id in [[0x57, 0x85, 0x4f, 0xc3], [0xde, 0xad, 0xbe, 0xef], [0xff, 0xff, 0xff, 0xff]] {
            assert!(!<AssetV2 as Asset<InMemoryTokenAccounting, FakePolicyAccounting>>::supports_interface(
                &LOGIC,
                FixedBytes::new(id)
            ));
        }
    }

    // --- unchanged behavior still delegates to V1 ---

    #[test]
    fn transfer_delegates_to_v1() {
        let mut tok = token();
        tok.accounting_mut().set_balance(ALICE, U256::from(100u64)).unwrap();
        LOGIC.transfer(&mut tok, ALICE, BOB, U256::from(30u64), true).unwrap();
        assert_eq!(tok.accounting().balance_of(ALICE).unwrap(), U256::from(70u64));
        assert_eq!(tok.accounting().balance_of(BOB).unwrap(), U256::from(30u64));
        assert_eq!(last_event(&tok).topics()[0], IB20::Transfer::SIGNATURE_HASH);
    }
}
