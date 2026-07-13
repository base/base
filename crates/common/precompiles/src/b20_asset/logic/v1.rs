//! Version 1 of the `b20_asset` precompile, active from Beryl onward.
//!
//! Self-contained: V1 owns its own selector routing (the relocated
//! `inner_with_observer` entry) and delegates to the shared, generic
//! [`B20AssetToken`] business logic. This is today's behavior wrapped verbatim —
//! no logic change. Selectors added by future versions fall through to the
//! existing decode paths, so V1 stays frozen once activated.

use alloc::string::ToString;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use crate::{
    AssetAccounting, B20AssetStorage, B20AssetToken, B20AssetVersion, B20TokenRole,
    BerylAuxiliaryMetrics, BerylSelector, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    IB20Asset::{self, IB20AssetCalls as SC},
    Mintable, NoopPrecompileCallObserver, Pausable, PermitArgs, Permittable, Policy,
    PrecompileCallObserver, RoleManaged, Token, Transferable,
    macros::decode_precompile_call,
};

/// First `b20_asset` execution-logic version. Frozen as of its activation at
/// Beryl; wraps today's routing and business logic verbatim.
#[derive(Debug, Default, Clone, Copy)]
pub struct B20AssetV1;

impl B20AssetVersion for B20AssetV1 {
    fn run<S, P, O>(
        &self,
        token: &mut B20AssetToken<S, P>,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> Result<Bytes>
    where
        S: AssetAccounting,
        P: Policy,
        O: PrecompileCallObserver,
    {
        token.inner_with_observer(ctx, calldata, observer)
    }
}

impl<S: AssetAccounting, P: Policy> B20AssetToken<S, P> {
    /// Decodes calldata and executes the matching `IB20Asset` or inherited `IB20` operation.
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
        // Asset-specific and overridden selectors are caught here first.
        if let Some(selector) = BerylSelector::selector(calldata)
            && IB20Asset::IB20AssetCalls::valid_selector(selector)
        {
            let call =
                IB20Asset::IB20AssetCalls::abi_decode_validate(calldata).map_err(|error| {
                    BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
                })?;
            let label = call.as_label();
            let asset_observer = observer.clone();
            return observer.observe(label, move || {
                self.handle_asset_call(ctx, call, privileged, asset_observer)
            });
        }

        // Fall through to inherited IB20 selectors.
        let call = decode_precompile_call!(calldata, IB20::IB20Calls);
        let label = call.as_label();

        observer.observe(label, || self.handle_b20_call(ctx, call, privileged))
    }

