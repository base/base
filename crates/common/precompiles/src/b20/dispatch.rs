use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolValue};
use base_precompile_storage::{BasePrecompileError, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use super::{
    B20Token, B20TokenPrecompile,
    abi::{IB20, IB20::IB20Calls as C},
};
use crate::{
    ActivationFeature, ActivationRegistryStorage, B20TokenRole, Burnable, CalldataCycleTracker,
    Configurable, Mintable, Pausable, PermitArgs, Permittable, Policy, RoleManaged,
    TokenAccounting, Transferable,
    macros::{decode_precompile_call, deduct_calldata_cost, track_precompile_cycles},
};

impl<S: TokenAccounting, P: Policy> B20Token<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        deduct_calldata_cost!(ctx, calldata);
        // Ensure the token has been deployed (has bytecode at its address).
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
        ActivationRegistryStorage::new(ctx).ensure_activated(ActivationFeature::B20Token.id())?;

        let call = decode_precompile_call!(calldata, IB20::IB20Calls);

        track_precompile_cycles!(B20TokenPrecompile, calldata, {
            let encoded: Bytes = match call {
                // --- Pure reads: direct to accounting ---
                C::name(_) => self.accounting.name()?.abi_encode().into(),
                C::symbol(_) => self.accounting.symbol()?.abi_encode().into(),
                C::decimals(_) => U256::from(self.accounting.decimals()?).abi_encode().into(),
                C::totalSupply(_) => self.accounting.total_supply()?.abi_encode().into(),
                C::balanceOf(c) => self.accounting.balance_of(c.account)?.abi_encode().into(),
                C::allowance(c) => {
                    self.accounting.allowance(c.owner, c.spender)?.abi_encode().into()
                }
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
                C::TRANSFER_RECEIVER_POLICY(_) => {
                    Self::transfer_receiver_policy().abi_encode().into()
                }
                C::TRANSFER_EXECUTOR_POLICY(_) => {
                    Self::transfer_executor_policy().abi_encode().into()
                }
                C::MINT_RECEIVER_POLICY(_) => Self::mint_receiver_policy().abi_encode().into(),
                C::hasRole(c) => self.has_role(c.role, c.account)?.abi_encode().into(),
                C::getRoleAdmin(c) => self.role_admin(c.role)?.abi_encode().into(),
                C::pausedFeatures(_) => self.paused_features()?.abi_encode().into(),
                C::policyId(c) => self.policy_id(c.policyScope)?.abi_encode().into(),

                // --- Domain reads (light logic) ---
                C::isPaused(c) => self.is_paused(c.feature)?.abi_encode().into(),
                C::DOMAIN_SEPARATOR(_) => {
                    self.domain_separator(ctx.chain_id())?.abi_encode().into()
                }
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
                    self.transfer_from_with_memo(
                        caller, c.from, c.to, c.amount, c.memo, privileged,
                    )?;
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
        })
    }
}

impl CalldataCycleTracker for B20TokenPrecompile {
    fn key_for_calldata(calldata: &[u8]) -> Option<&'static str> {
        let selector = calldata.get(..4)?.try_into().ok()?;

