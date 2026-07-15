//! ABI dispatch for the stablecoin B-20 variant.
//!
//! The dispatcher owns everything that is *not* version-specific: it decodes the
//! (via [`StablecoinVersions`]), and routes each operation — including reads — to
//! the active version's [`Stablecoin`] implementation. Only constant getters
//! (role IDs, policy type IDs, `decimals`) that are invariant across all versions
//! are answered inline.

use alloc::string::ToString;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolInterface, SolValue};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    B20PolicyType, B20StablecoinToken, B20TokenRole, B20Variant, BerylCallRecorder,
    BerylMetricLabels, BerylSelector,
    IB20::{self, IB20Calls as C},
    IB20Stablecoin::{self, IB20StablecoinCalls as SC},
    NoopPrecompileCallObserver, PermitArgs, Policy, PrecompileCallObserver, StablecoinAccounting,
    StablecoinV1, StablecoinVersion, StablecoinVersions,
    macros::decode_precompile_call,
};

impl<S: StablecoinAccounting, P: Policy> B20StablecoinToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20` handler for `upgrade`.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, upgrade, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded stablecoin B-20 operation.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder = BerylCallRecorder::start(
            observer.clone(),
            BerylMetricLabels::b20_stablecoin_call(calldata),
        );
        if !ctx.call_value().is_zero() {
            return recorder
                .record_base_error_result(ctx, BasePrecompileError::revert(IB20::NonPayable {}));
        }
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        // Gate by hardfork: resolve the active version once. `None` is unreachable in practice —
        // the precompile is only installed from Beryl — but we revert defensively.
        let Some(version) = StablecoinVersions::from_base_upgrade(upgrade) else {
            return recorder
                .record_base_error_result(ctx, BasePrecompileError::Revert(Bytes::new()));
        };
        // Ensure the token has been deployed (has bytecode at its address).
        match version.implementation().is_initialized(self) {
            Ok(true) => {}
            Ok(false) => {
                return recorder
                    .record_base_error_result(ctx, BasePrecompileError::Revert(Bytes::new()));
            }
            Err(error) => return recorder.record_base_error_result(ctx, error),
        }
        recorder.record_base_result(ctx, self.route(ctx, calldata, version, false, observer), |b| b)
    }

    /// Decodes calldata and executes the matching `IB20` operation for `upgrade`.
    pub fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> base_precompile_storage::Result<Bytes> {
        self.inner_with_observer(ctx, calldata, upgrade, NoopPrecompileCallObserver)
    }

    /// Decodes calldata, observes the decoded operation, and executes the matching handler
    /// against the version active at `upgrade`.
    pub fn inner_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        let Some(version) = StablecoinVersions::from_base_upgrade(upgrade) else {
            return Err(BasePrecompileError::Revert(Bytes::new()));
        };
        self.route(ctx, calldata, version, false, observer)
    }

    /// Decodes calldata and executes it with factory-init privilege.
    pub fn inner_with_privilege(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        self.route(ctx, calldata, StablecoinVersion::V1, privileged, NoopPrecompileCallObserver)
    }

    /// Grants `role` to `account` without checking caller authorization.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and the
    /// authorized [`Stablecoin::grant_role`](crate::Stablecoin) path is not yet reachable.
    // TODO: When factory get's logic for threading fork, remove this and pull in versions into the factory to use that function
    pub fn grant_role_unchecked(
        &mut self,
        role: B256,
        account: Address,
        sender: Address,
    ) -> base_precompile_storage::Result<()> {
        StablecoinV1.grant_role_unchecked(self, role, account, sender)
    }

    /// Decodes calldata, observes the decoded operation, and routes it to `version` with optional
    /// factory-init privilege.
    fn route<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: StablecoinVersion,
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        let logic = version.implementation();

        if let Some(selector) = BerylSelector::selector(calldata)
            && IB20Stablecoin::IB20StablecoinCalls::valid_selector(selector)
        {
            let call = IB20Stablecoin::IB20StablecoinCalls::abi_decode_validate(calldata).map_err(
                |error| BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() },
            )?;
            let label = call.as_label();
            return observer.observe(label, || {
                let encoded: Bytes = match call {
                    SC::currency(_) => logic.currency(self)?.abi_encode().into(),
                };
                Ok(encoded)
            });
        }

        let call = decode_precompile_call!(calldata, IB20::IB20Calls);
        let label = call.as_label();

        observer.observe(label, || {
            let encoded: Bytes = match call {
                // --- Pure reads: direct to accounting ---
                C::name(_) => logic.name(self)?.abi_encode().into(),
                C::symbol(_) => logic.symbol(self)?.abi_encode().into(),
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
                C::totalSupply(_) => logic.total_supply(self)?.abi_encode().into(),
                C::balanceOf(c) => logic.balance_of(self, c.account)?.abi_encode().into(),
                C::allowance(c) => logic.allowance(self, c.owner, c.spender)?.abi_encode().into(),
                C::supplyCap(_) => logic.supply_cap(self)?.abi_encode().into(),
                C::nonces(c) => logic.nonce(self, c.owner)?.abi_encode().into(),
                C::contractURI(_) => logic.contract_uri(self)?.abi_encode().into(),
                C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode().into(),
                C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
                C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
                C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
                C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
                C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
                C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),
                C::TRANSFER_SENDER_POLICY(_) => {
                    B20PolicyType::TransferSender.id().abi_encode().into()
                }
                C::TRANSFER_RECEIVER_POLICY(_) => {
                    B20PolicyType::TransferReceiver.id().abi_encode().into()
                }
                C::TRANSFER_EXECUTOR_POLICY(_) => {
                    B20PolicyType::TransferExecutor.id().abi_encode().into()
                }
                C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode().into(),
                C::hasRole(c) => logic.has_role(self, c.role, c.account)?.abi_encode().into(),
                C::getRoleAdmin(c) => logic.role_admin(self, c.role)?.abi_encode().into(),
                C::pausedFeatures(_) => logic.paused_features(self)?.abi_encode().into(),
                C::policyId(c) => logic.policy_id(self, c.policyScope)?.abi_encode().into(),

                // --- Computed reads: routed to the active version's logic ---
                C::isPaused(c) => logic.is_paused(self, c.feature)?.abi_encode().into(),
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

                // --- Mutating operations: routed to the active version's logic ---
                C::transfer(c) => {
                    let caller = ctx.caller();
                    logic.transfer(self, caller, c.to, c.amount, privileged)?;
                    true.abi_encode().into()
                }
                C::transferFrom(c) => {
                    let caller = ctx.caller();
                    logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged)?;
                    true.abi_encode().into()
                }
                C::approve(c) => {
                    let caller = ctx.caller();
                    logic.approve(self, caller, c.spender, c.amount)?;
                    true.abi_encode().into()
                }
                C::transferWithMemo(c) => {
                    let caller = ctx.caller();
                    logic.transfer(self, caller, c.to, c.amount, privileged)?;
                    logic.emit_memo(self, caller, c.memo)?;
                    true.abi_encode().into()
                }
                C::transferFromWithMemo(c) => {
                    let caller = ctx.caller();
                    logic.transfer_from(self, caller, c.from, c.to, c.amount, privileged)?;
                    logic.emit_memo(self, caller, c.memo)?;
                    true.abi_encode().into()
                }

                C::mint(c) => {
                    let caller = ctx.caller();
                    logic.mint(self, caller, c.to, c.amount, privileged)?;
                    Bytes::new()
                }
                C::mintWithMemo(c) => {
                    let caller = ctx.caller();
                    logic.mint(self, caller, c.to, c.amount, privileged)?;
                    logic.emit_memo(self, caller, c.memo)?;
                    Bytes::new()
                }

                C::burn(c) => {
                    let caller = ctx.caller();
                    logic.burn(self, caller, c.amount)?;
                    Bytes::new()
                }
                C::burnWithMemo(c) => {
                    let caller = ctx.caller();
                    logic.burn(self, caller, c.amount)?;
                    logic.emit_memo(self, caller, c.memo)?;
                    Bytes::new()
                }
                C::burnBlocked(c) => {
                    let caller = ctx.caller();
                    logic.burn_blocked(self, caller, c.from, c.amount, privileged)?;
                    Bytes::new()
                }

                C::pause(c) => {
                    let caller = ctx.caller();
                    logic.pause(self, caller, c.features, privileged)?;
                    Bytes::new()
                }
                C::unpause(c) => {
                    let caller = ctx.caller();
                    logic.unpause(self, caller, c.features, privileged)?;
                    Bytes::new()
                }

                C::updateSupplyCap(c) => {
                    let caller = ctx.caller();
                    logic.update_supply_cap(self, caller, c.newSupplyCap, privileged)?;
                    Bytes::new()
                }
                C::updateName(c) => {
                    let caller = ctx.caller();
                    logic.update_name(self, caller, c.newName, privileged)?;
                    Bytes::new()
                }
                C::updateSymbol(c) => {
                    let caller = ctx.caller();
                    logic.update_symbol(self, caller, c.newSymbol, privileged)?;
                    Bytes::new()
                }
                C::updateContractURI(c) => {
                    let caller = ctx.caller();
                    logic.update_contract_uri(self, caller, c.newURI, privileged)?;
                    Bytes::new()
                }
                C::grantRole(c) => {
                    let caller = ctx.caller();
                    logic.grant_role(self, caller, c.role, c.account, privileged)?;
                    Bytes::new()
                }
                C::revokeRole(c) => {
                    let caller = ctx.caller();
                    logic.revoke_role(self, caller, c.role, c.account, privileged)?;
                    Bytes::new()
                }
                C::renounceRole(c) => {
                    let caller = ctx.caller();
                    logic.renounce_role(self, caller, c.role, c.callerConfirmation)?;
                    Bytes::new()
                }
                C::renounceLastAdmin(_) => {
                    let caller = ctx.caller();
                    logic.renounce_last_admin(self, caller)?;
                    Bytes::new()
                }
                C::setRoleAdmin(c) => {
                    let caller = ctx.caller();
                    logic.set_role_admin(self, caller, c.role, c.newAdminRole, privileged)?;
                    Bytes::new()
                }
                C::updatePolicy(c) => {
                    let caller = ctx.caller();
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
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{
        B20StablecoinToken, IB20, InMemoryPolicy, InMemoryTokenAccounting,
        NoopPrecompileCallObserver, TestStablecoinToken,
    };

    const TOKEN: Address = Address::repeat_byte(0x01);

    fn make_stablecoin_token_with_decimals(decimals: u8) -> TestStablecoinToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.decimals = decimals;
        TestStablecoinToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn call_inner(token: &mut TestStablecoinToken, calldata: &[u8]) -> Vec<u8> {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(TOKEN);
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata, BaseUpgrade::Beryl))
            .unwrap()
            .to_vec()
    }

    fn make_token() -> B20StablecoinToken<InMemoryTokenAccounting, InMemoryPolicy> {
        B20StablecoinToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN),
            InMemoryPolicy::new(),
        )
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

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: Address::ZERO }.abi_encode();
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));

        let out = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_with_observer(
                ctx,
                &calldata,
                BaseUpgrade::Beryl,
                NoopPrecompileCallObserver,
            )
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
    }
}