    fn handle_b20_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: C,
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        let encoded: Bytes = match call {
            // --- Pure reads ---
            C::name(_) => self.accounting().name()?.abi_encode().into(),
            C::symbol(_) => self.accounting().symbol()?.abi_encode().into(),
            C::decimals(_) => {
                U256::from(AssetAccounting::decimals(self.accounting())?).abi_encode().into()
            }
            C::totalSupply(_) => self.accounting().total_supply()?.abi_encode().into(),
            C::balanceOf(c) => self.accounting().balance_of(c.account)?.abi_encode().into(),
            C::allowance(c) => self.accounting().allowance(c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => self.accounting().supply_cap()?.abi_encode().into(),
            C::nonces(c) => self.accounting().nonce(c.owner)?.abi_encode().into(),
            C::contractURI(_) => self.accounting().contract_uri()?.abi_encode().into(),

            // --- Role identifiers ---
            C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),

            // --- Policy type identifiers ---
            C::TRANSFER_SENDER_POLICY(_) => Self::transfer_sender_policy().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => Self::transfer_receiver_policy().abi_encode().into(),
            C::TRANSFER_EXECUTOR_POLICY(_) => Self::transfer_executor_policy().abi_encode().into(),
            C::MINT_RECEIVER_POLICY(_) => Self::mint_receiver_policy().abi_encode().into(),

            // --- Role reads ---
            C::hasRole(c) => self.has_role(c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => self.role_admin(c.role)?.abi_encode().into(),

            // --- Pause reads ---
            C::pausedFeatures(_) => self.paused_features()?.abi_encode().into(),
            C::isPaused(c) => self.is_paused(c.feature)?.abi_encode().into(),

            // --- Policy reads ---
            C::policyId(c) => self.policy_id(c.policyScope)?.abi_encode().into(),

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

    fn handle_asset_call<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        call: SC,
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        let caller = ctx.caller();
        let encoded: Bytes = match call {
            // --- Role / precision constants ---
            SC::OPERATOR_ROLE(_) => Self::OPERATOR_ROLE.abi_encode().into(),
            SC::WAD_PRECISION(_) => B20AssetStorage::WAD.abi_encode().into(),

            // --- Multiplier reads ---
            SC::multiplier(_) => self.accounting().multiplier()?.abi_encode().into(),
            SC::toScaledBalance(c) => self.to_scaled_balance(c.rawBalance)?.abi_encode().into(),
            SC::toRawBalance(c) => self.to_raw_balance(c.scaledBalance)?.abi_encode().into(),
            SC::scaledBalanceOf(c) => self.scaled_balance_of(c.account)?.abi_encode().into(),

            // --- Announcement reads ---
            SC::isAnnouncementIdUsed(c) => {
                self.accounting().is_announcement_id_used(c.id.as_str())?.abi_encode().into()
            }

            // --- Extra metadata reads ---
            SC::extraMetadata(c) => {
                self.accounting().extra_metadata(c.key.as_str())?.abi_encode().into()
            }

            // --- Multiplier mutations ---
            SC::updateMultiplier(c) => {
                self.update_multiplier(caller, c.newMultiplier, privileged)?;
                Bytes::new()
            }

            // --- Announcement ---
            SC::announce(c) => {
                self.announce(ctx, c, privileged, &observer)?;
                Bytes::new()
            }

            // --- Batched mint ---
            SC::batchMint(c) => {
                observer.record_batch_items(
                    &BerylAuxiliaryMetrics::b20("asset", "batchMint"),
                    c.recipients.len(),
                );
                self.batch_mint(caller, c.recipients, c.amounts, privileged)?;
                Bytes::new()
            }

            // --- Extra metadata mutations ---
            SC::updateExtraMetadata(c) => {
                self.update_extra_metadata(caller, c.key, c.value, privileged)?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    /// Posts an announcement and atomically executes `internal_calls` via self-dispatch.
    ///
    /// The selector check in the inner loop prevents recursive invocation.
    fn announce<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        call: IB20Asset::announceCall,
        privileged: bool,
        observer: &O,
    ) -> base_precompile_storage::Result<()>
    where
        O: PrecompileCallObserver,
    {
        let caller = ctx.caller();
        let internal_calls = call.internalCalls;
        let internal_call_count = internal_calls.len();
        let internal_call_bytes = internal_calls.iter().map(|call| call.len()).sum();
        observer.record_internal_calls(
            &BerylAuxiliaryMetrics::b20("asset", "announce"),
            internal_call_count,
            internal_call_bytes,
        );
        self.ensure_operator_role(caller, privileged)?;

        let id = call.id;
        if self.accounting().is_announcement_id_used(id.as_str())? {
            return Err(BasePrecompileError::revert(IB20Asset::AnnouncementIdAlreadyUsed { id }));
        }
        self.accounting_mut().mark_announcement_id_used(id.as_str())?;

        self.accounting_mut().emit_event(
            IB20Asset::Announcement {
                caller,
                id: id.clone(),
                description: call.description,
                uri: call.uri,
            }
            .encode_log_data(),
        )?;

        // Each internal call is dispatched via `inner_with_privilege`, a direct Rust function
        // call. Unlike the base-std Solidity reference which routes each `internalCalls` entry
        // through a DELEGATECALL (~100 gas opcode overhead + memory expansion), the native
        // precompile replaces the entire EVM execution path so per-opcode call overhead does not
        // apply. The cheaper batched cost is intentional: the native precompile pays for the
        // storage work of each sub-call (the same SLOAD/SSTORE operations as the Solidity
        // reference) but not for EVM call-frame overhead that exists only in the interpreter.
        for call in &internal_calls {
            let call_bytes: &[u8] = call.as_ref();
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Asset::InternalCallMalformed {
                    call: call.clone(),
                }));
            }
            if call_bytes[..4] == IB20Asset::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
            }
            self.inner_with_privilege(ctx, call_bytes, privileged).map_err(|err| {
                if err.is_system_error() {
                    err
                } else {
                    BasePrecompileError::revert(IB20Asset::InternalCallFailed {
                        call: call.clone(),
                    })
                }
            })?;
        }

        self.accounting_mut().emit_event(IB20Asset::EndAnnouncement { id }.encode_log_data())
    }
}