        match selector {
            IB20::nameCall::SELECTOR => Some("precompile-b20-name"),
            IB20::symbolCall::SELECTOR => Some("precompile-b20-symbol"),
            IB20::decimalsCall::SELECTOR => Some("precompile-b20-decimals"),
            IB20::totalSupplyCall::SELECTOR => Some("precompile-b20-totalSupply"),
            IB20::balanceOfCall::SELECTOR => Some("precompile-b20-balanceOf"),
            IB20::allowanceCall::SELECTOR => Some("precompile-b20-allowance"),
            IB20::supplyCapCall::SELECTOR => Some("precompile-b20-supplyCap"),
            IB20::noncesCall::SELECTOR => Some("precompile-b20-nonces"),
            IB20::contractURICall::SELECTOR => Some("precompile-b20-contractURI"),
            IB20::DEFAULT_ADMIN_ROLECall::SELECTOR => Some("precompile-b20-DEFAULT_ADMIN_ROLE"),
            IB20::MINT_ROLECall::SELECTOR => Some("precompile-b20-MINT_ROLE"),
            IB20::BURN_ROLECall::SELECTOR => Some("precompile-b20-BURN_ROLE"),
            IB20::BURN_BLOCKED_ROLECall::SELECTOR => Some("precompile-b20-BURN_BLOCKED_ROLE"),
            IB20::PAUSE_ROLECall::SELECTOR => Some("precompile-b20-PAUSE_ROLE"),
            IB20::UNPAUSE_ROLECall::SELECTOR => Some("precompile-b20-UNPAUSE_ROLE"),
            IB20::METADATA_ROLECall::SELECTOR => Some("precompile-b20-METADATA_ROLE"),
            IB20::TRANSFER_SENDER_POLICYCall::SELECTOR => {
                Some("precompile-b20-TRANSFER_SENDER_POLICY")
            }
            IB20::TRANSFER_RECEIVER_POLICYCall::SELECTOR => {
                Some("precompile-b20-TRANSFER_RECEIVER_POLICY")
            }
            IB20::TRANSFER_EXECUTOR_POLICYCall::SELECTOR => {
                Some("precompile-b20-TRANSFER_EXECUTOR_POLICY")
            }
            IB20::MINT_RECEIVER_POLICYCall::SELECTOR => Some("precompile-b20-MINT_RECEIVER_POLICY"),
            IB20::hasRoleCall::SELECTOR => Some("precompile-b20-hasRole"),
            IB20::getRoleAdminCall::SELECTOR => Some("precompile-b20-getRoleAdmin"),
            IB20::pausedFeaturesCall::SELECTOR => Some("precompile-b20-pausedFeatures"),
            IB20::policyIdCall::SELECTOR => Some("precompile-b20-policyId"),
            IB20::isPausedCall::SELECTOR => Some("precompile-b20-isPaused"),
            IB20::DOMAIN_SEPARATORCall::SELECTOR => Some("precompile-b20-DOMAIN_SEPARATOR"),
            IB20::eip712DomainCall::SELECTOR => Some("precompile-b20-eip712Domain"),
            IB20::transferCall::SELECTOR => Some("precompile-b20-transfer"),
            IB20::transferFromCall::SELECTOR => Some("precompile-b20-transferFrom"),
            IB20::approveCall::SELECTOR => Some("precompile-b20-approve"),
            IB20::transferWithMemoCall::SELECTOR => Some("precompile-b20-transferWithMemo"),
            IB20::transferFromWithMemoCall::SELECTOR => Some("precompile-b20-transferFromWithMemo"),
            IB20::mintCall::SELECTOR => Some("precompile-b20-mint"),
            IB20::mintWithMemoCall::SELECTOR => Some("precompile-b20-mintWithMemo"),
            IB20::burnCall::SELECTOR => Some("precompile-b20-burn"),
            IB20::burnWithMemoCall::SELECTOR => Some("precompile-b20-burnWithMemo"),
            IB20::burnBlockedCall::SELECTOR => Some("precompile-b20-burnBlocked"),
            IB20::pauseCall::SELECTOR => Some("precompile-b20-pause"),
            IB20::unpauseCall::SELECTOR => Some("precompile-b20-unpause"),
            IB20::updateSupplyCapCall::SELECTOR => Some("precompile-b20-updateSupplyCap"),
            IB20::updateNameCall::SELECTOR => Some("precompile-b20-updateName"),
            IB20::updateSymbolCall::SELECTOR => Some("precompile-b20-updateSymbol"),
            IB20::updateContractURICall::SELECTOR => Some("precompile-b20-updateContractURI"),
            IB20::grantRoleCall::SELECTOR => Some("precompile-b20-grantRole"),
            IB20::revokeRoleCall::SELECTOR => Some("precompile-b20-revokeRole"),
            IB20::renounceRoleCall::SELECTOR => Some("precompile-b20-renounceRole"),
            IB20::renounceLastAdminCall::SELECTOR => Some("precompile-b20-renounceLastAdmin"),
            IB20::setRoleAdminCall::SELECTOR => Some("precompile-b20-setRoleAdmin"),
            IB20::updatePolicyCall::SELECTOR => Some("precompile-b20-updatePolicy"),
            IB20::permitCall::SELECTOR => Some("precompile-b20-permit"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;

    use super::*;

    #[test]
    fn resolves_b20_cycle_tracker_key() {
        assert_eq!(
            B20TokenPrecompile::key_for_calldata(&IB20::transferCall::SELECTOR),
            Some("precompile-b20-transfer")
        );
        assert_eq!(
            B20TokenPrecompile::key_for_calldata(&IB20::updateSupplyCapCall::SELECTOR),
            Some("precompile-b20-updateSupplyCap")
        );
    }
}
