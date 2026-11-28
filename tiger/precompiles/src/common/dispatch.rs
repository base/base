//! ABI dispatch shared by the asset and stablecoin B-20 precompiles: the call envelope and the
//! common `IB20` surface. Each kind's own dispatcher decodes against its version's ABI and routes
//! its extension calls itself.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolValue;

use crate::{
    B20Logic, B20PolicyType, B20Token, B20TokenRole, B20Version, BasePrecompileError, Calldata,
    IB20::{self, IB20Calls as C}, PermitArgs, PolicyAccounting, PrecompileResult, Result,
    StorageCtx, TokenAccounting,
};

impl<S: TokenAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// The call envelope of both token kinds: [`Calldata::dispatch`] at `version` (`None` is
    /// unreachable in practice, since the tokens are only installed from Beryl), then a revert
    /// unless the token is deployed, then `route`.
    pub fn dispatch_with(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: Option<B20Version>,
        route: impl FnOnce(&mut Self, B20Version) -> Result<Bytes>,
    ) -> PrecompileResult {
        Calldata::dispatch(ctx, calldata, Some(IB20::NonPayable {}), version, |version| {
            if !self.accounting.is_initialized()? {
                return Err(BasePrecompileError::Revert(Bytes::new()));
            }
            route(self, version)
        })
    }

    /// Handles a call on the common `IB20` surface. `decimals` answers `decimals()`, which the
    /// kinds store differently.
    pub(crate) fn handle_b20_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: C,
        logic: B20Logic,
        privileged: bool,
        decimals: impl FnOnce(&S) -> Result<u8>,
    ) -> Result<Bytes> {
        let caller = ctx.caller();
        // Encoders for the mutations, which return `true` (ERC-20 transfers and approvals) or
        // nothing.
        let ok = |result: Result<()>| result.map(|()| true.abi_encode());
        let empty = |result: Result<()>| result.map(|()| Vec::new());
        let encoded: Vec<u8> = match call {
            C::name(_) => self.accounting.name()?.abi_encode(),
            C::symbol(_) => self.accounting.symbol()?.abi_encode(),
            C::decimals(_) => U256::from(decimals(&self.accounting)?).abi_encode(),
            C::totalSupply(_) => self.accounting.total_supply()?.abi_encode(),
            C::balanceOf(c) => self.accounting.balance_of(c.account)?.abi_encode(),
            C::allowance(c) => self.accounting.allowance(c.owner, c.spender)?.abi_encode(),
            C::supplyCap(_) => self.accounting.supply_cap()?.abi_encode(),
            C::nonces(c) => self.accounting.nonce(c.owner)?.abi_encode(),
            C::contractURI(_) => self.accounting.contract_uri()?.abi_encode(),

            C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode(),
            C::SEIZE_ROLE(_) => B20TokenRole::Seize.id().abi_encode(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode(),

            C::TRANSFER_SENDER_POLICY(_) => B20PolicyType::TransferSender.id().abi_encode(),
            C::TRANSFER_RECEIVER_POLICY(_) => B20PolicyType::TransferReceiver.id().abi_encode(),
            C::TRANSFER_EXECUTOR_POLICY(_) => B20PolicyType::TransferExecutor.id().abi_encode(),
            C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode(),
            C::SEIZE_EXEMPT_POLICY(_) => B20PolicyType::SeizeExempt.id().abi_encode(),
            C::SEIZE_RECEIVER_POLICY(_) => B20PolicyType::SeizeReceiver.id().abi_encode(),

            C::hasRole(c) => self.accounting.has_role(c.role, c.account)?.abi_encode(),
            C::getRoleAdmin(c) => self.accounting.role_admin(c.role)?.abi_encode(),
            C::pausedFeatures(_) => logic.paused_features(self)?.abi_encode(),
            C::isPaused(c) => logic.is_paused(self, c.feature)?.abi_encode(),
            C::policyId(c) => logic.policy_id(self, c.policyScope)?.abi_encode(),
            C::DOMAIN_SEPARATOR(_) => logic.domain_separator(self, ctx.chain_id())?.abi_encode(),
            // The return tuple, encoded as the call's return parameters.
            C::eip712Domain(_) => logic.eip712_domain(self, ctx.chain_id())?.abi_encode_params(),

            C::transfer(c) => ok(logic.transfer(self, caller, c.to, c.amount, privileged))?,
            C::transferFrom(c) => {
                ok(logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged))?
            }
            C::approve(c) => ok(logic.approve(self, caller, c.spender, c.amount))?,
            C::transferWithMemo(c) => {
                logic.transfer(self, caller, c.to, c.amount, privileged)?;
                ok(logic.emit_memo(self, caller, c.memo))?
            }
            C::transferFromWithMemo(c) => {
                logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged)?;
                ok(logic.emit_memo(self, caller, c.memo))?
            }
            C::mint(c) => empty(logic.mint(self, caller, c.to, c.amount, privileged))?,
            C::mintWithMemo(c) => {
                logic.mint(self, caller, c.to, c.amount, privileged)?;
                empty(logic.emit_memo(self, caller, c.memo))?
            }
            // Self-burns and renounces are never factory-privileged: during init the caller is
            // the factory, not a holder.
            C::burn(c) => empty(logic.burn(self, caller, c.amount))?,
            C::burnWithMemo(c) => {
                logic.burn(self, caller, c.amount)?;
                empty(logic.emit_memo(self, caller, c.memo))?
            }
            C::burnBlocked(c) => {
                empty(logic.burn_blocked(self, caller, c.from, c.amount, privileged))?
            }
            C::seizeWithMemo(c) => {
                empty(logic.seize_with_memo(self, caller, c.from, c.to, c.amount, c.memo))?
            }
            C::pause(c) => empty(logic.pause(self, caller, c.features, privileged))?,
            C::unpause(c) => empty(logic.unpause(self, caller, c.features, privileged))?,
            C::updateSupplyCap(c) => {
                empty(logic.update_supply_cap(self, caller, c.newSupplyCap, privileged))?
            }
            C::updateName(c) => empty(logic.update_name(self, caller, c.newName, privileged))?,
            C::updateSymbol(c) => {
                empty(logic.update_symbol(self, caller, c.newSymbol, privileged))?
            }
            C::updateContractURI(c) => {
                empty(logic.update_contract_uri(self, caller, c.newURI, privileged))?
            }
            C::grantRole(c) => {
                empty(logic.grant_role(self, caller, c.role, c.account, privileged))?
            }
            C::revokeRole(c) => {
                empty(logic.revoke_role(self, caller, c.role, c.account, privileged))?
            }
            C::renounceRole(c) => {
                empty(logic.renounce_role(self, caller, c.role, c.callerConfirmation))?
            }
            C::renounceLastAdmin(_) => empty(logic.renounce_last_admin(self, caller))?,
            C::setRoleAdmin(c) => {
                empty(logic.set_role_admin(self, caller, c.role, c.newAdminRole, privileged))?
            }
            C::updatePolicy(c) => {
                let (scope, id) = (c.policyScope, c.newPolicyId);
                empty(logic.update_policy(self, caller, scope, id, privileged))?
            }
            C::permit(c) => {
                let (owner, spender, value, deadline) = (c.owner, c.spender, c.value, c.deadline);
                let args = PermitArgs { owner, spender, value, deadline, v: c.v, r: c.r, s: c.s };
                empty(logic.permit(self, ctx.chain_id(), ctx.timestamp(), args))?
            }
        };
        Ok(encoded.into())
    }
}
