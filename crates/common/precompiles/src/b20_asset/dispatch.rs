//! ABI dispatch for the asset B-20 variant.
//!
//! The dispatcher owns everything that is *not* version-specific: it decodes the
//! calldata, resolves the active version once from the hardfork (via
//! [`AssetVersions`]), and routes each operation — including reads — to the active
//! version's [`Asset`] implementation. Only constant getters (role IDs, policy type
//! IDs) that are invariant across all versions are answered inline. The `announce`
//! internal-call loop stays here because re-dispatching arbitrary sub-calls is a
//! routing responsibility; its version-defined business steps live on [`Asset`].

use alloc::string::String;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{abi, SolCall, SolType, SolValue};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    AssetAccounting, AssetCall, AssetVersion, AssetVersions, B20AssetStorage, B20AssetToken,
    B20PolicyType, B20TokenRole, BerylAuxiliaryMetrics, BerylCallRecorder, BerylMetricLabels,
    IB20::{self, IB20Calls as C},
    IB20Asset::{self, IB20AssetCalls as SC},
    NoopPrecompileCallObserver, PermitArgs, PolicyAccounting, PrecompileCallObserver,
};

impl<S: AssetAccounting, A: PolicyAccounting> B20AssetToken<S, A> {
    /// ABI-dispatches `calldata` to the appropriate handler for `upgrade`.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, upgrade, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded asset B-20 operation.
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
        let mut recorder =
            BerylCallRecorder::start(observer.clone(), BerylMetricLabels::b20_asset_call(calldata));
        if !ctx.call_value().is_zero() {
            return recorder
                .record_base_error_result(ctx, BasePrecompileError::revert(IB20::NonPayable {}));
        }
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        // Gate by hardfork: resolve the active version once. `None` is unreachable in practice —
        // the precompile is only installed from Beryl — but we revert defensively.
        let Some(version) = AssetVersions::from_base_upgrade(upgrade) else {
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

    /// Grants `role` to `account` without checking caller authorization, using the token logic
    /// implementation active at `upgrade`.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and the
    /// authorized [`Asset::grant_role`](crate::Asset) path is not yet reachable.
    pub fn grant_role_unchecked(
        &mut self,
        role: alloy_primitives::B256,
        account: alloy_primitives::Address,
        sender: alloy_primitives::Address,
        upgrade: BaseUpgrade,
    ) -> base_precompile_storage::Result<()> {
        // `None` is unreachable in practice — the precompile is only installed from Beryl — but
        // we revert defensively, mirroring `dispatch_with_observer`.
        let Some(version) = AssetVersions::from_base_upgrade(upgrade) else {
            return Err(BasePrecompileError::Revert(Bytes::new()));
        };
        version.implementation().grant_role_unchecked(self, role, account, sender)
    }

    /// Decodes calldata, observes the decoded operation, and routes it to `version` with optional
    /// factory-init privilege.
    pub fn route<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: AssetVersion,
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        // `announce` is intercepted before the generic decode so its `bytes[] internalCalls` is
        // decoded borrowed (never materialized into an owned `Vec<Bytes>`). Routing it through the
        // canonical enum would force that owned expansion, which an aliased payload can amplify into
        // a heap-exhaustion DoS. Gated on `valid_selector` so a future fork that drops `announce`
        // from the asset surface falls through to the generic unknown-selector path.
        if let Some(selector) = calldata.first_chunk::<4>().copied()
            && selector == IB20Asset::announceCall::SELECTOR
            && version.abi().asset.valid_selector(selector)
        {
            return observer.observe("precompile-b20-asset-announce", || {
                self.announce(ctx, calldata, version, privileged, &observer)?;
                Ok(Bytes::new())
            });
        }

        let call = version.abi().decode(calldata)?;
        let label = call.as_label();
        match call {
            AssetCall::Asset(call) => {
                let asset_observer = observer.clone();
                observer.observe(label, move || {
                    self.handle_asset_call(ctx, call, version, privileged, asset_observer)
                })
            }
            AssetCall::Common(call) => {
                observer.observe(label, || self.handle_b20_call(ctx, call, version, privileged))
            }
        }
    }

