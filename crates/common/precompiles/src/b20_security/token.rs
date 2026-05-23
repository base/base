//! `B20SecurityToken` struct — the security B-20 token type.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use super::{
    IB20Security,
    accounting::SecurityAccounting,
    ids::{BURN_FROM_ROLE, REDEEM_SENDER_POLICY, SECURITY_OPERATOR_ROLE},
};
use crate::{
    B20Guards, B20PolicyType, B20TokenRole, Burnable, Configurable, IB20, Mintable, Pausable,
    Permittable, Policy, RoleManaged, Token, Transferable,
};

/// WAD precision for share ratio arithmetic: 1e18.
pub(super) const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// EVM precompile for the security B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: SecurityAccounting`
/// so the dispatch layer can read and write security-specific storage (share ratio,
/// security identifiers, announcement IDs). The `in_announcement` flag guards against
/// recursive `announce` calls within a single precompile invocation.
#[derive(Debug, Clone)]
pub struct B20SecurityToken<S: SecurityAccounting, P: Policy> {
    pub(super) accounting: S,
    pub(super) policy: P,
    pub(super) in_announcement: bool,
}

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Creates a `B20SecurityToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy, in_announcement: false }
    }

    /// Updates the share-to-tokens ratio. Requires `SECURITY_OPERATOR_ROLE`.
    pub fn update_share_ratio(
        &mut self,
        caller: Address,
        new_shares_to_tokens_ratio: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role::<Self>(self, caller, SECURITY_OPERATOR_ROLE)?;
        }
        self.accounting_mut().set_shares_to_tokens_ratio(new_shares_to_tokens_ratio)?;
        self.accounting_mut().emit_event(
            IB20Security::ShareRatioUpdated { sharesToTokensRatio: new_shares_to_tokens_ratio }
                .encode_log_data(),
        )
    }

    /// Mints tokens to multiple recipients. All-or-nothing.
    pub fn batch_mint(
        &mut self,
        caller: Address,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        if recipients.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        if recipients.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(recipients.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        for (recipient, amount) in recipients.into_iter().zip(amounts) {
            self.mint(caller, recipient, amount, privileged)?;
        }
        Ok(())
    }

    /// Burns tokens from multiple accounts unconditionally. All-or-nothing.
    ///
    /// Unlike `burnBlocked`, this path has no policy precondition; `BURN_FROM_ROLE` is the
    /// on-chain authorization.
    pub fn batch_burn(
        &mut self,
        caller: Address,
        accounts: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role::<Self>(self, caller, BURN_FROM_ROLE)?;
        }
        if accounts.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(accounts.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if accounts.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        if !privileged {
            B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
        }
        for (account, amount) in accounts.into_iter().zip(amounts) {
            if amount.is_zero() {
                return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
            }
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
            self.accounting_mut().set_total_supply(supply.saturating_sub(amount))?;
            self.accounting_mut().emit_event(
                IB20::Transfer { from: account, to: Address::ZERO, amount }.encode_log_data(),
            )?;
        }
        Ok(())
    }

    /// Posts an announcement and atomically executes `internal_calls` via self-dispatch.
    ///
    /// The `in_announcement` flag prevents recursive `announce` calls within a single
    /// precompile invocation.
    pub fn announce(
        &mut self,
        ctx: StorageCtx<'_>,
        internal_calls: Vec<Bytes>,
        id: String,
        description: String,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        if self.in_announcement {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
        }

        let caller = ctx.caller();
        if !privileged {
            B20Guards::ensure_role::<Self>(self, caller, SECURITY_OPERATOR_ROLE)?;
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

        self.in_announcement = true;

        for call in &internal_calls {
            let call_bytes: &[u8] = call.as_ref();
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Security::InternalCallMalformed {
                    call: call.clone(),
                }));
            }
            // `in_announcement == true` causes recursive announce calls to revert via the
            // guard at the top of this function. No separate selector check needed.
            self.inner(ctx, call_bytes).map_err(|_| {
                BasePrecompileError::revert(IB20Security::InternalCallFailed { call: call.clone() })
            })?;
        }

        self.accounting_mut().emit_event(IB20Security::EndAnnouncement { id }.encode_log_data())
    }

    /// Ensures `policy_type` names either an inherited B-20 policy slot or the security redeem slot.
    pub fn ensure_supported_policy_type(policy_type: B256) -> Result<()> {
        if B20PolicyType::from_id(policy_type).is_some() || policy_type == REDEEM_SENDER_POLICY {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_type,
            }))
        }
    }

    /// Returns the configured policy ID for `policy_type`.
    pub fn policy_id(&self, policy_type: B256) -> Result<u64> {
        Self::ensure_supported_policy_type(policy_type)?;
        self.accounting().policy_id(policy_type)
    }

    /// Updates the configured policy ID for `policy_type`.
    pub fn update_policy(
        &mut self,
        caller: Address,
        policy_type: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> Result<()> {
        Self::ensure_supported_policy_type(policy_type)?;
        if !privileged {
            self.ensure_role(caller, Self::default_admin_role())?;
        }
        let old_policy_id = self.policy_id(policy_type)?;
        if !self.policy().policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        self.accounting_mut().set_policy_id(policy_type, new_policy_id)?;
        self.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_type,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }

    /// Converts a token balance to shares: `balance * sharesToTokensRatio / WAD`.
    pub fn to_shares(&self, balance: U256) -> Result<U256> {
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        Ok(balance.saturating_mul(ratio) / WAD)
    }

    /// Performs a security-specific redeem: share-based floor check, burn, security `Redeemed` event.
    pub fn security_redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        self.security_redeem_inner(caller, amount, None)
    }

    /// [`Self::security_redeem`] with a memo emitted between `Transfer` and `Redeemed`.
    pub fn security_redeem_with_memo(
        &mut self,
        caller: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        self.security_redeem_inner(caller, amount, Some(memo))
    }

    /// Sets a new minimum-redeemable threshold in shares. Requires `DEFAULT_ADMIN_ROLE`.
    pub fn update_minimum_redeemable(
        &mut self,
        caller: Address,
        new_minimum_redeemable: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::DefaultAdmin)?;
        }
        self.accounting_mut().set_minimum_redeemable(new_minimum_redeemable)?;
        self.accounting_mut().emit_event(
            IB20Security::MinimumRedeemableUpdated {
                caller,
                newMinimumRedeemable: new_minimum_redeemable,
            }
            .encode_log_data(),
        )
    }

    /// Writes (or removes when `value` is empty) a security identifier. Requires `SECURITY_OPERATOR_ROLE`.
    pub fn update_security_identifier(
        &mut self,
        caller: Address,
        identifier_type: String,
        value: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_role::<Self>(self, caller, SECURITY_OPERATOR_ROLE)?;
        }
        if identifier_type.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::InvalidIdentifierType {}));
        }
        self.accounting_mut()
            .set_security_identifier_value(identifier_type.as_str(), value.clone())?;
        self.accounting_mut().emit_event(
            IB20Security::SecurityIdentifierUpdated { identifierType: identifier_type, value }
                .encode_log_data(),
        )
    }

    fn security_redeem_inner(
        &mut self,
        caller: Address,
        amount: U256,
        memo: Option<B256>,
    ) -> Result<()> {
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::REDEEM)?;
        B20Guards::ensure_policy::<Self>(self, REDEEM_SENDER_POLICY, caller)?;
        if amount.is_zero() {
            return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
        }
        let ratio = self.accounting().shares_to_tokens_ratio()?;
        let shares = amount.saturating_mul(ratio) / WAD;
        let minimum = self.accounting().minimum_redeemable()?;
        if shares == U256::ZERO || shares < minimum {
            return Err(BasePrecompileError::revert(IB20Security::BelowMinimumRedeemable {
                shares,
                minimum,
            }));
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
        self.accounting_mut().set_total_supply(supply.saturating_sub(amount))?;
        self.accounting_mut().emit_event(
            IB20::Transfer { from: caller, to: Address::ZERO, amount }.encode_log_data(),
        )?;
        if let Some(memo) = memo {
            self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())?;
        }
        self.accounting_mut().emit_event(
            IB20Security::Redeemed { from: caller, amt: amount, sharesToTokensRatio: ratio }
                .encode_log_data(),
        )
    }
}

impl<S: SecurityAccounting, P: Policy> Token for B20SecurityToken<S, P> {
    type Accounting = S;
    type Policy = P;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &P {
        &self.policy
    }

    fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}

impl<S: SecurityAccounting, P: Policy> Transferable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Mintable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Burnable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Pausable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Configurable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Permittable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> RoleManaged for B20SecurityToken<S, P> {}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::{BasePrecompileError, StorageCtx, setup_storage};

    use super::*;
    use crate::{
        B20PausableFeature, B20TokenRole, IB20, PolicyHandle, Token, TokenAccounting,
        b20_security::{B20SecurityStorage, SecurityAccounting},
        common::test_utils::{InMemoryPolicy, InMemoryTokenAccounting},
    };

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.roles.insert((SECURITY_OPERATOR_ROLE, ALICE), true);
        accounting.roles.insert((B20TokenRole::Mint.id(), ALICE), true);
        accounting.roles.insert((BURN_FROM_ROLE, ALICE), true);
        accounting
            .policy_ids
            .insert(REDEEM_SENDER_POLICY, crate::PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn with_ctx<R>(caller: Address, f: impl FnOnce(StorageCtx<'_>) -> R) -> R {
        let (mut storage, _) = setup_storage();
        storage.set_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| f(ctx))
    }

    #[test]
    fn update_share_ratio_updates_storage_and_emits_event() {
        let mut token = make_token();
        let new_ratio = WAD * U256::from(3u64);

        token.update_share_ratio(ALICE, new_ratio, false).unwrap();

        assert_eq!(token.accounting().shares_to_tokens_ratio().unwrap(), new_ratio);
        assert_eq!(token.accounting().events.len(), 1);
        assert_eq!(
            token.accounting().events[0],
            IB20Security::ShareRatioUpdated { sharesToTokensRatio: new_ratio }.encode_log_data()
        );
    }

    #[test]
    fn update_share_ratio_rejects_missing_security_operator_role() {
        let mut token = make_token();
        let new_ratio = WAD * U256::from(2u64);

        assert_eq!(
            token.update_share_ratio(BOB, new_ratio, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: BOB,
                neededRole: SECURITY_OPERATOR_ROLE,
            })
        );
        assert_eq!(token.accounting().shares_to_tokens_ratio().unwrap(), WAD);
        assert_eq!(token.accounting().events.len(), 0);
    }

    #[test]
    fn batch_mint_increases_balances() {
        let mut token = make_token();
        token
            .batch_mint(
                ALICE,
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
                false,
            )
            .unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(200u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn batch_mint_rejects_empty() {
        let mut token = make_token();
        assert!(token.batch_mint(ALICE, alloc::vec![], alloc::vec![], false).is_err());
    }

    #[test]
    fn batch_mint_rejects_length_mismatch() {
        let mut token = make_token();
        assert!(
            token
                .batch_mint(ALICE, alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE], false)
                .is_err()
        );
    }

    #[test]
    fn batch_mint_zero_amount_succeeds() {
        let mut token = make_token();

        token.batch_mint(ALICE, alloc::vec![ALICE], alloc::vec![U256::ZERO], false).unwrap();
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::ZERO);
    }

    #[test]
    fn batch_mint_rejects_missing_mint_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(B20TokenRole::Mint.id(), ALICE));

        assert_eq!(
            token
                .batch_mint(ALICE, alloc::vec![BOB], alloc::vec![U256::from(1u64)], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
    }

    #[test]
    fn batch_burn_decrements_balances() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(500u64));
        token.accounting_mut().total_supply = U256::from(500u64);

        token
            .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::from(200u64)], false)
            .unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn batch_burn_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        assert!(
            token
                .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::from(100u64)], false)
                .is_err()
        );
    }

    #[test]
    fn batch_burn_rejects_missing_burn_from_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(BURN_FROM_ROLE, ALICE));
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            token
                .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::from(1u64)], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: BURN_FROM_ROLE,
            })
        );
    }

    #[test]
    fn batch_burn_rejects_empty() {
        let mut token = make_token();
        assert_eq!(
            token.batch_burn(ALICE, alloc::vec![], alloc::vec![], false).unwrap_err(),
            BasePrecompileError::revert(IB20Security::EmptyBatch {})
        );
    }

    #[test]
    fn batch_burn_rejects_length_mismatch() {
        let mut token = make_token();
        assert_eq!(
            token
                .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );
        assert_eq!(
            token.batch_burn(ALICE, alloc::vec![], alloc::vec![U256::ONE], false).unwrap_err(),
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ZERO,
                rightLen: U256::ONE,
            })
        );
    }

    #[test]
    fn batch_burn_validates_batch_shape_before_pause() {
        let mut token = make_token();
        token.accounting_mut().paused =
            crate::B20PausableFeature::mask(IB20::PausableFeature::BURN);

        assert_eq!(
            token
                .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );
        assert_eq!(
            token.batch_burn(ALICE, alloc::vec![], alloc::vec![], false).unwrap_err(),
            BasePrecompileError::revert(IB20Security::EmptyBatch {})
        );
    }

    #[test]
    fn batch_burn_rejects_zero_amount() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            token
                .batch_burn(ALICE, alloc::vec![ALICE], alloc::vec![U256::ZERO], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidAmount {})
        );
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 0);
    }

    #[test]
    fn batch_burn_multiple_accounts_emits_one_transfer_each() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().balances.insert(BOB, U256::from(200u64));
        token.accounting_mut().total_supply = U256::from(300u64);
        token
            .batch_burn(
                ALICE,
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
                false,
            )
            .unwrap();
        assert_eq!(token.accounting().events.len(), 2);
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
    }

    #[test]
    fn announcement_id_not_used_initially() {
        let token = make_token();
        let id = "2026-Q1-split";
        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
    }

    #[test]
    fn announce_marks_id_used_and_emits_bookend_events() {
        let mut token = make_token();
        let id = "2026-Q1-split".to_string();
        let description = "Q1 split".to_string();
        let uri = "https://example.com".to_string();

        with_ctx(ALICE, |ctx| {
            assert!(!token.accounting().is_announcement_id_used(&id).unwrap());
            token
                .announce(ctx, vec![], id.clone(), description.clone(), uri.clone(), false)
                .unwrap();
        });

        assert!(token.accounting().is_announcement_id_used(&id).unwrap());
        assert_eq!(token.accounting().events.len(), 2);
        assert_eq!(
            token.accounting().events[0],
            IB20Security::Announcement { caller: ALICE, id: id.clone(), description, uri }
                .encode_log_data()
        );
        assert_eq!(
            token.accounting().events[1],
            IB20Security::EndAnnouncement { id }.encode_log_data()
        );
    }

    #[test]
    fn announce_rejects_consumed_id() {
        let mut token = make_token();
        let id = "2026-Q1-split".to_string();
        token.accounting_mut().mark_announcement_id_used(&id).unwrap();

        assert_eq!(
            with_ctx(ALICE, |ctx| {
                token.announce(
                    ctx,
                    vec![],
                    id.clone(),
                    "Q1 split".into(),
                    "https://x".into(),
                    false,
                )
            })
            .unwrap_err(),
            BasePrecompileError::revert(IB20Security::AnnouncementIdAlreadyUsed { id })
        );
    }

    #[test]
    fn announce_rejects_missing_security_operator_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));

        assert_eq!(
            with_ctx(ALICE, |ctx| {
                token.announce(
                    ctx,
                    vec![],
                    "2026-Q1-split".into(),
                    "Q1 split".into(),
                    "https://x".into(),
                    false,
                )
            })
            .unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: SECURITY_OPERATOR_ROLE,
            })
        );
    }

    #[test]
    fn to_shares_one_to_one_ratio() {
        let token = make_token();
        assert_eq!(token.to_shares(U256::from(100u64)).unwrap(), U256::from(100u64));
    }

    #[test]
    fn to_shares_two_to_one_ratio() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = WAD * U256::from(2u64);
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_shares(U256::from(50u64)).unwrap(), U256::from(100u64));
    }

    #[test]
    fn to_shares_zero_balance_yields_zero() {
        let token = make_token();
        assert_eq!(token.to_shares(U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn to_shares_sub_wad_ratio_truncates_to_zero() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = WAD / U256::from(2u64);
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_shares(U256::from(1u64)).unwrap(), U256::ZERO);
    }

    #[test]
    fn shares_of_derives_from_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(75u64));
        let balance = token.accounting().balance_of(ALICE).unwrap();
        assert_eq!(token.to_shares(balance).unwrap(), U256::from(75u64));
    }

    #[test]
    fn security_redeem_burns_and_emits_security_event() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem(ALICE, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn security_redeem_rejects_below_minimum_shares() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(10u64);

        assert!(token.security_redeem(ALICE, U256::from(5u64)).is_err());
    }

    #[test]
    fn security_redeem_rejects_zero_shares() {
        let mut token = make_token();
        token.accounting_mut().shares_to_tokens_ratio = WAD / U256::from(2u64);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert!(token.security_redeem(ALICE, U256::ONE).is_err());
    }

    #[test]
    fn security_redeem_rejects_when_redeem_feature_paused() {
        let mut token = make_token();
        token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::REDEEM);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            token.security_redeem(ALICE, U256::from(1u64)).unwrap_err(),
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::REDEEM,
            })
        );
    }

    #[test]
    fn security_redeem_rejects_when_sender_policy_denies() {
        let policy_id = 7;
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.balances.insert(ALICE, U256::from(100u64));
        accounting.total_supply = U256::from(100u64);
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, policy_id);
        let mut policy = InMemoryPolicy::new();
        policy.create_existing_policy(policy_id);
        let mut token = TestSecurityToken::with_storage_and_policy(accounting, policy);

        assert_eq!(
            token.security_redeem(ALICE, U256::from(1u64)).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: REDEEM_SENDER_POLICY,
                policyId: policy_id,
            })
        );
    }

    #[test]
    fn security_redeem_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        token.accounting_mut().total_supply = U256::from(10u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        assert!(token.security_redeem(ALICE, U256::from(100u64)).is_err());
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(10u64));
    }

    #[test]
    fn security_redeem_rejects_zero_amount() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        token.accounting_mut().total_supply = U256::from(10u64);

        assert_eq!(
            token.security_redeem(ALICE, U256::ZERO).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidAmount {})
        );
    }

    #[test]
    fn security_redeem_at_exact_minimum_succeeds() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);
        token.accounting_mut().minimum_redeemable = U256::from(5u64);

        token.security_redeem(ALICE, U256::from(5u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(45u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(45u64));
    }

    #[test]
    fn security_redeem_with_non_unit_ratio_applies_correct_share_math() {
        let mut token = make_token();
        token.accounting_mut().shares_to_tokens_ratio = WAD * U256::from(2u64);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(10u64);

        assert!(token.security_redeem(ALICE, U256::from(4u64)).is_err());
        token.security_redeem(ALICE, U256::from(5u64)).unwrap();
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(95u64));
    }

    #[test]
    fn security_redeem_emits_transfer_then_redeemed() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem(ALICE, U256::from(10u64)).unwrap();

        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn security_redeem_with_memo_emits_memo_before_redeemed() {
        let mut token = make_token();
        let amount = U256::from(10u64);
        let memo = B256::repeat_byte(0x42);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);

        token.security_redeem_with_memo(ALICE, amount, memo).unwrap();

        assert_eq!(
            token.accounting().events[0],
            IB20::Transfer { from: ALICE, to: Address::ZERO, amount }.encode_log_data()
        );
        assert_eq!(
            token.accounting().events[1],
            IB20::Memo { caller: ALICE, memo }.encode_log_data()
        );
        assert_eq!(
            token.accounting().events[2],
            IB20Security::Redeemed { from: ALICE, amt: amount, sharesToTokensRatio: WAD }
                .encode_log_data()
        );
    }

    #[test]
    fn storage_backed_redeem_uses_wad_when_share_ratio_slot_is_unset() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(TOKEN, ctx),
                PolicyHandle::new(ctx),
            );
            token.accounting_mut().set_balance(ALICE, U256::from(100u64)).unwrap();
            token.accounting_mut().set_total_supply(U256::from(100u64)).unwrap();
            token.accounting_mut().set_minimum_redeemable(U256::from(10u64)).unwrap();

            assert_eq!(token.accounting().shares_to_tokens_ratio().unwrap(), WAD);
            token.security_redeem(ALICE, U256::from(10u64)).unwrap();

            assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(90u64));
            assert_eq!(token.accounting().total_supply().unwrap(), U256::from(90u64));
        });
    }

    #[test]
    fn security_identifier_roundtrip() {
        let mut token = make_token();

        assert_eq!(token.accounting().security_identifier("ISIN").unwrap(), "");
        token
            .accounting_mut()
            .set_security_identifier_value("ISIN", "US0000000000".to_string())
            .unwrap();
        assert_eq!(
            token.accounting().security_identifier("ISIN").unwrap(),
            "US0000000000".to_string()
        );
    }

    #[test]
    fn security_identifier_missing_key_returns_empty() {
        let token = make_token();
        assert_eq!(token.accounting().security_identifier("CUSIP").unwrap(), "");
    }

    #[test]
    fn security_identifier_empty_value_clears_entry() {
        let mut token = make_token();
        token
            .accounting_mut()
            .set_security_identifier_value("FIGI", "BBG000B9XRY4".to_string())
            .unwrap();
        assert_eq!(token.accounting().security_identifier("FIGI").unwrap(), "BBG000B9XRY4");
        token.accounting_mut().set_security_identifier_value("FIGI", String::new()).unwrap();
        assert_eq!(token.accounting().security_identifier("FIGI").unwrap(), "");
    }

    #[test]
    fn minimum_redeemable_persists() {
        let mut token = make_token();
        let floor = U256::from(42u64);
        token.accounting_mut().set_minimum_redeemable(floor).unwrap();
        assert_eq!(token.accounting().minimum_redeemable().unwrap(), floor);
    }

    // --- privileged bypass tests ---

    #[test]
    fn privileged_update_share_ratio_bypasses_security_operator_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));
        let new_ratio = WAD * U256::from(2u64);

        token.update_share_ratio(ALICE, new_ratio, true).unwrap();

        assert_eq!(token.accounting().shares_to_tokens_ratio().unwrap(), new_ratio);
    }

    #[test]
    fn non_privileged_update_share_ratio_rejects_missing_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));

        assert_eq!(
            token.update_share_ratio(ALICE, WAD, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: SECURITY_OPERATOR_ROLE,
            })
        );
    }

    #[test]
    fn privileged_batch_mint_bypasses_mint_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(B20TokenRole::Mint.id(), ALICE));

        token.batch_mint(ALICE, alloc::vec![BOB], alloc::vec![U256::from(10u64)], true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(10u64));
    }

    #[test]
    fn privileged_batch_burn_bypasses_burn_from_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(BURN_FROM_ROLE, ALICE));
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        token.batch_burn(ALICE, alloc::vec![BOB], alloc::vec![U256::from(10u64)], true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(40u64));
    }

    #[test]
    fn privileged_batch_burn_bypasses_burn_feature_paused() {
        let mut token = make_token();
        token.accounting_mut().paused =
            crate::B20PausableFeature::mask(IB20::PausableFeature::BURN);
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        token.batch_burn(ALICE, alloc::vec![BOB], alloc::vec![U256::from(10u64)], true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(40u64));
    }

    #[test]
    fn non_privileged_batch_burn_rejects_missing_burn_from_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(BURN_FROM_ROLE, ALICE));
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        assert_eq!(
            token
                .batch_burn(ALICE, alloc::vec![BOB], alloc::vec![U256::from(1u64)], false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: BURN_FROM_ROLE,
            })
        );
    }

    #[test]
    fn privileged_announce_bypasses_security_operator_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));
        let id = "2026-Q2-split".to_string();

        with_ctx(ALICE, |ctx| {
            token
                .announce(ctx, vec![], id.clone(), "Q2 split".into(), "https://x".into(), true)
                .unwrap();
        });

        assert!(token.accounting().is_announcement_id_used(&id).unwrap());
    }

    #[test]
    fn privileged_update_minimum_redeemable_bypasses_admin_role() {
        let mut token = make_token();
        let floor = U256::from(99u64);

        token.update_minimum_redeemable(BOB, floor, true).unwrap();

        assert_eq!(token.accounting().minimum_redeemable().unwrap(), floor);
    }

    #[test]
    fn non_privileged_update_minimum_redeemable_rejects_missing_admin_role() {
        let mut token = make_token();

        assert_eq!(
            token.update_minimum_redeemable(BOB, U256::from(1u64), false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: BOB,
                neededRole: B20TokenRole::DefaultAdmin.id(),
            })
        );
    }

    #[test]
    fn privileged_update_security_identifier_bypasses_security_operator_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));

        token
            .update_security_identifier(ALICE, "ISIN".into(), "US0000000001".into(), true)
            .unwrap();

        assert_eq!(
            token.accounting().security_identifier("ISIN").unwrap(),
            "US0000000001".to_string()
        );
    }

    #[test]
    fn non_privileged_update_security_identifier_rejects_missing_role() {
        let mut token = make_token();
        token.accounting_mut().roles.remove(&(SECURITY_OPERATOR_ROLE, ALICE));

        assert_eq!(
            token
                .update_security_identifier(ALICE, "ISIN".into(), "US0000000001".into(), false)
                .unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: SECURITY_OPERATOR_ROLE,
            })
        );
    }
}
