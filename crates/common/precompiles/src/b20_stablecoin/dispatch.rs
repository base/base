//! ABI dispatch for the stablecoin B-20 variant.
//!
//! Dispatches the full `IB20` selector set using B-20 stablecoin activation.
//! All logic mirrors `B20Token::inner_with_privilege` exactly; the only
//! distinction is the activation guard and the `StablecoinAccounting` bound
//! that provides `currency()` from the stablecoin extension namespace.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    B20StablecoinToken,
    abi::{IB20Stablecoin, IB20Stablecoin::IB20StablecoinCalls as SC},
    accounting::StablecoinAccounting,
};
use crate::{
    ActivationFeature, ActivationRegistryStorage, B20TokenRole, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    Mintable, Pausable, Permittable, Policy, RoleManaged, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost},
};

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        // Ensure the token has been deployed (has bytecode at its address).
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

    /// Decodes calldata and executes the matching `IB20` operation.
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
            .ensure_activated(ActivationFeature::B20Stablecoin.id())?;

        if let Ok(call) = IB20Stablecoin::IB20StablecoinCalls::abi_decode(calldata) {
            return self.handle_stablecoin_call(call);
        }

        let call = decode_precompile_call!(calldata, IB20::IB20Calls);

        let encoded: Bytes = match call {
            // --- Pure reads: direct to accounting ---
            C::name(_) => self.accounting.name()?.abi_encode().into(),
            C::symbol(_) => self.accounting.symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(self.accounting.decimals()?).abi_encode().into(),
            C::totalSupply(_) => self.accounting.total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting.balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => self.accounting.allowance(c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => self.accounting.supply_cap()?.abi_encode().into(),
            C::nonces(c) => self.accounting.nonce(c.owner)?.abi_encode().into(),
            C::contractURI(_) => self.accounting.contract_uri()?.abi_encode().into(),
            C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),
            C::TRANSFER_SENDER_POLICY(_) => Self::transfer_sender_policy().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => Self::transfer_receiver_policy().abi_encode().into(),
            C::TRANSFER_EXECUTOR_POLICY(_) => Self::transfer_executor_policy().abi_encode().into(),
            C::MINT_RECEIVER_POLICY(_) => Self::mint_receiver_policy().abi_encode().into(),
            C::hasRole(c) => self.has_role(c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => self.role_admin(c.role)?.abi_encode().into(),
            C::pausedFeatures(_) => self.paused_features()?.abi_encode().into(),
            C::policyId(c) => self.policy_id(c.policyScope)?.abi_encode().into(),

            // --- Domain reads (light logic) ---
            C::isPaused(c) => self.is_paused(c.feature)?.abi_encode().into(),
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
            C::burn(c) => {
                let caller = ctx.caller();
                // Self-burn operations are never factory-privileged: during init the caller is the
                // factory, not a token holder.
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

    fn handle_stablecoin_call(&self, call: SC) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            SC::currency(_) => self.accounting.currency()?.abi_encode().into(),
        };
        Ok(encoded)
    }
}