    fn handle_b20_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: C,
        version: AssetVersion,
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        let logic = version.implementation();
        let caller = ctx.caller();
        let encoded: Bytes = match call {
            // --- Pure reads (routed to the active version) ---
            C::name(_) => logic.name(self)?.abi_encode().into(),
            C::symbol(_) => logic.symbol(self)?.abi_encode().into(),
            C::decimals(_) => U256::from(logic.decimals(self)?).abi_encode().into(),
            C::totalSupply(_) => logic.total_supply(self)?.abi_encode().into(),
            C::balanceOf(c) => logic.balance_of(self, c.account)?.abi_encode().into(),
            C::allowance(c) => logic.allowance(self, c.owner, c.spender)?.abi_encode().into(),
            C::supplyCap(_) => logic.supply_cap(self)?.abi_encode().into(),
            C::nonces(c) => logic.nonce(self, c.owner)?.abi_encode().into(),
            C::contractURI(_) => logic.contract_uri(self)?.abi_encode().into(),

            // --- Role identifiers (invariant across versions) ---
            C::DEFAULT_ADMIN_ROLE(_) => B20TokenRole::DefaultAdmin.id().abi_encode().into(),
            C::MINT_ROLE(_) => B20TokenRole::Mint.id().abi_encode().into(),
            C::BURN_ROLE(_) => B20TokenRole::Burn.id().abi_encode().into(),
            C::BURN_BLOCKED_ROLE(_) => B20TokenRole::BurnBlocked.id().abi_encode().into(),
            C::SEIZE_ROLE(_) => B20TokenRole::Seize.id().abi_encode().into(),
            C::PAUSE_ROLE(_) => B20TokenRole::Pause.id().abi_encode().into(),
            C::UNPAUSE_ROLE(_) => B20TokenRole::Unpause.id().abi_encode().into(),
            C::METADATA_ROLE(_) => B20TokenRole::Metadata.id().abi_encode().into(),

            // --- Policy type identifiers (invariant across versions) ---
            C::TRANSFER_SENDER_POLICY(_) => B20PolicyType::TransferSender.id().abi_encode().into(),
            C::TRANSFER_RECEIVER_POLICY(_) => {
                B20PolicyType::TransferReceiver.id().abi_encode().into()
            }
            C::TRANSFER_EXECUTOR_POLICY(_) => {
                B20PolicyType::TransferExecutor.id().abi_encode().into()
            }
            C::MINT_RECEIVER_POLICY(_) => B20PolicyType::MintReceiver.id().abi_encode().into(),
            C::SEIZE_HOLDER_POLICY(_) => B20PolicyType::SeizeHolder.id().abi_encode().into(),
            C::SEIZE_RECEIVER_POLICY(_) => B20PolicyType::SeizeReceiver.id().abi_encode().into(),

            // --- Role reads ---
            C::hasRole(c) => logic.has_role(self, c.role, c.account)?.abi_encode().into(),
            C::getRoleAdmin(c) => logic.role_admin(self, c.role)?.abi_encode().into(),

            // --- Pause reads ---
            C::pausedFeatures(_) => logic.paused_features(self)?.abi_encode().into(),
            C::isPaused(c) => logic.is_paused(self, c.feature)?.abi_encode().into(),

            // --- Policy reads ---
            C::policyId(c) => logic.policy_id(self, c.policyScope)?.abi_encode().into(),

            // --- Domain reads ---
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

            // --- ERC-20 mutating ---
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

            // --- Mint ---
            C::mint(c) => {
                logic.mint(self, caller, c.to, c.amount, privileged)?;
                Bytes::new()
            }
            C::mintWithMemo(c) => {
                logic.mint(self, caller, c.to, c.amount, privileged)?;
                logic.emit_memo(self, caller, c.memo)?;
                Bytes::new()
            }

            // --- Burn ---
            // Self-burn operations are never factory-privileged: during init the caller is the
            // factory, not a token holder.
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

            // --- Seize ---
            C::seizeWithMemo(c) => {
                logic.seize_with_memo(self, caller, c.from, c.to, c.amount, c.memo)?;
                Bytes::new()
            }

            // --- Pause ---
            C::pause(c) => {
                logic.pause(self, caller, c.features, privileged)?;
                Bytes::new()
            }
            C::unpause(c) => {
                logic.unpause(self, caller, c.features, privileged)?;
                Bytes::new()
            }

            // --- Admin ---
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

            // --- Role mutations ---
            C::grantRole(c) => {
                logic.grant_role(self, caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            C::revokeRole(c) => {
                logic.revoke_role(self, caller, c.role, c.account, privileged)?;
                Bytes::new()
            }
            // Renounce operations are never factory-privileged: they are only meaningful for the
            // role holder making the call after token creation.
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

            // --- Policy mutations ---
            C::updatePolicy(c) => {
                logic.update_policy(self, caller, c.policyScope, c.newPolicyId, privileged)?;
                Bytes::new()
            }

            // --- Permit ---
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

    fn handle_asset_call<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        call: SC,
        version: AssetVersion,
        privileged: bool,
        observer: O,
    ) -> base_precompile_storage::Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        let logic = version.implementation();
        let caller = ctx.caller();
        let encoded: Bytes = match call {
            SC::OPERATOR_ROLE(_) => logic.operator_role().abi_encode().into(),
            SC::WAD_PRECISION(_) => B20AssetStorage::WAD.abi_encode().into(),
            SC::MAX_UI_MULTIPLIER(_) => logic.max_ui_multiplier()?.abi_encode().into(),

            // --- Multiplier reads ---
            SC::multiplier(_) => logic.multiplier(self)?.abi_encode().into(),
            SC::uiMultiplier(_) => logic.ui_multiplier(self)?.abi_encode().into(),
            SC::newUIMultiplier(_) => logic.new_ui_multiplier(self)?.abi_encode().into(),
            SC::effectiveAt(_) => logic.effective_at(self)?.abi_encode().into(),
            SC::toScaledBalance(c) => {
                logic.to_scaled_balance(self, c.rawBalance)?.abi_encode().into()
            }
            SC::toRawBalance(c) => logic.to_raw_balance(self, c.scaledBalance)?.abi_encode().into(),
            // ERC-8056 Conversion extension: aliases of `toScaledBalance` / `toRawBalance`.
            SC::toUIAmount(c) => logic.to_scaled_balance(self, c.rawAmount)?.abi_encode().into(),
            SC::fromUIAmount(c) => logic.to_raw_balance(self, c.uiAmount)?.abi_encode().into(),
            SC::scaledBalanceOf(c) => logic.scaled_balance_of(self, c.account)?.abi_encode().into(),
            SC::balanceOfUI(c) => logic.balance_of_ui(self, c.account)?.abi_encode().into(),
            SC::totalSupplyUI(_) => logic.total_supply_ui(self)?.abi_encode().into(),

            // --- ERC-165 ---
            SC::supportsInterface(c) => {
                logic.supports_interface(c.interfaceId)?.abi_encode().into()
            }

            // --- Announcement reads ---
            SC::isAnnouncementIdUsed(c) => {
                logic.is_announcement_id_used(self, c.id.as_str())?.abi_encode().into()
            }

            // --- Extra metadata reads ---
            SC::extraMetadata(c) => logic.extra_metadata(self, c.key.as_str())?.abi_encode().into(),

            // --- Multiplier mutations ---
            SC::updateMultiplier(c) => {
                logic.update_multiplier(self, caller, c.newMultiplier, privileged)?;
                Bytes::new()
            }
            SC::updateUIMultiplier(c) => {
                logic.update_ui_multiplier(
                    self,
                    caller,
                    c.newMultiplier,
                    c.effectiveAt,
                    privileged,
                )?;
                Bytes::new()
            }
            SC::cancelUIMultiplierUpdate(_) => {
                logic.cancel_ui_multiplier_update(self, caller, privileged)?;
                Bytes::new()
            }

            // `announce` is intercepted in `route` (borrowed decode) and never reaches this enum.
            SC::announce(_) => return Err(BasePrecompileError::Revert(Bytes::new())),

            // --- Batched mint ---
            SC::batchMint(c) => {
                observer.record_batch_items(
                    &BerylAuxiliaryMetrics::b20("asset", "batchMint"),
                    c.recipients.len(),
                );
                logic.batch_mint(self, caller, c.recipients, c.amounts, privileged)?;
                Bytes::new()
            }

            // --- Extra metadata mutations ---
            SC::updateExtraMetadata(c) => {
                logic.update_extra_metadata(self, caller, c.key, c.value, privileged)?;
                Bytes::new()
            }
        };
        Ok(encoded)
    }

    /// Posts an announcement and atomically executes `internalCalls` via self-dispatch.
    ///
    /// `calldata` is the full `announce` calldata (selector included); `route` only enters here
    /// after matching the 4-byte `announce` selector. The `bytes[] internalCalls` vector is decoded
    /// **borrowed** — each entry is a slice into `calldata`, never an owned copy — so an aliased
    /// payload (many element offsets pointing at one tail) cannot amplify a small calldata into a
    /// large heap allocation. Only the three strings are materialized, and they do not fan out.
    ///
    /// The borrowed decode runs the same `decode_sequence` + `type_check` as the canonical
    /// `abi_decode_validate`, skipping only the infallible `detokenize`, so it accepts and rejects
    /// exactly the same inputs. On any rejection it defers to the existing version-aware decode
    /// ([`AssetAbiPair::decode`](crate::AssetAbiPair)) so the revert carries byte-identical consensus
    /// error bytes (frozen surface for V1, canonical for V2).
    ///
    /// Cobalt (V2) additionally meters the expanded dynamic byte length before the body runs.
    fn announce<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: AssetVersion,
        privileged: bool,
        observer: &O,
    ) -> base_precompile_storage::Result<()>
    where
        O: PrecompileCallObserver,
    {
        match Self::decode_announce_borrowed(&calldata[4..]) {
            Ok((internal_calls, id, description, uri)) => {
                let expanded_bytes: usize =
                    internal_calls.0.iter().map(|call| call.as_slice().len()).sum();
                Self::deduct_announce_expanded_gas(ctx, version, expanded_bytes)?;
                self.run_announce(
                    ctx,
                    version,
                    privileged,
                    observer,
                    String::from_utf8_lossy(id.as_slice()).into_owned(),
                    String::from_utf8_lossy(description.as_slice()).into_owned(),
                    String::from_utf8_lossy(uri.as_slice()).into_owned(),
                    internal_calls.0.len(),
                    expanded_bytes,
                    internal_calls.0.iter().map(|call| call.as_slice()),
                )
            }
            // Borrowed decode rejected. By the accept-set equality above, the existing decode also
            // rejects, so `?` returns its exact error bytes. The `Ok` arm is unreachable by that
            // equality; running the owned announce there is a total-function safety net (never a
            // valid amplifying payload) that avoids a consensus-crashing panic if the equality were
            // ever to not hold.
            Err(()) => match version.abi().decode(calldata)? {
                AssetCall::Asset(IB20Asset::IB20AssetCalls::announce(call)) => {
                    let expanded_bytes: usize =
                        call.internalCalls.iter().map(|call| call.len()).sum();
                    Self::deduct_announce_expanded_gas(ctx, version, expanded_bytes)?;
                    self.run_announce(
                        ctx,
                        version,
                        privileged,
                        observer,
                        call.id,
                        call.description,
                        call.uri,
                        call.internalCalls.len(),
                        expanded_bytes,
                        call.internalCalls.iter().map(|call| &call[..]),
                    )
                }
                _ => Err(BasePrecompileError::Revert(Bytes::new())),
            },
        }
    }

