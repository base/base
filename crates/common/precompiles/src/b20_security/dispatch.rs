//! ABI dispatch for the security B-20 variant.
//!
//! Security-specific selectors are tried first via `IB20Security::IB20SecurityCalls`.
//! This catches overridden selectors (`redeem`, `redeemWithMemo`) before the
//! inherited `IB20` fallthrough, ensuring security semantics always apply.
//! The `IB20` match block still includes those arms (Rust requires exhaustiveness)
//! and routes them to the same security implementation as a safety net.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolEvent, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    B20SecurityToken,
    abi::{IB20Security, IB20Security::IB20SecurityCalls as SC},
    accounting::SecurityAccounting,
};
use crate::{
    ActivationRegistryStorage, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    Mintable, Pausable, Permittable, Policy, Token, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

/// WAD precision for share ratio arithmetic: 1e18.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Security` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);

        match self.accounting.is_initialized() {
            Ok(true) => {}
            Ok(false) => {
                return BasePrecompileError::revert(IB20::Uninitialized {})
                    .into_precompile_result(ctx.gas_used());
            }
            Err(e) => return e.into_precompile_result(ctx.gas_used()),
        }
        self.inner(ctx, calldata).into_precompile_result(ctx.gas_used(), |b| b)
    }

    /// Decodes calldata and executes the matching `IB20Security` or inherited `IB20` operation.
    pub fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationRegistryStorage::B20_SECURITY)?;

        // Security-specific and overridden selectors are caught here first.
        if let Ok(call) = IB20Security::IB20SecurityCalls::abi_decode(calldata) {
            return self.handle_security_call(ctx, call);
        }

        // Fall through to inherited IB20 selectors.
        let call = decode_precompile_call!(calldata, IB20::IB20Calls);

        let encoded: Bytes = match call {
            // --- Pure reads ---
            C::name(_) => self.accounting.name()?.abi_encode().into(),
            C::symbol(_) => self.accounting.symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(self.accounting.decimals()?).abi_encode().into(),
            C::totalSupply(_) => self.accounting.total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting.balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => self.accounting.allowance(c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => self.accounting.supply_cap()?.abi_encode().into(),
            C::paused(_) => self.accounting.paused()?.abi_encode().into(),
            C::nonces(c) => self.accounting.nonce(c.owner)?.abi_encode().into(),
            C::minimumRedeemable(_) => self.accounting.minimum_redeemable()?.abi_encode().into(),
            C::contractURI(_) => self.accounting.contract_uri()?.abi_encode().into(),
            C::capabilities(_) => self.accounting.capabilities()?.abi_encode().into(),

            // --- Domain reads ---
            C::isPaused(c) => self.is_paused(c.vector)?.abi_encode().into(),
            C::isPausable(_) => self.is_pausable()?.abi_encode().into(),
            C::isCapMutable(_) => self.is_cap_mutable()?.abi_encode().into(),
            C::DOMAIN_SEPARATOR(_) => self.domain_separator(ctx.chain_id())?.abi_encode().into(),
            C::eip712Domain(_) => self.eip712_domain(ctx.chain_id())?.abi_encode().into(),

            // --- ERC-20 mutating ---
            C::transfer(c) => {
                let caller = ctx.caller();
                self.transfer(caller, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                let caller = ctx.caller();
                self.transfer_from(caller, c.from, c.to, c.amount)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                let caller = ctx.caller();
                self.approve(caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_with_memo(caller, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_from_with_memo(caller, c.from, c.to, c.amount, c.memo)?;
                true.abi_encode().into()
            }

            // --- Mint ---
            C::mint(c) => {
                self.mint(c.to, c.amount)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                self.mint_with_memo(c.to, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Burn ---
            C::burn(c) => {
                let caller = ctx.caller();
                self.burn(caller, c.amount)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                let caller = ctx.caller();
                self.burn_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }

            // Redeem / redeemWithMemo: normally caught by the security decoder above; these arms
            // exist only for Rust match exhaustiveness and apply the same security semantics.
            C::redeem(c) => {
                let caller = ctx.caller();
                self.security_redeem(caller, c.amount)?;
                Bytes::new()
            }
            C::redeemWithMemo(c) => {
                let caller = ctx.caller();
                self.security_redeem(caller, c.amount)?;
                self.accounting_mut().emit_event(IB20::Memo { memo: c.memo }.encode_log_data())?;
                Bytes::new()
            }
            C::setMinimumRedeemable(c) => {
                self.accounting_mut().set_minimum_redeemable(c.newMinimum)?;
                self.accounting_mut().emit_event(
                    IB20Security::MinimumRedeemableUpdated { newMinimumRedeemable: c.newMinimum }
                        .encode_log_data(),
                )?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                let caller = ctx.caller();
                self.pause(caller, c.vectors)?;
                Bytes::new()
            }
            C::unpause(_) => {
                let caller = ctx.caller();
                self.unpause(caller)?;
                Bytes::new()
            }

            // --- Admin ---
            C::setSupplyCap(c) => {
                let caller = ctx.caller();
                Configurable::set_supply_cap(self, caller, c.newSupplyCap)?;
                Bytes::new()
            }
            C::setName(c) => {
                let caller = ctx.caller();
                Configurable::set_name(self, caller, c.newName)?;
                Bytes::new()
            }
            C::setSymbol(c) => {
                let caller = ctx.caller();
                Configurable::set_symbol(self, caller, c.newSymbol)?;
                Bytes::new()
            }
            C::setContractURI(c) => {
                let caller = ctx.caller();
                Configurable::set_contract_uri(self, caller, c.newURI)?;
                Bytes::new()
            }

            // --- Permit ---
            C::permit(c) => {
                self.permit(
                    ctx.chain_id(),
                    ctx.timestamp(),
                    c.owner,
                    c.spender,
                    c.value,
                    c.deadline,
                    c.v,
                    c.r,
                    c.s,
                )?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    fn handle_security_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: SC,
    ) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            // --- Role / precision constants ---
            SC::SECURITY_OPERATOR_ROLE(_) => {
                keccak256(b"SECURITY_OPERATOR_ROLE").abi_encode().into()
            }
            SC::BURN_FROM_ROLE(_) => keccak256(b"BURN_FROM_ROLE").abi_encode().into(),
            SC::WAD_PRECISION(_) => WAD.abi_encode().into(),
            SC::REDEEMER_SENDER_POLICY(_) => {
                keccak256(b"REDEEMER_SENDER_POLICY").abi_encode().into()
            }

            // --- Share ratio reads ---
            SC::sharesToTokensRatio(_) => {
                self.accounting.shares_to_tokens_ratio()?.abi_encode().into()
            }
            SC::toShares(c) => self.to_shares(c.balance)?.abi_encode().into(),
            SC::sharesOf(c) => {
                let balance = self.accounting.balance_of(c.account)?;
                self.to_shares(balance)?.abi_encode().into()
            }

            // --- Announcement reads ---
            SC::isAnnouncementIdUsed(c) => {
                let id_hash = keccak256(c.id.as_bytes());
                self.accounting.is_announcement_id_used(id_hash)?.abi_encode().into()
            }

            // --- Security identifier reads ---
            SC::securityIdentifier(c) => {
                let key = keccak256(c.identifierType.as_bytes());
                self.accounting.security_identifier(key)?.abi_encode().into()
            }

            // --- Share ratio mutations ---
            SC::updateShareRatio(c) => {
                self.accounting_mut().set_shares_to_tokens_ratio(c.newSharesToTokensRatio)?;
                self.accounting_mut().emit_event(
                    IB20Security::ShareRatioUpdated {
                        sharesToTokensRatio: c.newSharesToTokensRatio,
                    }
                    .encode_log_data(),
                )?;
                Bytes::new()
            }

            // --- Announcement ---
            SC::announce(c) => {
                self.announce(ctx, c.internalCalls, c.id, c.description, c.uri)?;
                Bytes::new()
            }

            // --- Batched mint / burn ---
            SC::batchMint(c) => {
                self.batch_mint(c.recipients, c.amounts)?;
                Bytes::new()
            }
            SC::batchBurn(c) => {
                self.batch_burn(c.accounts, c.amounts)?;
                Bytes::new()
            }

            // --- Security redeem (overrides IB20 redeem semantics) ---
            SC::redeem(c) => {
                let caller = ctx.caller();
                self.security_redeem(caller, c.amount)?;
                Bytes::new()
            }
            SC::redeemWithMemo(c) => {
                let caller = ctx.caller();
                self.security_redeem(caller, c.amount)?;
                self.accounting_mut().emit_event(IB20::Memo { memo: c.memo }.encode_log_data())?;
                Bytes::new()
            }

            // --- Minimum redeemable (security version, in shares) ---
            SC::updateMinimumRedeemable(c) => {
                self.accounting_mut().set_minimum_redeemable(c.newMinimumRedeemable)?;
                self.accounting_mut().emit_event(
                    IB20Security::MinimumRedeemableUpdated {
                        newMinimumRedeemable: c.newMinimumRedeemable,
                    }
                    .encode_log_data(),
                )?;
                Bytes::new()
            }

            // --- Security identifier mutations ---
            SC::updateSecurityIdentifier(c) => {
                if c.identifierType.is_empty() {
                    return Err(BasePrecompileError::revert(
                        IB20Security::InvalidIdentifierType {},
                    ));
                }
                let key = keccak256(c.identifierType.as_bytes());
                self.accounting_mut().set_security_identifier(key, c.value.clone())?;
                self.accounting_mut().emit_event(
                    IB20Security::SecurityIdentifierUpdated {
                        identifierType: c.identifierType,
                        value: c.value,
                    }
                    .encode_log_data(),
                )?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    /// Converts a token balance to shares: `balance * sharesToTokensRatio / WAD`.
    fn to_shares(&self, balance: U256) -> base_precompile_storage::Result<U256> {
        let ratio = self.accounting.shares_to_tokens_ratio()?;
        Ok(balance.saturating_mul(ratio) / WAD)
    }

    /// Performs a security-specific redeem: share-based floor check, burn, security `Redeemed` event.
    fn security_redeem(
        &mut self,
        caller: Address,
        amount: U256,
    ) -> base_precompile_storage::Result<()> {
        let ratio = self.accounting.shares_to_tokens_ratio()?;
        let shares = amount.saturating_mul(ratio) / WAD;
        let minimum = self.accounting.minimum_redeemable()?;
        if shares == U256::ZERO || shares < minimum {
            return Err(BasePrecompileError::revert(IB20Security::BelowMinimumRedeemable {
                shares,
                minimum,
            }));
        }
        let balance = self.accounting.balance_of(caller)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: caller,
                balance,
                needed: amount,
            }));
        }
        self.accounting_mut().set_balance(caller, balance - amount)?;
        let supply = self.accounting.total_supply()?;
        self.accounting_mut().set_total_supply(supply.saturating_sub(amount))?;
        self.accounting_mut().emit_event(
            IB20::Transfer { from: caller, to: Address::ZERO, amount }.encode_log_data(),
        )?;
        self.accounting_mut().emit_event(
            IB20Security::Redeemed { from: caller, amt: amount, sharesToTokensRatio: ratio }
                .encode_log_data(),
        )
    }

    /// Mints tokens to multiple recipients. All-or-nothing.
    fn batch_mint(
        &mut self,
        recipients: Vec<Address>,
        amounts: Vec<U256>,
    ) -> base_precompile_storage::Result<()> {
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
            self.mint(recipient, amount)?;
        }
        Ok(())
    }

    /// Burns tokens from multiple accounts unconditionally. All-or-nothing.
    ///
    /// Unlike `burnBlocked`, this path has no policy precondition — the
    /// `BURN_FROM_ROLE` authorization is the sole gate (role checks are a TODO
    /// matching the rest of the codebase).
    fn batch_burn(
        &mut self,
        accounts: Vec<Address>,
        amounts: Vec<U256>,
    ) -> base_precompile_storage::Result<()> {
        if accounts.is_empty() {
            return Err(BasePrecompileError::revert(IB20Security::EmptyBatch {}));
        }
        if accounts.len() != amounts.len() {
            return Err(BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::from(accounts.len()),
                rightLen: U256::from(amounts.len()),
            }));
        }
        for (account, amount) in accounts.into_iter().zip(amounts) {
            let balance = self.accounting.balance_of(account)?;
            if balance < amount {
                return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                    sender: account,
                    balance,
                    needed: amount,
                }));
            }
            self.accounting_mut().set_balance(account, balance - amount)?;
            let supply = self.accounting.total_supply()?;
            self.accounting_mut().set_total_supply(supply.saturating_sub(amount))?;
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
    ) -> base_precompile_storage::Result<()> {
        if self.in_announcement {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
        }

        let id_hash: B256 = keccak256(id.as_bytes());
        if self.accounting.is_announcement_id_used(id_hash)? {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementIdAlreadyUsed {
                id: id.clone(),
            }));
        }
        self.accounting_mut().mark_announcement_id_used(id_hash)?;

        let caller = ctx.caller();
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
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, keccak256};

    use crate::{
        Token, TokenAccounting,
        b20_security::{B20SecurityToken, SecurityAccounting},
        common::test_utils::{InMemoryPolicy, InMemoryTokenAccounting},
    };

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = WAD; // 1:1 ratio
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
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
    fn batch_mint_increases_balances() {
        let mut token = make_token();
        token
            .batch_mint(
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
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
        assert!(token.batch_mint(alloc::vec![], alloc::vec![]).is_err());
    }

    #[test]
    fn batch_mint_rejects_length_mismatch() {
        let mut token = make_token();
        assert!(token.batch_mint(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]).is_err());
    }

    #[test]
    fn batch_burn_decrements_balances() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(500u64));
        token.accounting_mut().total_supply = U256::from(500u64);

        token.batch_burn(alloc::vec![ALICE], alloc::vec![U256::from(200u64)]).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn batch_burn_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        assert!(token.batch_burn(alloc::vec![ALICE], alloc::vec![U256::from(100u64)]).is_err());
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
        assert_eq!(token.accounting().events.len(), 2); // Transfer + Redeemed
    }

    #[test]
    fn security_redeem_rejects_below_minimum_shares() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(10u64);

        // 5 tokens * 1e18 ratio / 1e18 = 5 shares < 10 minimum
        assert!(token.security_redeem(ALICE, U256::from(5u64)).is_err());
    }

    #[test]
    fn security_redeem_rejects_zero_shares() {
        let mut token = make_token();
        token.accounting_mut().shares_to_tokens_ratio = U256::ZERO;
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        // 0 ratio → 0 shares → always rejected
        assert!(token.security_redeem(ALICE, U256::from(50u64)).is_err());
    }

    #[test]
    fn announce_marks_id_used() {
        let mut token = make_token();
        let id_hash = keccak256(b"2026-Q1-split");

        assert!(!token.accounting().is_announcement_id_used(id_hash).unwrap());
        token.accounting_mut().mark_announcement_id_used(id_hash).unwrap();
        assert!(token.accounting().is_announcement_id_used(id_hash).unwrap());
    }

    #[test]
    fn security_identifier_roundtrip() {
        let mut token = make_token();
        let key = keccak256(b"ISIN");

        assert_eq!(token.accounting().security_identifier(key).unwrap(), "");
        token.accounting_mut().set_security_identifier(key, "US0000000000".to_string()).unwrap();
        assert_eq!(
            token.accounting().security_identifier(key).unwrap(),
            "US0000000000".to_string()
        );
    }
}
