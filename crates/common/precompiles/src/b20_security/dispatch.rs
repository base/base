//! ABI dispatch for the security B-20 variant.
//!
//! Security-specific selectors are tried first via `IB20Security::IB20SecurityCalls`.
//! This catches overridden selectors (`redeem`, `redeemWithMemo`) before the
//! inherited `IB20` fallthrough, ensuring security semantics always apply.
//! The `IB20` match block still includes those arms (Rust requires exhaustiveness)
//! and routes them to the same security implementation as a safety net.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    B20SecurityToken,
    abi::{IB20Security, IB20Security::IB20SecurityCalls as SC},
    accounting::SecurityAccounting,
    ids::{BURN_FROM_ROLE, REDEEM_SENDER_POLICY, SECURITY_OPERATOR_ROLE},
    token::WAD,
};
use crate::{
    ActivationFeature, ActivationRegistryStorage, B20PolicyType, B20TokenRole, Burnable,
    Configurable,
    IB20::{self, IB20Calls as C},
    Mintable, Pausable, Permittable, Policy, RoleManaged, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Security` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);

        match self.accounting.is_initialized() {
            Ok(true) => {}
            Ok(false) => {
                return BasePrecompileError::Revert(Bytes::new())
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
            C::policyId(c) => self.policy_id(c.policyType)?.abi_encode().into(),

            // --- Domain reads ---
            C::DOMAIN_SEPARATOR(_) => self.domain_separator(ctx.chain_id())?.abi_encode().into(),
            C::eip712Domain(_) => self.eip712_domain(ctx.chain_id())?.abi_encode().into(),

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
                self.update_policy(caller, c.policyType, c.newPolicyId, privileged)?;
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
                self.update_share_ratio(ctx.caller(), c.newSharesToTokensRatio, privileged)?;
                Bytes::new()
            }

            // --- Announcement ---
            SC::announce(c) => {
                self.announce(ctx, c.internalCalls, c.id, c.description, c.uri, privileged)?;
                Bytes::new()
            }

            // --- Batched mint / burn ---
            SC::batchMint(c) => {
                self.batch_mint(ctx.caller(), c.recipients, c.amounts, privileged)?;
                Bytes::new()
            }
            SC::batchBurn(c) => {
                self.batch_burn(ctx.caller(), c.accounts, c.amounts, privileged)?;
                Bytes::new()
            }

            // --- Security redeem (overrides IB20 redeem semantics) ---
            SC::redeem(c) => {
                self.security_redeem(ctx.caller(), c.amount)?;
                Bytes::new()
            }
            SC::redeemWithMemo(c) => {
                self.security_redeem_with_memo(ctx.caller(), c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Minimum redeemable (security version, in shares) ---
            SC::minimumRedeemable(_) => self.accounting.minimum_redeemable()?.abi_encode().into(),
            SC::updateMinimumRedeemable(c) => {
                self.update_minimum_redeemable(ctx.caller(), c.newMinimumRedeemable, privileged)?;
                Bytes::new()
            }

            // --- Security identifier mutations ---
            SC::updateSecurityIdentifier(c) => {
                self.update_security_identifier(
                    ctx.caller(),
                    c.identifierType,
                    c.value,
                    privileged,
                )?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }
}