    /// Decodes `announce`'s parameters **borrowed** — each `bytes` is a slice into `rest`, not an
    /// owned copy. `rest` is the calldata with the 4-byte selector already stripped.
    ///
    /// This mirrors alloy's `abi_decode_validate` (`decode_sequence` then `type_check`) and omits
    /// only the infallible `detokenize`, so it accepts and rejects exactly the same inputs. Running
    /// `type_check` is mandatory, not optional: `string` validation rejects non-UTF-8 while
    /// `detokenize` is lossy, so skipping it would accept an `id`/`description`/`uri` the canonical
    /// path rejects — an accept-side divergence the caller's error fallback could not catch.
    fn decode_announce_borrowed(
        rest: &[u8],
    ) -> core::result::Result<<IB20Asset::announceCall as SolCall>::Token<'_>, ()> {
        let token = abi::decode_sequence::<<IB20Asset::announceCall as SolCall>::Token<'_>>(rest)
            .map_err(|_| ())?;
        <<IB20Asset::announceCall as SolCall>::Parameters<'_> as SolType>::type_check(&token)
            .map_err(|_| ())?;
        Ok(token)
    }

    /// Charges Cobalt expanded-dynamic gas for announce's `bytes[]` payload. Beryl is unchanged.
    fn deduct_announce_expanded_gas(
        ctx: StorageCtx<'_>,
        version: AssetVersion,
        expanded_bytes: usize,
    ) -> base_precompile_storage::Result<()> {
        if !matches!(version, AssetVersion::V2) || expanded_bytes == 0 {
            return Ok(());
        }
        ctx.deduct_gas(BerylCallRecorder::<NoopPrecompileCallObserver>::expanded_gas_cost(
            expanded_bytes,
        ))
    }

    /// Runs the announcement body shared by the borrowed fast path and the owned safety net:
    /// records metrics, brackets the internal-call loop with the version's
    /// [`Asset::begin_announce`]/[`Asset::end_announce`], and self-dispatches each entry.
    ///
    /// Each internal call is dispatched via `route`, a direct Rust function call. Unlike the
    /// base-std Solidity reference which routes each `internalCalls` entry through a DELEGATECALL
    /// (~100 gas opcode overhead + memory expansion), the native precompile replaces the entire
    /// EVM execution path so per-opcode call overhead does not apply. The cheaper batched cost is
    /// intentional: the native precompile pays for the storage work of each sub-call (the same
    /// SLOAD/SSTORE operations as the Solidity reference) but not for EVM call-frame overhead
    /// that exists only in the interpreter.
    #[allow(clippy::too_many_arguments)]
    fn run_announce<'c, O, I>(
        &mut self,
        ctx: StorageCtx<'_>,
        version: AssetVersion,
        privileged: bool,
        observer: &O,
        id: String,
        description: String,
        uri: String,
        internal_call_count: usize,
        internal_call_bytes: usize,
        internal_calls: I,
    ) -> base_precompile_storage::Result<()>
    where
        O: PrecompileCallObserver,
        I: Iterator<Item = &'c [u8]>,
    {
        observer.record_internal_calls(
            &BerylAuxiliaryMetrics::b20("asset", "announce"),
            internal_call_count,
            internal_call_bytes,
        );

        let logic = version.implementation();
        let caller = ctx.caller();
        logic.begin_announce(self, caller, id.clone(), description, uri, privileged)?;

        for call_bytes in internal_calls {
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Asset::InternalCallMalformed {
                    call: Bytes::copy_from_slice(call_bytes),
                }));
            }
            if call_bytes[..4] == IB20Asset::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
            }
            self.route(ctx, call_bytes, version, privileged, NoopPrecompileCallObserver).map_err(
                |err| {
                    if err.is_system_error() {
                        err
                    } else {
                        BasePrecompileError::revert(IB20Asset::InternalCallFailed {
                            call: Bytes::copy_from_slice(call_bytes),
                        })
                    }
                },
            )?;
        }

        logic.end_announce(self, id)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{HashMapStorageProvider, Result, StorageCtx};

    use crate::{
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        AssetV1, AssetVersion, B20AssetStorage, B20AssetToken, B20TokenRole, BerylCallRecorder,
        BerylErrorKind, FakePolicyAccounting, IB20, IB20Asset, InMemoryTokenAccounting,
        NoopPrecompileCallObserver, PolicyVersion, PrecompileCallMetric, PrecompileCallObserver,
        PrecompileCallOutcome, PrecompileCallStatus, Token, TokenAccounting,
    };

    type TestAssetToken = B20AssetToken<InMemoryTokenAccounting, FakePolicyAccounting>;

    /// Upgrade at which the asset precompile is active for every dispatch test.
    const UPGRADE: BaseUpgrade = BaseUpgrade::Beryl;

    #[derive(Debug, Clone, Default)]
    struct RecordingObserver {
        calls: Arc<Mutex<Vec<(PrecompileCallMetric, PrecompileCallOutcome)>>>,
    }

    impl RecordingObserver {
        fn calls(&self) -> Vec<(PrecompileCallMetric, PrecompileCallOutcome)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PrecompileCallObserver for RecordingObserver {
        fn record_call(&self, call: &PrecompileCallMetric, outcome: &PrecompileCallOutcome) {
            self.calls.lock().unwrap().push((call.clone(), *outcome));
        }
    }

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const TOKEN: Address = Address::repeat_byte(0x01);
    const ACTIVATION_ADMIN: Address = Address::repeat_byte(0xcb);
    const ACTIVATION_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN));

    fn make_token() -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD; // 1:1 multiplier
        TestAssetToken::with_storage_and_policy(
            accounting,
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
    }

    fn activate_b20_asset(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationFeature::B20Asset.id(), ACTIVATION_ADMIN_CONFIG)
        })
        .unwrap();
    }

    fn storage_with_caller(caller: Address) -> HashMapStorageProvider {
        let mut storage = HashMapStorageProvider::new(1);
        activate_b20_asset(&mut storage);
        storage.set_caller(caller);
        storage
    }

    fn call_asset(token: &mut TestAssetToken, caller: Address, calldata: Vec<u8>) -> Result<Bytes> {
        let mut storage = storage_with_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| {
            token.route(ctx, calldata.as_ref(), AssetVersion::V1, false, NoopPrecompileCallObserver)
        })
    }

    fn call_asset_v2_at(
        token: &mut TestAssetToken,
        caller: Address,
        now: U256,
        calldata: Vec<u8>,
    ) -> Result<Bytes> {
        let mut storage = storage_with_caller(caller);
        storage.set_timestamp(now);
        token.accounting_mut().timestamp = now;
        StorageCtx::enter(&mut storage, |ctx| {
            token.route(ctx, calldata.as_ref(), AssetVersion::V2, false, NoopPrecompileCallObserver)
        })
    }

    fn batch_mint_calldata(recipients: Vec<Address>, amounts: Vec<U256>) -> Vec<u8> {
        IB20Asset::batchMintCall { recipients, amounts }.abi_encode()
    }

    #[test]
    fn dispatch_with_observer_records_asset_success() {
        let observer = RecordingObserver::default();
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
        let mut storage = storage_with_caller(ALICE);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_with_observer(ctx, &calldata, UPGRADE, observer.clone())
        })
        .expect("dispatch should not fatally error");

        assert!(output.is_success());
        let calls = observer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.precompile, "b20");
        assert_eq!(calls[0].0.method, "balanceOf");
        assert_eq!(calls[0].0.variant, Some("asset"));
        assert_eq!(calls[0].1.status, PrecompileCallStatus::Success);
    }

    #[test]
    fn dispatch_with_observer_records_asset_decode_failure() {
        let observer = RecordingObserver::default();
        let mut token = make_token();
        let calldata = IB20::balanceOfCall::SELECTOR;
        let mut storage = storage_with_caller(ALICE);

        let output = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_with_observer(ctx, &calldata, UPGRADE, observer.clone())
        })
        .expect("dispatch should not fatally error");

        assert!(output.is_revert());
        let calls = observer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.precompile, "b20");
        assert_eq!(calls[0].0.method, "balanceOf");
        assert_eq!(calls[0].0.variant, Some("asset"));
        assert_eq!(calls[0].1.status, PrecompileCallStatus::Revert);
        assert_eq!(calls[0].1.error, Some(BerylErrorKind::AbiDecode));
    }

    #[test]
    fn batch_mint_increases_balances() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        call_asset(
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
    fn extra_metadata_roundtrip() {
        let mut token = make_token();

        assert_eq!(token.accounting().extra_metadata("category").unwrap(), "");
        token
            .accounting_mut()
            .set_extra_metadata_value("category", "real-world-asset".to_string())
            .unwrap();
        assert_eq!(
            token.accounting().extra_metadata("category").unwrap(),
            "real-world-asset".to_string()
        );
    }

    /// System errors produced by an inner `announce` call must propagate unchanged and must
    /// not be wrapped as [`IB20Asset::InternalCallFailed`]. A deliberately overflowing
    /// `toScaledBalance` produces `Panic(UnderOverflow)`, which `is_system_error()` returns
    /// `true` for.
    #[test]
    fn announce_inner_system_error_propagates_unchanged() {
        let mut token = make_token();
        // Any balance > 1 overflows when multiplied by this multiplier.
        token.accounting_mut().multiplier = U256::MAX / U256::from(2u64) + U256::ONE;
        token.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, ALICE), true);

        let inner_call = Bytes::from(
            IB20Asset::toScaledBalanceCall { rawBalance: U256::from(2u64) }.abi_encode(),
        );
        let calldata = IB20Asset::announceCall {
            internalCalls: alloc::vec![inner_call],
            id: String::from("test-sys-err"),
            description: String::from("test"),
            uri: String::new(),
        }
        .abi_encode();

        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();

        assert_eq!(err, base_precompile_storage::BasePrecompileError::under_overflow());
    }

    /// A non-system revert produced by an inner `announce` call must be wrapped as
    /// [`IB20Asset::InternalCallFailed`], preserving the original calldata in the error field.
    #[test]
    fn announce_inner_ordinary_revert_wraps_as_internal_call_failed() {
        let mut token = make_token();
        // ALICE has OPERATOR_ROLE (needed for announce) but not MINT_ROLE (needed for mint).
        token.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, ALICE), true);

        let inner_call = Bytes::from(IB20::mintCall { to: BOB, amount: U256::ONE }.abi_encode());
        let calldata = IB20Asset::announceCall {
            internalCalls: alloc::vec![inner_call.clone()],
            id: String::from("test-ord-revert"),
            description: String::from("test"),
            uri: String::new(),
        }
        .abi_encode();

        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();

        assert_eq!(
            err,
            base_precompile_storage::BasePrecompileError::revert(IB20Asset::InternalCallFailed {
                call: inner_call
            })
        );
    }

    #[test]
    fn route_v2_schedules_and_flips_multiplier_lazily() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, ALICE), true);
        let target = B20AssetStorage::WAD * U256::from(3u64);
        let effective_at = U256::from(1_000u64);

        call_asset_v2_at(
            &mut token,
            ALICE,
            U256::from(1u64),
            IB20Asset::updateUIMultiplierCall { newMultiplier: target, effectiveAt: effective_at }
                .abi_encode(),
        )
        .unwrap();

        let before = call_asset_v2_at(
            &mut token,
            ALICE,
            U256::from(999u64),
            IB20Asset::multiplierCall {}.abi_encode(),
        )
        .unwrap();
        assert_eq!(before, Bytes::from(B20AssetStorage::WAD.abi_encode()));

        let after = call_asset_v2_at(
            &mut token,
            ALICE,
            effective_at,
            IB20Asset::multiplierCall {}.abi_encode(),
        )
        .unwrap();
        assert_eq!(after, Bytes::from(target.abi_encode()));

        let effective_at_read = call_asset_v2_at(
            &mut token,
            ALICE,
            U256::from(1u64),
            IB20Asset::effectiveAtCall {}.abi_encode(),
        )
        .unwrap();
        assert_eq!(effective_at_read, Bytes::from(effective_at.abi_encode()));
    }

    #[test]
    fn route_v2_supports_interface() {
        let mut token = make_token();
        let out = call_asset_v2_at(
            &mut token,
            ALICE,
            U256::ZERO,
            IB20Asset::supportsInterfaceCall {
                interfaceId: alloy_primitives::FixedBytes::new([0x01, 0xff, 0xc9, 0xa7]),
            }
            .abi_encode(),
        )
        .unwrap();
        assert_eq!(out, Bytes::from(true.abi_encode()));
    }

    #[test]
    fn route_v1_rejects_scheduled_selector_as_unknown() {
        let mut token = make_token();
        let calldata = IB20Asset::updateUIMultiplierCall {
            newMultiplier: B20AssetStorage::WAD,
            effectiveAt: U256::from(2u64),
        }
        .abi_encode();
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        // Routed at V1 (Beryl) the ERC-8056 selector is absent from the frozen asset surface, so it
        // falls through to the disjoint inherited IB20 decode and stays unknown.
        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();
        assert_eq!(
            err,
            base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector)
        );
    }

    /// Pins rejection *ahead* of argument decoding: a pre-Cobalt (V1) call carrying an ERC-8056
    /// selector but non-decodable arguments must still reject as `UnknownFunctionSelector`, exactly
    /// as it did before the shared ABI enum grew; never `AbiDecodeFailed`. This falls out of the
    /// frozen V1 surface not declaring the selector — the asset branch is skipped before any
    /// argument decode, and the disjoint inherited IB20 decode yields the unknown selector. A gate
    /// that decoded first (post-decode) would leak `AbiDecodeFailed` here and fork historical Beryl.
    #[test]
    fn route_v1_rejects_scheduled_selector_with_malformed_args_as_unknown() {
        let mut token = make_token();
        // A valid `updateUIMultiplier` selector followed by truncated (non-decodable) arguments.
        let mut calldata = IB20Asset::updateUIMultiplierCall::SELECTOR.to_vec();
        calldata.extend_from_slice(&[0u8; 3]);
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();
        assert_eq!(
            err,
            base_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector)
        );
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
        let mut storage = storage_with_caller(ALICE);
        storage.set_call_value(U256::from(1u64));

        let out = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_with_observer(ctx, &calldata, UPGRADE, NoopPrecompileCallObserver)
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
    }

    /// Builds `announce` calldata where `n` `bytes[]` offsets all reference one shared `tail`.
    ///
    /// Starts from a valid one-element alloy encoding, then expands the array header so every
    /// element offset points at the same tail blob and rewrites the trailing string offsets.
    fn aliased_announce_calldata(n: usize, tail: &[u8]) -> Vec<u8> {
        assert!(n >= 1, "need at least one aliased entry");
        let base = IB20Asset::announceCall {
            internalCalls: alloc::vec![Bytes::copy_from_slice(tail)],
            id: String::from("aliased-id"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();

        // Args layout after selector:
        //   [off_calls][off_id][off_desc][off_uri][len=1][off0=0x40][bytes blob][id][desc][uri]
        let args = &base[4..];
        let read_u256 = |at: usize| -> usize {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&args[at + 24..at + 32]);
            u64::from_be_bytes(buf) as usize
        };
        let write_u256 = |out: &mut [u8], at: usize, v: usize| {
            out[at..at + 32].fill(0);
            out[at + 24..at + 32].copy_from_slice(&(v as u64).to_be_bytes());
        };

        let off_calls = read_u256(0);
        let off_id = read_u256(32);
        let off_desc = read_u256(64);
        let off_uri = read_u256(96);
        assert_eq!(read_u256(off_calls), 1, "base encode must be one-element");

        let bytes_blob = &args[off_calls + 64..off_id]; // after len+off0
        let strings = &args[off_id..];

        // New array section: length + n offsets + shared blob.
        let extra_offsets = (n - 1) * 32;
        let new_off_id = off_id + extra_offsets;
        let new_off_desc = off_desc + extra_offsets;
        let new_off_uri = off_uri + extra_offsets;
        let shared_elem_off = n * 32;

        let mut out = base[..4].to_vec();
        out.resize(4 + 128 + 32 + n * 32 + bytes_blob.len() + strings.len(), 0);
        let args_out = &mut out[4..];
        write_u256(args_out, 0, off_calls);
        write_u256(args_out, 32, new_off_id);
        write_u256(args_out, 64, new_off_desc);
        write_u256(args_out, 96, new_off_uri);
        write_u256(args_out, off_calls, n);
        for i in 0..n {
            write_u256(args_out, off_calls + 32 + i * 32, shared_elem_off);
        }
        let blob_at = off_calls + 32 + n * 32;
        args_out[blob_at..blob_at + bytes_blob.len()].copy_from_slice(bytes_blob);
        args_out[new_off_id..new_off_id + strings.len()].copy_from_slice(strings);
        out
    }



    #[test]
    fn aliased_announce_succeeds_on_beryl_without_expanded_gas() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, ALICE), true);

        let n = 1_024usize;
        // Valid zero-arg view: each aliased entry re-dispatches successfully.
        let tail = IB20Asset::multiplierCall {}.abi_encode();
        let calldata = aliased_announce_calldata(n, &tail);
        let owned_style = IB20Asset::announceCall {
            internalCalls: alloc::vec![Bytes::copy_from_slice(&tail); n],
            id: String::from("aliased-id"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();
        assert!(
            calldata.len() < owned_style.len(),
            "aliased wire {} must be smaller than owned-style {}",
            calldata.len(),
            owned_style.len()
        );
        let mut storage = storage_with_caller(ALICE);

        StorageCtx::enter(&mut storage, |ctx| {
            token.route(ctx, &calldata, AssetVersion::V1, false, NoopPrecompileCallObserver)
        })
        .expect("borrowed aliased announce must succeed on Beryl");

        assert!(token.accounting().is_announcement_id_used("aliased-id").unwrap());
    }

    #[test]
    fn aliased_announce_charges_expanded_gas_on_cobalt() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((AssetV1::OPERATOR_ROLE, ALICE), true);

        let n = 1_024usize;
        let tail = IB20Asset::multiplierCall {}.abi_encode();
        let calldata = aliased_announce_calldata(n, &tail);
        let expanded = n * tail.len();
        let expected =
            BerylCallRecorder::<NoopPrecompileCallObserver>::expanded_gas_cost(expanded);

        let mut storage = storage_with_caller(ALICE);
        StorageCtx::enter(&mut storage, |ctx| {
            token.route(ctx, &calldata, AssetVersion::V2, false, NoopPrecompileCallObserver)
        })
        .expect("borrowed aliased announce must succeed when gas is uncapped");

        // Body work also deducts gas; Cobalt must at least charge the expanded schedule.
        assert!(
            storage.gas_deducted() >= expected,
            "gas_deducted {} must cover expanded cost {}",
            storage.gas_deducted(),
            expected
        );
    }
}