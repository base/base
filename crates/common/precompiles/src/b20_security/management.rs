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
    ///
    /// Revert priority: PAUSE → ROLE → INPUT VALIDATION → POLICY → BUSINESS LOGIC.
    fn batch_mint(
        &mut self,
        ctx: StorageCtx<'_>,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
        privileged: bool,
    ) -> Result<()> {
        // 1. PAUSE (Kill Switch)
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::MINT)?;
        // 2. ROLE
        let caller = ctx.caller();
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Mint)?;
        }
        // 3. INPUT VALIDATION
        if recipients.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(recipients.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        if recipients.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        // Per-element checks: InvalidReceiver → POLICY → SupplyCapExceeded
        for (recipient, amount) in recipients.into_iter().zip(amounts) {
            self.mint_inner(recipient, amount)?;
        }
        Ok(())
    }

    fn mint_inner(&mut self, to: Address, amount: U256) -> Result<()> {
        use alloy_sol_types::SolEvent;
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::MintReceiver, to)?;
        let supply = self.accounting().total_supply()?;
        let cap = self.accounting().supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        self.accounting_mut().set_total_supply(new_supply)?;
        let to_balance = self.accounting().balance_of(to)?;
        let new_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(to, new_balance)?;
        self.accounting_mut()
            .emit_event(IB20::Transfer { from: Address::ZERO, to, amount }.encode_log_data())
    }

    /// Burns tokens from multiple accounts unconditionally. All-or-nothing.
    ///
    /// Unlike `burnBlocked`, this path has no policy precondition. The
    /// `BURN_FROM_ROLE` authorization and burn pause check are the only gates.
    ///
    /// Revert priority: PAUSE → ROLE → INPUT VALIDATION → BUSINESS LOGIC.
    fn batch_burn(
        &mut self,
        ctx: StorageCtx<'_>,
        accounts: Vec<Address>,
        amounts: Vec<U256>,
    ) -> Result<()> {
        // 1. PAUSE (Kill Switch)
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
        // 2. ROLE
        let caller = ctx.caller();
        self.ensure_burn_from_role(caller)?;
        // 3. INPUT VALIDATION
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

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{Address, U256};
    use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};
    use rstest::rstest;

    use crate::{
        B20PausableFeature, B20PolicyType, B20SecurityStorage, B20SecurityToken, B20TokenRole,
        IB20, IB20Security, InMemoryPolicy, InMemoryTokenAccounting, PolicyRegistryStorage,
        SecurityManagement, Token,
    };

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const BURN_FROM_ROLE: alloy_primitives::B256 = B20TokenRole::BurnFrom.id();
    const MINT_ROLE: alloy_primitives::B256 = B20TokenRole::Mint.id();
    const REDEEM_SENDER_POLICY: alloy_primitives::B256 = B20PolicyType::RedeemSender.id();

    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn storage_with_caller(caller: Address) -> HashMapStorageProvider {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(caller);
        storage
    }

    #[rstest]
    #[case::pause_before_role(
        true,
        false,
        false,
        BasePrecompileError::revert(IB20::ContractPaused { feature: IB20::PausableFeature::BURN })
    )]
    #[case::role_before_length_mismatch(
        false,
        false,
        true,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: BURN_FROM_ROLE,
        })
    )]
    #[case::length_mismatch_before_balance(
        false,
        true,
        true,
        BasePrecompileError::revert(IB20Security::LengthMismatch {
            leftLen: U256::ONE,
            rightLen: U256::from(2u64),
        })
    )]
    fn batch_burn_revert_ordering(
        #[case] paused: bool,
        #[case] has_role: bool,
        #[case] length_mismatch: bool,
        #[case] expected_error: BasePrecompileError,
    ) {
        let mut token = make_token();
        if paused {
            token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::BURN);
        }
        if has_role {
            token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        }
        let mut storage = storage_with_caller(ALICE);
        let (accounts, amounts) = if length_mismatch {
            (vec![ALICE], vec![U256::ONE, U256::ONE])
        } else {
            (vec![ALICE], vec![U256::ONE])
        };

        let err = StorageCtx::enter(&mut storage, |ctx| token.batch_burn(ctx, accounts, amounts))
            .unwrap_err();

        assert_eq!(err, expected_error);
    }

    #[test]
    fn batch_burn_empty_batch_before_business_logic() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        let mut storage = storage_with_caller(ALICE);

        let err = StorageCtx::enter(&mut storage, |ctx| token.batch_burn(ctx, vec![], vec![]))
            .unwrap_err();

        assert_eq!(err, BasePrecompileError::revert(IB20Security::EmptyBatch {}));
    }

    #[rstest]
    #[case::pause_before_role(
        true,
        false,
        false,
        false,
        BasePrecompileError::revert(IB20::ContractPaused { feature: IB20::PausableFeature::MINT })
    )]
    #[case::role_before_length_mismatch(
        false,
        false,
        true,
        false,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: MINT_ROLE,
        })
    )]
    #[case::length_mismatch_before_policy(
        false,
        true,
        true,
        true,
        BasePrecompileError::revert(IB20Security::LengthMismatch {
            leftLen: U256::ONE,
            rightLen: U256::from(2u64),
        })
    )]
    fn batch_mint_revert_ordering(
        #[case] paused: bool,
        #[case] has_role: bool,
        #[case] length_mismatch: bool,
        #[case] policy_blocks: bool,
        #[case] expected_error: BasePrecompileError,
    ) {
        let mut token = make_token();
        if paused {
            token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::MINT);
        }
        if has_role {
            token.accounting_mut().roles.insert((MINT_ROLE, ALICE), true);
        }
        if policy_blocks {
            token.accounting_mut().policy_ids.insert(
                B20PolicyType::MintReceiver.id(),
                PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            );
        }
        let mut storage = storage_with_caller(ALICE);
        let (recipients, amounts) = if length_mismatch {
            (vec![BOB], vec![U256::ONE, U256::ONE])
        } else {
            (vec![BOB], vec![U256::ONE])
        };

        let err = StorageCtx::enter(&mut storage, |ctx| {
            token.batch_mint(ctx, recipients, amounts, false)
        })
        .unwrap_err();

        assert_eq!(err, expected_error);
    }

    #[test]
    fn batch_mint_empty_batch_before_policy() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((MINT_ROLE, ALICE), true);
        token.accounting_mut().policy_ids.insert(
            B20PolicyType::MintReceiver.id(),
            PolicyRegistryStorage::ALWAYS_BLOCK_ID,
        );
        let mut storage = storage_with_caller(ALICE);

        let err =
            StorageCtx::enter(&mut storage, |ctx| token.batch_mint(ctx, vec![], vec![], false))
                .unwrap_err();

        assert_eq!(err, BasePrecompileError::revert(IB20Security::EmptyBatch {}));
    }

    #[test]
    fn batch_mint_invalid_receiver_before_policy() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((MINT_ROLE, ALICE), true);
        token.accounting_mut().policy_ids.insert(
            B20PolicyType::MintReceiver.id(),
            PolicyRegistryStorage::ALWAYS_BLOCK_ID,
        );
        let mut storage = storage_with_caller(ALICE);

        let err = StorageCtx::enter(&mut storage, |ctx| {
            token.batch_mint(ctx, vec![Address::ZERO], vec![U256::ONE], false)
        })
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO })
        );
    }

    #[test]
    fn batch_mint_policy_before_supply_cap() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((MINT_ROLE, ALICE), true);
        token.accounting_mut().supply_cap = U256::ZERO;
        token.accounting_mut().policy_ids.insert(
            B20PolicyType::MintReceiver.id(),
            PolicyRegistryStorage::ALWAYS_BLOCK_ID,
        );
        let mut storage = storage_with_caller(ALICE);

        let err = StorageCtx::enter(&mut storage, |ctx| {
            token.batch_mint(ctx, vec![BOB], vec![U256::ONE], false)
        })
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::MintReceiver.id(),
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            })
        );
    }

    #[rstest]
    #[case::pause_before_policy(
        true,
        true,
        false,
        BasePrecompileError::revert(IB20::ContractPaused { feature: IB20::PausableFeature::REDEEM })
    )]
    #[case::policy_before_minimum_check(
        false,
        true,
        false,
        BasePrecompileError::revert(IB20::PolicyForbids {
            policyScope: REDEEM_SENDER_POLICY,
            policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
        })
    )]
    #[case::minimum_check_before_balance(
        false,
        false,
        true,
        BasePrecompileError::revert(IB20Security::BelowMinimumRedeemable {
            shares: U256::from(1u64),
            minimum: U256::from(100u64),
        })
    )]
    fn security_redeem_revert_ordering(
        #[case] paused: bool,
        #[case] policy_blocks: bool,
        #[case] below_minimum: bool,
        #[case] expected_error: BasePrecompileError,
    ) {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = B20SecurityStorage::WAD;
        accounting.balances.insert(ALICE, U256::ZERO);
        accounting.total_supply = U256::ZERO;
        if paused {
            accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::REDEEM);
        }
        if policy_blocks {
            accounting
                .policy_ids
                .insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        } else {
            accounting
                .policy_ids
                .insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        }
        if below_minimum {
            accounting.minimum_redeemable = U256::from(100u64);
        }
        let mut token =
            TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.security_redeem(ALICE, U256::from(1u64)).unwrap_err(),
            expected_error
        );
    }
}
