//! ABI dispatch for the stablecoin B-20 variant.
//!
//! Dispatches the full `IB20` selector set for stablecoin tokens.
//! All logic mirrors `B20Token::inner_with_privilege` exactly; the only
//! distinction is the `StablecoinAccounting` bound that provides `currency()`
//! from the stablecoin extension namespace.

use alloc::string::ToString;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    B20StablecoinToken, B20TokenRole, B20Variant, BerylCallRecorder, BerylMetricLabels,
    BerylSelector, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    IB20Stablecoin::{self, IB20StablecoinCalls as SC},
    Mintable, NoopPrecompileCallObserver, Pausable, PermitArgs, Permittable, Policy,
    PrecompileCallObserver, RoleManaged, StablecoinAccounting, Token, Transferable,
    macros::decode_precompile_call,
};

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded stablecoin B-20 operation.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder = BerylCallRecorder::start(
            observer.clone(),
            BerylMetricLabels::b20_stablecoin_call(calldata),
        );
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        // Ensure the token has been deployed (has bytecode at its address).
        match self.accounting().is_initialized() {
            Ok(true) => {}
            Ok(false) => {
                return recorder
                    .record_base_error_result(ctx, BasePrecompileError::Revert(Bytes::new()));
            }
            Err(error) => return recorder.record_base_error_result(ctx, error),
        }
        recorder.record_base_result(ctx, self.inner_with_observer(ctx, calldata, observer), |b| b)
    }

    /// Decodes calldata and executes the matching `IB20` operation.
    pub fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// Decodes calldata, observes the decoded operation, and executes the matching handler.
    pub fn inner_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        self.inner_with_privilege_and_observer(ctx, calldata, false, observer)
    }

    /// Decodes calldata and executes it with optional factory-init privilege.
    pub fn inner_with_privilege(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_privilege_and_observer(
            ctx,
            calldata,
            privileged,
            NoopPrecompileCallObserver,
        )
    }

    /// Decodes calldata, observes the decoded operation, and executes it with optional
    /// factory-init privilege.
    pub fn inner_with_privilege_and_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        if let Some(selector) = BerylSelector::selector(calldata)
            && IB20Stablecoin::IB20StablecoinCalls::valid_selector(selector)
        {
            let call = IB20Stablecoin::IB20StablecoinCalls::abi_decode_validate(calldata).map_err(
                |error| BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() },
            )?;
            let label = call.as_label();
            return observer.observe(label, || self.handle_stablecoin_call(call));
        }

        let call = decode_precompile_call!(calldata, IB20::IB20Calls);
        let label = call.as_label();

        observer.observe(label, || {
            let encoded: Bytes = match call {
                // --- Pure reads: direct to accounting ---
                C::name(_) => self.accounting().name()?.abi_encode().into(),
                C::symbol(_) => self.accounting().symbol()?.abi_encode().into(),
                // Stablecoin precision is fixed at 6 by the protocol spec; never read from
                // storage to avoid the zero-return window during the factory bootstrap
                // (BOP-349/PSRC-27).
                C::decimals(_) => U256::from(
                    B20Variant::Stablecoin
                        .decimals()
                        .expect("stablecoin has fixed 6-decimal precision"),
                )
                .abi_encode()
                .into(),
                C::totalSupply(_) => self.accounting().total_supply()?.abi_encode().into(),
                C::balanceOf(c) => self.accounting().balance_of(c.account)?.abi_encode().into(),
                C::allowance(c) => {
                    self.accounting().allowance(c.owner, c.spender)?.abi_encode().into()
                }
                C::supplyCap(_) => self.accounting().supply_cap()?.abi_encode().into(),
                C::nonces(c) => self.accounting().nonce(c.owner)?.abi_encode().into(),
                C::contractURI(_) => self.accounting().contract_uri()?.abi_encode().into(),
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
                    // Self-burn operations are never factory-privileged: during init the caller is
                    // the factory, not a token holder.
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
                // Renounce operations are never factory-privileged: they are only meaningful for
                // the role holder making the call after token creation.
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

    fn handle_stablecoin_call(&self, call: SC) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            SC::currency(_) => self.accounting().currency()?.abi_encode().into(),
        };
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use alloy_sol_types::{SolCall, SolValue};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{IB20, InMemoryPolicy, InMemoryTokenAccounting, TestStablecoinToken};

    const TOKEN: Address = Address::repeat_byte(0x01);

    fn make_stablecoin_token_with_decimals(decimals: u8) -> TestStablecoinToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.decimals = decimals;
        TestStablecoinToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn call_inner(token: &mut TestStablecoinToken, calldata: &[u8]) -> Vec<u8> {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(TOKEN);
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata)).unwrap().to_vec()
    }

    /// Decimals always returns 6 regardless of what the underlying accounting stores.
    ///
    /// During the factory bootstrap window the storage slot is uninitialized and would
    /// return 0 if read directly. Hard-coding 6 in the dispatch eliminates that window.
    #[test]
    fn decimals_returns_fixed_six_regardless_of_storage() {
        // Token with decimals = 0 in storage (simulates an uninitialized slot).
        let mut uninitialized = make_stablecoin_token_with_decimals(0);
        let calldata = IB20::decimalsCall {}.abi_encode();
        let result = call_inner(&mut uninitialized, &calldata);
        assert_eq!(U256::abi_decode(&result).unwrap(), U256::from(6u8));

        // Token with decimals = 18 in storage (default for InMemoryTokenAccounting).
        let mut default = make_stablecoin_token_with_decimals(18);
        let result = call_inner(&mut default, &calldata);
        assert_eq!(U256::abi_decode(&result).unwrap(), U256::from(6u8));
    }
}
