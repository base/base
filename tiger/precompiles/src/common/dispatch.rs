//! ABI dispatch shared by the asset and stablecoin B-20 precompiles: the call envelope and the
//! common `IB20` surface. Each kind's own dispatcher decodes against its version's ABI and routes
//! its extension calls itself.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolValue};
use base_tiger_precompile_storage::{BasePrecompileError, PrecompileResult, Result, StorageCtx};

use crate::{
    B20Logic, B20PolicyType, B20Token, B20TokenRole, B20Version, CalldataGas,
    IB20::{self, IB20Calls as C},
    PermitArgs, PolicyAccounting, Token, TokenAccounting,
};

impl<S: TokenAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// The call envelope: rejects value, charges calldata gas, requires an active `version`
    /// (`None` is unreachable in practice, since the precompile is only installed from Beryl) and
    /// a deployed token, then runs `route`.
    pub(crate) fn dispatch_with(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: Option<B20Version>,
        route: impl FnOnce(&mut Self, B20Version) -> Result<Bytes>,
    ) -> PrecompileResult {
        if !ctx.call_value().is_zero() {
            return ctx.error_result(BasePrecompileError::revert(IB20::NonPayable {}));
        }
        if let Err(error) = CalldataGas::deduct(ctx, calldata) {
            return ctx.error_result(error);
        }
        let Some(version) = version else {
            return ctx.error_result(BasePrecompileError::Revert(Bytes::new()));
        };
        match self.accounting().is_initialized() {
            Ok(true) => {}
            Ok(false) => return ctx.error_result(BasePrecompileError::Revert(Bytes::new())),
            Err(error) => return ctx.error_result(error),
        }
        ctx.result_output(route(self, version), |b| b)
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
        let encoded: Bytes = match call {
            C::name(_) => self.accounting().name()?.abi_encode().into(),
            C::symbol(_) => self.accounting().symbol()?.abi_encode().into(),
            C::decimals(_) => U256::from(decimals(self.accounting())?).abi_encode().into(),
            C::totalSupply(_) => self.accounting().total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting().balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => self.accounting().allowance(c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => self.accounting().supply_cap()?.abi_encode().into(),
            C::nonces(c) => self.accounting().nonce(c.owner)?.abi_encode().into(),
            C::contractURI(_) => self.accounting().contract_uri()?.abi_encode().into(),

            C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::SEIZE_ROLE(_) => B20TokenRole::Seize.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),

            C::TRANSFER_SENDER_POLICY(_) => B20PolicyType::TransferSender.id().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => {
                B20PolicyType::TransferReceiver.id().abi_encode().into()
            }
            C::TRANSFER_EXECUTOR_POLICY(_) => {
                B20PolicyType::TransferExecutor.id().abi_encode().into()
            }
            C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode().into(),
            C::SEIZE_EXEMPT_POLICY(_) => B20PolicyType::SeizeExempt.id().abi_encode().into(),
            C::SEIZE_RECEIVER_POLICY(_) => B20PolicyType::SeizeReceiver.id().abi_encode().into(),

            C::hasRole(c) => self.accounting().has_role(c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => self.accounting().role_admin(c.role)?.abi_encode().into(),
            C::pausedFeatures(_) => logic.paused_features(self)?.abi_encode().into(),
            C::isPaused(c) => logic.is_paused(self, c.feature)?.abi_encode().into(),
            C::policyId(c) => logic.policy_id(self, c.policyScope)?.abi_encode().into(),
            C::DOMAIN_SEPARATOR(_) => {
                logic.domain_separator(self, ctx.chain_id())?.abi_encode().into()
            }
            C::eip712Domain(_) => {
                let (fields, name, version, chain_id, verifying_contract, salt, extensions) =
                    logic.eip712_domain(self, ctx.chain_id())?;
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

            C::transfer(c) => {
                logic.transfer(self, caller, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::transferFrom(c) => {
                logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged)?;
                true.abi_encode().into()
            }
            C::approve(c) => {
                logic.approve(self, caller, c.spender, c.amount)?;
                true.abi_encode().into()
            }
            C::transferWithMemo(c) => {
                logic.transfer(self, caller, c.to, c.amount, privileged)?;
                logic.emit_memo(self, caller, c.memo)?;
                true.abi_encode().into()
            }
            C::transferFromWithMemo(c) => {
                logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged)?;
                logic.emit_memo(self, caller, c.memo)?;
                true.abi_encode().into()
            }
            C::mint(c) => {
                logic.mint(self, caller, c.to, c.amount, privileged)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                logic.mint(self, caller, c.to, c.amount, privileged)?;
                logic.emit_memo(self, caller, c.memo)?;
                Bytes::new()
            }
            // Self-burns and renounces are never factory-privileged: during init the caller is
            // the factory, not a holder.
            C::burn(c) => {
                logic.burn(self, caller, c.amount)?;
                Bytes::new()
            }
            C::burnWithMemo(c) => {
                logic.burn(self, caller, c.amount)?;
                logic.emit_memo(self, caller, c.memo)?;
                Bytes::new()
            }
            C::burnBlocked(c) => {
                logic.burn_blocked(self, caller, c.from, c.amount, privileged)?;
                Bytes::new()
            }
            C::seizeWithMemo(c) => {
                logic.seize_with_memo(self, caller, c.from, c.to, c.amount, c.memo)?;
                Bytes::new()
            }
            C::pause(c) => {
                logic.pause(self, caller, c.features, privileged)?;
                Bytes::new()
            }
            C::unpause(c) => {
                logic.unpause(self, caller, c.features, privileged)?;
                Bytes::new()
            }
            C::updateSupplyCap(c) => {
                logic.update_supply_cap(self, caller, c.newSupplyCap, privileged)?;
                Bytes::new()
            }
            C::updateName(c) => {
                logic.update_name(self, caller, c.newName, privileged)?;
                Bytes::new()
            }
            C::updateSymbol(c) => {
                logic.update_symbol(self, caller, c.newSymbol, privileged)?;
                Bytes::new()
            }
            C::updateContractURI(c) => {
                logic.update_contract_uri(self, caller, c.newURI, privileged)?;
                Bytes::new()
            }
            C::grantRole(c) => {
                logic.grant_role(self, caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            C::revokeRole(c) => {
                logic.revoke_role(self, caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            C::renounceRole(c) => {
                logic.renounce_role(self, caller, c.role, c.callerConfirmation)?;
                Bytes::new()
            }
            C::renounceLastAdmin(_) => {
                logic.renounce_last_admin(self, caller)?;
                Bytes::new()
            }
            C::setRoleAdmin(c) => {
                logic.set_role_admin(self, caller, c.role, c.newAdminRole, privileged)?;
                Bytes::new()
            }
            C::updatePolicy(c) => {
                logic.update_policy(self, caller, c.policyScope, c.newPolicyId, privileged)?;
                Bytes::new()
            }
            C::permit(c) => {
                logic.permit(
                    self,
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
}
