//! ABI dispatch for the security B-20 variant.
//!
//! Security-specific selectors are tried first via `IB20Security::IB20SecurityCalls`.
//! This catches overridden selectors (`redeem`, `redeemWithMemo`) before the
//! inherited `IB20` fallthrough, ensuring security semantics always apply.
//! The `IB20` match block still includes those arms (Rust requires exhaustiveness)
//! and routes them to the same security implementation as a safety net.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    B20SecurityToken,
    abi::{IB20Security, IB20Security::IB20SecurityCalls as SC},
    accounting::SecurityAccounting,
    ids::{BURN_FROM_ROLE, REDEEM_SENDER_POLICY, SECURITY_OPERATOR_ROLE},
};
use crate::{
    ActivationFeature, ActivationRegistryStorage, B20Guards, B20PolicyType, B20TokenRole, Burnable,
    Configurable,
    IB20::{self, IB20Calls as C},
    Mintable, Pausable, PermitArgs, Permittable, Policy, RoleManaged, Token, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

/// WAD precision for share ratio arithmetic: 1e18.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Ensures `policy_scope` names either an inherited B-20 policy slot or the
    /// security redeem slot.
    fn is_supported_policy_scope(policy_scope: B256) -> bool {
        policy_scope == REDEEM_SENDER_POLICY || B20PolicyType::from_id(policy_scope).is_some()
    }

    fn ensure_supported_policy_type(policy_scope: B256) -> base_precompile_storage::Result<()> {
        if Self::is_supported_policy_scope(policy_scope) {
            Ok(())
        } else {
            Err(BasePrecompileError::revert(IB20::UnsupportedPolicyType {
                policyScope: policy_scope,
            }))
        }
    }

    fn ensure_security_operator(
        &self,
        caller: Address,
        privileged: bool,
    ) -> base_precompile_storage::Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, SECURITY_OPERATOR_ROLE) }
    }

    fn ensure_default_admin(
        &self,
        caller: Address,
        privileged: bool,
    ) -> base_precompile_storage::Result<()> {
        if privileged { Ok(()) } else { self.ensure_role(caller, Self::default_admin_role()) }
    }

    fn ensure_burn_from_role(&self, caller: Address) -> base_precompile_storage::Result<()> {
        self.ensure_role(caller, BURN_FROM_ROLE)
    }

    /// Returns the configured policy ID for `policy_scope`.
    fn policy_id_checked(&self, policy_scope: B256) -> base_precompile_storage::Result<u64> {
        Self::ensure_supported_policy_type(policy_scope)?;
        self.accounting.policy_id(policy_scope)
    }

    /// Updates the configured policy ID for `policy_scope`.
    fn update_policy(
        &mut self,
        caller: Address,
        policy_scope: B256,
        new_policy_id: u64,
        privileged: bool,
    ) -> base_precompile_storage::Result<()> {
        Self::ensure_supported_policy_type(policy_scope)?;
        if !privileged {
            self.ensure_role(caller, Self::default_admin_role())?;
        }
        let old_policy_id = self.accounting.policy_id(policy_scope)?;
        if !self.policy().policy_exists(new_policy_id)? {
            return Err(BasePrecompileError::revert(IB20::PolicyNotFound {
                policyId: new_policy_id,
            }));
        }
        self.accounting_mut().set_policy_id(policy_scope, new_policy_id)?;
        self.accounting_mut().emit_event(
            IB20::PolicyUpdated {
                policyScope: policy_scope,
                oldPolicyId: old_policy_id,
                newPolicyId: new_policy_id,
            }
            .encode_log_data(),
        )
    }
}

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Security` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);

        match self.accounting.is_initialized() {
            Ok(true) => {}
            Ok(false) => {
                return BasePrecompileError::Revert(Bytes::new())
                    .into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
            }
            Err(e) => return e.into_precompile_result(ctx.gas_used(), ctx.state_gas_used()),
        }
        self.inner(ctx, calldata).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            |b| b,
        )
    }

    /// Decodes calldata and executes the matching `IB20Security` or inherited `IB20` operation.
    pub fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_privilege(ctx, calldata, false)
    }

    /// Decodes calldata and executes it with optional factory-init privilege.
    pub fn inner_with_privilege(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        ActivationRegistryStorage::new(ctx)
            .ensure_activated(ActivationFeature::B20Security.id())?;

        // Security-specific and overridden selectors are caught here first.
        if let Ok(call) = IB20Security::IB20SecurityCalls::abi_decode(calldata) {
            return self.handle_security_call(ctx, call, privileged);
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
            C::nonces(c) => self.accounting.nonce(c.owner)?.abi_encode().into(),
            C::contractURI(_) => self.accounting.contract_uri()?.abi_encode().into(),

            // --- Role identifiers ---
            C::DEFAULT_ADMIN_ROLE(_) => Self::default_admin_role().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),

            // --- Policy type identifiers ---
            C::TRANSFER_SENDER_POLICY(_) => B20PolicyType::TransferSender.id().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => {
                B20PolicyType::TransferReceiver.id().abi_encode().into()
            }
            C::TRANSFER_EXECUTOR_POLICY(_) => {
                B20PolicyType::TransferExecutor.id().abi_encode().into()
            }
            C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode().into(),

            // --- Role reads ---
            C::hasRole(c) => self.accounting.has_role(c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => self.accounting.role_admin(c.role)?.abi_encode().into(),

            // --- Pause reads ---
            C::pausedFeatures(_) => self.paused_features()?.abi_encode().into(),
            C::isPaused(c) => self.is_paused(c.feature)?.abi_encode().into(),

            // --- Policy reads ---
            C::policyId(c) => self.policy_id_checked(c.policyScope)?.abi_encode().into(),

            // --- Domain reads ---
            C::DOMAIN_SEPARATOR(_) => self.domain_separator(ctx.chain_id())?.abi_encode().into(),
            C::eip712Domain(_) => {
                let (fields, name, version, chain_id, verifying_contract, salt, extensions) =
                    self.eip712_domain(ctx.chain_id())?;
                IB20::eip712DomainCall::abi_encode_returns(&IB20::eip712DomainReturn {
                    fields,
                    name,
                    version,
                    chainId: chain_id,
                    verifyingContract: verifying_contract,
                    salt,
                    extensions,
                })
                .into()
            }

            // --- ERC-20 mutating ---
            C::transfer(c) => {
                let caller = ctx.caller();
                self.transfer(caller, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                let caller = ctx.caller();
                self.transfer_from(caller, c.from, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                let caller = ctx.caller();
                self.approve(caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_with_memo(caller, c.to, c.amount, c.memo, privileged)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                let caller = ctx.caller();
                self.transfer_from_with_memo(caller, c.from, c.to, c.amount, c.memo, privileged)?;
                true.abi_encode().into()
            }

            // --- Mint ---
            C::mint(c) => {
                let caller = ctx.caller();
                self.mint(caller, c.to, c.amount, privileged)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                let caller = ctx.caller();
                self.mint_with_memo(caller, c.to, c.amount, c.memo, privileged)?;
                Bytes::new()
            }

            // --- Burn ---
            // Self-burn operations are never factory-privileged: during init the caller is the
            // factory, not a token holder.
            C::burn(c) => {
                let caller = ctx.caller();
                self.burn(caller, caller, c.amount, false)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                let caller = ctx.caller();
                self.burn_with_memo(caller, caller, c.amount, c.memo, false)?;
                Bytes::new()
            }
            C::burnBlocked(c) => {
                let caller = ctx.caller();
                self.burn_blocked(caller, c.from, c.amount, privileged)?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                let caller = ctx.caller();
                self.pause(caller, c.features, privileged)?;
                Bytes::new()
            }
            C::unpause(c) => {
                let caller = ctx.caller();
                self.unpause(caller, c.features, privileged)?;
                Bytes::new()
            }

            // --- Admin ---
            C::updateSupplyCap(c) => {
                let caller = ctx.caller();
                Configurable::update_supply_cap(self, caller, c.newSupplyCap, privileged)?;
                Bytes::new()
            }
            C::updateName(c) => {
                let caller = ctx.caller();
                Configurable::update_name(self, caller, c.newName, privileged)?;
                Bytes::new()
            }
            C::updateSymbol(c) => {
                let caller = ctx.caller();
                Configurable::update_symbol(self, caller, c.newSymbol, privileged)?;
                Bytes::new()
            }
            C::updateContractURI(c) => {
                let caller = ctx.caller();
                Configurable::update_contract_uri(self, caller, c.newURI, privileged)?;
                Bytes::new()
            }

            // --- Role mutations ---
            C::grantRole(c) => {
                let caller = ctx.caller();
                self.grant_role(caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            C::revokeRole(c) => {
                let caller = ctx.caller();
                self.revoke_role(caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            // Renounce operations are never factory-privileged: they are only meaningful for the
            // role holder making the call after token creation.
            C::renounceRole(c) => {
                let caller = ctx.caller();
                self.renounce_role(caller, c.role, c.callerConfirmation)?;
                Bytes::new()
            }
            C::renounceLastAdmin(_) => {
                let caller = ctx.caller();
                self.renounce_last_admin(caller)?;
                Bytes::new()
            }
            C::setRoleAdmin(c) => {
                let caller = ctx.caller();
                self.set_role_admin(caller, c.role, c.newAdminRole, privileged)?;
                Bytes::new()
            }

            // --- Policy mutations ---
            C::updatePolicy(c) => {
                let caller = ctx.caller();
                self.update_policy(caller, c.policyScope, c.newPolicyId, privileged)?;
                Bytes::new()
            }

            // --- Permit ---
            C::permit(c) => {
                self.permit(
                    ctx.chain_id(),
                    ctx.timestamp(),
                    PermitArgs {
                        owner: c.owner,
                        spender: c.spender,
                        value: c.value,
                        deadline: c.deadline,
                        v: c.v,
                        r: c.r,
                        s: c.s,
                    },
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
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            // --- Role / precision constants ---
            SC::SECURITY_OPERATOR_ROLE(_) => SECURITY_OPERATOR_ROLE.abi_encode().into(),
            SC::BURN_FROM_ROLE(_) => BURN_FROM_ROLE.abi_encode().into(),
            SC::WAD_PRECISION(_) => WAD.abi_encode().into(),
            SC::REDEEM_SENDER_POLICY(_) => REDEEM_SENDER_POLICY.abi_encode().into(),

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
                self.accounting.is_announcement_id_used(c.id.as_str())?.abi_encode().into()
            }

            // --- Security identifier reads ---
            SC::securityIdentifier(c) => {
                self.accounting.security_identifier(c.identifierType.as_str())?.abi_encode().into()
            }

            // --- Share ratio mutations ---
            SC::updateShareRatio(c) => {
                let caller = ctx.caller();
                self.ensure_security_operator(caller, privileged)?;
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
                self.announce(ctx, c.internalCalls, c.id, c.description, c.uri, privileged)?;
                Bytes::new()
            }

            // --- Batched mint / burn ---
            SC::batchMint(c) => {
                self.batch_mint(ctx, c.recipients, c.amounts, privileged)?;
                Bytes::new()
            }
            SC::batchBurn(c) => {
                self.batch_burn(ctx, c.accounts, c.amounts)?;
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
                self.security_redeem_with_memo(caller, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Minimum redeemable (security version, in shares) ---
            SC::minimumRedeemable(_) => self.accounting.minimum_redeemable()?.abi_encode().into(),
            SC::updateMinimumRedeemable(c) => {
                let caller = ctx.caller();
                self.ensure_default_admin(caller, privileged)?;
                self.accounting_mut().set_minimum_redeemable(c.newMinimumRedeemable)?;
                self.accounting_mut().emit_event(
                    IB20Security::MinimumRedeemableUpdated {
                        caller,
                        newMinimumRedeemable: c.newMinimumRedeemable,
                    }
                    .encode_log_data(),
                )?;
                Bytes::new()
            }

            // --- Security identifier mutations ---
            SC::updateSecurityIdentifier(c) => {
                let caller = ctx.caller();
                self.ensure_security_operator(caller, privileged)?;
                if c.identifierType.is_empty() {
                    return Err(BasePrecompileError::revert(
                        IB20Security::InvalidIdentifierType {},
                    ));
                }
                self.accounting_mut()
                    .set_security_identifier_value(c.identifierType.as_str(), c.value.clone())?;
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
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// [`Self::security_redeem`] with a memo emitted between `Transfer` and `Redeemed`.
    fn security_redeem_with_memo(
        &mut self,
        caller: Address,
        amount: U256,
        memo: B256,
    ) -> base_precompile_storage::Result<()> {
        let ratio = self.security_redeem_burn(caller, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())?;
        self.emit_redeemed(caller, amount, ratio)
    }

    /// Performs the shared security redeem burn and returns the ratio used for the floor check.
    fn security_redeem_burn(
        &mut self,
        caller: Address,
        amount: U256,
    ) -> base_precompile_storage::Result<U256> {
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::REDEEM)?;
        B20Guards::ensure_policy::<Self>(self, REDEEM_SENDER_POLICY, caller)?;
        if amount.is_zero() {
            return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
        }
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
        Ok(ratio)
    }

    fn emit_redeemed(
        &mut self,
        caller: Address,
        amount: U256,
        ratio: U256,
    ) -> base_precompile_storage::Result<()> {
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
    ) -> base_precompile_storage::Result<()> {
        let caller = ctx.caller();
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
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
        for (account, amount) in accounts.into_iter().zip(amounts) {
            if amount.is_zero() {
                return Err(BasePrecompileError::revert(IB20::InvalidAmount {}));
            }
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
        privileged: bool,
    ) -> base_precompile_storage::Result<()> {
        let caller = ctx.caller();
        self.ensure_security_operator(caller, privileged)?;
        if self.in_announcement {
            return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
        }

        if self.accounting.is_announcement_id_used(id.as_str())? {
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
            if call_bytes[..4] == IB20Security::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Security::AnnouncementInProgress {}));
            }
            self.inner_with_privilege(ctx, call_bytes, privileged).map_err(|_| {
                BasePrecompileError::revert(IB20Security::InternalCallFailed { call: call.clone() })
            })?;
        }

        self.accounting_mut().emit_event(IB20Security::EndAnnouncement { id }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_sol_types::{SolCall, SolEvent};
    use base_precompile_storage::{
        BasePrecompileError, HashMapStorageProvider, Result, StorageCtx, setup_storage,
    };

    use super::{BURN_FROM_ROLE, REDEEM_SENDER_POLICY};
    use crate::{
        ActivationFeature, ActivationRegistryStorage, B20PausableFeature, B20TokenRole, IB20,
        PolicyHandle, PolicyRegistryStorage, Token, TokenAccounting,
        b20_security::{B20SecurityStorage, B20SecurityToken, IB20Security, SecurityAccounting},
        common::test_utils::{InMemoryPolicy, InMemoryTokenAccounting},
    };

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const ACTIVATION_ADMIN: Address = Address::repeat_byte(0xcb);
    const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    fn make_token() -> TestSecurityToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.shares_to_tokens_ratio = WAD; // 1:1 ratio
        // Explicitly open redemption so non-policy tests are not blocked by the ALWAYS_BLOCK default.
        accounting.policy_ids.insert(REDEEM_SENDER_POLICY, PolicyRegistryStorage::ALWAYS_ALLOW_ID);
        TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn activate_b20_security(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationFeature::B20Security.id(), Some(ACTIVATION_ADMIN))
        })
        .unwrap();
    }

    fn storage_with_caller(caller: Address) -> HashMapStorageProvider {
        let mut storage = HashMapStorageProvider::new(1);
        activate_b20_security(&mut storage);
        storage.set_caller(caller);
        storage
    }

    fn call_security(
        token: &mut TestSecurityToken,
        caller: Address,
        calldata: Vec<u8>,
    ) -> Result<Bytes> {
        let mut storage = storage_with_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata.as_ref()))
    }

    fn batch_mint_calldata(recipients: Vec<Address>, amounts: Vec<U256>) -> Vec<u8> {
        IB20Security::batchMintCall { recipients, amounts }.abi_encode()
    }

    fn batch_burn_calldata(accounts: Vec<Address>, amounts: Vec<U256>) -> Vec<u8> {
        IB20Security::batchBurnCall { accounts, amounts }.abi_encode()
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
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        call_security(
            &mut token,
            ALICE,
            batch_mint_calldata(
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
            ),
        )
        .unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(200u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(
            token.accounting().events,
            alloc::vec![
                IB20::Transfer { from: Address::ZERO, to: ALICE, amount: U256::from(100u64) }
                    .encode_log_data(),
                IB20::Transfer { from: Address::ZERO, to: BOB, amount: U256::from(200u64) }
                    .encode_log_data()
            ]
        );
    }

    #[test]
    fn batch_mint_requires_mint_role() {
        let mut token = make_token();

        let err = call_security(
            &mut token,
            ALICE,
            batch_mint_calldata(alloc::vec![BOB], alloc::vec![U256::from(100u64)]),
        )
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
    }

    #[test]
    fn batch_burn_decrements_balances() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().balances.insert(ALICE, U256::from(500u64));
        token.accounting_mut().total_supply = U256::from(500u64);

        call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![ALICE], alloc::vec![U256::from(200u64)]),
        )
        .unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(300u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(300u64));
        assert_eq!(
            token.accounting().events,
            alloc::vec![
                IB20::Transfer { from: ALICE, to: Address::ZERO, amount: U256::from(200u64) }
                    .encode_log_data()
            ]
        );
    }

    #[test]
    fn batch_burn_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));

        assert_eq!(
            call_security(
                &mut token,
                ALICE,
                batch_burn_calldata(alloc::vec![ALICE], alloc::vec![U256::from(100u64)]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: ALICE,
                balance: U256::from(10u64),
                needed: U256::from(100u64),
            })
        );
    }

    #[test]
    fn batch_burn_requires_burn_from_role() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        let err = call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![BOB], alloc::vec![U256::from(10u64)]),
        )
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: BURN_FROM_ROLE,
            })
        );
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(50u64));
    }

    #[test]
    fn batch_burn_with_role_decrements_balances() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![BOB], alloc::vec![U256::from(10u64)]),
        )
        .unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(40u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(40u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn batch_burn_respects_burn_pause() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::BURN);
        token.accounting_mut().balances.insert(BOB, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        let err = call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![BOB], alloc::vec![U256::from(10u64)]),
        )
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::BURN,
            })
        );
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(50u64));
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
        token.accounting_mut().shares_to_tokens_ratio = U256::ONE;
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        // 1 token-wei * 1 / WAD rounds down to 0 shares, which is always rejected.
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
        accounting.shares_to_tokens_ratio = WAD;
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
    fn announce_marks_id_used() {
        let mut token = make_token();
        let id = "2026-Q1-split";

        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
        token.accounting_mut().mark_announcement_id_used(id).unwrap();
        assert!(token.accounting().is_announcement_id_used(id).unwrap());
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

    // --- batchMint: EmptyBatch / LengthMismatch ---

    #[test]
    fn batch_mint_rejects_empty() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_security(&mut token, ALICE, batch_mint_calldata(alloc::vec![], alloc::vec![]))
                .unwrap_err(),
            BasePrecompileError::revert(IB20Security::EmptyBatch {})
        );
    }

    #[test]
    fn batch_mint_rejects_length_mismatch() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_security(
                &mut token,
                ALICE,
                batch_mint_calldata(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );
    }

    // --- batchBurn: EmptyBatch / LengthMismatch / multi-account Transfer events ---

    #[test]
    fn batch_burn_rejects_empty() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);

        let err =
            call_security(&mut token, ALICE, batch_burn_calldata(alloc::vec![], alloc::vec![]))
                .unwrap_err();

        assert_eq!(err, BasePrecompileError::revert(IB20Security::EmptyBatch {}));
    }

    #[test]
    fn batch_burn_rejects_length_mismatch() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);

        let err = call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );

        let err = call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![], alloc::vec![U256::ONE]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ZERO,
                rightLen: U256::ONE,
            })
        );
    }

    #[test]
    fn batch_burn_validates_batch_shape_before_pause() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().paused = B20PausableFeature::mask(IB20::PausableFeature::BURN);

        let err = call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20Security::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );

        let err =
            call_security(&mut token, ALICE, batch_burn_calldata(alloc::vec![], alloc::vec![]))
                .unwrap_err();
        assert_eq!(err, BasePrecompileError::revert(IB20Security::EmptyBatch {}));
    }

    #[test]
    fn batch_burn_rejects_zero_amount() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            call_security(
                &mut token,
                ALICE,
                batch_burn_calldata(alloc::vec![ALICE], alloc::vec![U256::ZERO]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidAmount {})
        );
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 0);
    }

    #[test]
    fn batch_burn_multiple_accounts_emits_one_transfer_each() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((BURN_FROM_ROLE, ALICE), true);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().balances.insert(BOB, U256::from(200u64));
        token.accounting_mut().total_supply = U256::from(300u64);

        call_security(
            &mut token,
            ALICE,
            batch_burn_calldata(
                alloc::vec![ALICE, BOB],
                alloc::vec![U256::from(100u64), U256::from(200u64)],
            ),
        )
        .unwrap();

        // IB20Security: "Emits Transfer(accounts[i], address(0), amounts[i]) per element"
        assert_eq!(
            token.accounting().events,
            alloc::vec![
                IB20::Transfer { from: ALICE, to: Address::ZERO, amount: U256::from(100u64) }
                    .encode_log_data(),
                IB20::Transfer { from: BOB, to: Address::ZERO, amount: U256::from(200u64) }
                    .encode_log_data()
            ]
        );
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
    }

    // --- redeem: InsufficientBalance / boundary / ratio math / event pair ---

    #[test]
    fn security_redeem_rejects_insufficient_balance() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        token.accounting_mut().total_supply = U256::from(10u64);
        token.accounting_mut().minimum_redeemable = U256::from(1u64);
        // amount=100 > balance=10 → InsufficientBalance after the share-floor check passes
        assert!(token.security_redeem(ALICE, U256::from(100u64)).is_err());
        // no state mutation on failure
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(10u64));
    }

    #[test]
    fn security_redeem_at_exact_minimum_succeeds() {
        let mut token = make_token(); // 1:1 ratio
        token.accounting_mut().balances.insert(ALICE, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);
        // 5 tokens * WAD / WAD = 5 shares == minimum → boundary must be accepted
        token.accounting_mut().minimum_redeemable = U256::from(5u64);
        token.security_redeem(ALICE, U256::from(5u64)).unwrap();
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(45u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(45u64));
    }

    #[test]
    fn security_redeem_with_non_unit_ratio_applies_correct_share_math() {
        let mut token = make_token();
        // 2:1 ratio: 1 token = 2 shares
        token.accounting_mut().shares_to_tokens_ratio = WAD * U256::from(2u64);
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        // minimum = 10 shares → need at least 5 tokens
        token.accounting_mut().minimum_redeemable = U256::from(10u64);
        // 4 tokens → 8 shares < 10 → BelowMinimumRedeemable
        assert!(token.security_redeem(ALICE, U256::from(4u64)).is_err());
        // 5 tokens → 10 shares == minimum → accepted
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
        // "Emits Transfer(caller, address(0), amount) followed by Redeemed(caller, amount, ratio)"
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

    // --- toShares: zero balance / sub-WAD truncation / sharesOf delegation ---

    #[test]
    fn to_shares_zero_balance_yields_zero() {
        let token = make_token();
        assert_eq!(token.to_shares(U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn to_shares_sub_wad_ratio_truncates_to_zero() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // 0.5 WAD: 1 token → 0.5 shares → truncates to 0 via integer division
        accounting.shares_to_tokens_ratio = WAD / U256::from(2u64);
        let token = TestSecurityToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_shares(U256::from(1u64)).unwrap(), U256::ZERO);
    }

    #[test]
    fn shares_of_derives_from_balance() {
        let mut token = make_token(); // 1:1 ratio
        token.accounting_mut().balances.insert(ALICE, U256::from(75u64));
        // sharesOf(account) = toShares(balanceOf(account))
        let balance = token.accounting().balance_of(ALICE).unwrap();
        assert_eq!(token.to_shares(balance).unwrap(), U256::from(75u64));
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

    // --- updateShareRatio: persistence ---

    #[test]
    fn shares_to_tokens_ratio_update_persists() {
        let mut token = make_token();
        let new_ratio = WAD * U256::from(3u64);
        token.accounting_mut().set_shares_to_tokens_ratio(new_ratio).unwrap();
        assert_eq!(token.accounting().shares_to_tokens_ratio().unwrap(), new_ratio);
    }

    // --- securityIdentifier / updateSecurityIdentifier ---

    #[test]
    fn security_identifier_missing_key_returns_empty() {
        let token = make_token();
        // "Returns the empty string if not set"
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
        // "passing an empty value removes the entry"
        token.accounting_mut().set_security_identifier_value("FIGI", String::new()).unwrap();
        assert_eq!(token.accounting().security_identifier("FIGI").unwrap(), "");
    }

    // --- minimumRedeemable / updateMinimumRedeemable ---

    #[test]
    fn minimum_redeemable_persists() {
        let mut token = make_token();
        let floor = U256::from(42u64);
        token.accounting_mut().set_minimum_redeemable(floor).unwrap();
        assert_eq!(token.accounting().minimum_redeemable().unwrap(), floor);
    }

    // --- isAnnouncementIdUsed: fresh state ---

    #[test]
    fn announcement_id_not_used_initially() {
        let token = make_token();
        let id = "2026-Q1-split";
        // "Returns true if id has previously been consumed by announce" → false for new id
        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
    }
}
