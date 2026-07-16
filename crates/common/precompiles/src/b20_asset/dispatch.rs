//! ABI dispatch for the asset B-20 variant.
//!
//! The dispatcher owns everything that is *not* version-specific: it decodes the
//! calldata, resolves the active version once from the hardfork (via
//! [`AssetVersions`]), and routes each operation — including reads — to the active
//! version's [`Asset`] implementation. Only constant getters (role IDs, policy type
//! IDs) that are invariant across all versions are answered inline. The `announce`
//! internal-call loop stays here because re-dispatching arbitrary sub-calls is a
//! routing responsibility; its version-defined business steps live on [`Asset`].

use alloc::string::ToString;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolInterface, SolValue};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    AssetAccounting, AssetV1, AssetVersion, AssetVersions, B20AssetStorage, B20AssetToken,
    B20PolicyType, B20TokenRole, BerylAuxiliaryMetrics, BerylCallRecorder, BerylMetricLabels,
    BerylSelector,
    IB20::{self, IB20Calls as C},
    IB20Asset::{self, IB20AssetCalls as SC},
    NoopPrecompileCallObserver, PermitArgs, PolicyAccounting, PrecompileCallObserver,
    macros::decode_precompile_call,
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

    /// Decodes calldata and executes the matching operation for `upgrade`.
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
        let Some(version) = AssetVersions::from_base_upgrade(upgrade) else {
            return Err(BasePrecompileError::Revert(Bytes::new()));
        };
        self.route(ctx, calldata, version, false, observer)
    }

    /// Decodes calldata and executes it with factory-init privilege.
    ///
    /// Pinned to [`AssetVersion::V1`], the token's introduction version: factory-initiated setup
    /// calls run before any later fork can be relevant.
    pub fn inner_with_privilege(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        privileged: bool,
    ) -> base_precompile_storage::Result<Bytes> {
        self.route(ctx, calldata, AssetVersion::V1, privileged, NoopPrecompileCallObserver)
    }

    /// Grants `role` to `account` without checking caller authorization.
    ///
    /// The one token-level mutation the factory needs at bootstrap, when no admin exists yet and the
    /// authorized [`Asset::grant_role`](crate::Asset) path is not yet reachable. Pinned to
    /// [`AssetV1`], the token's introduction version.
    // TODO: When the factory gains fork threading, remove this and pull versions into the factory.
    pub fn grant_role_unchecked(
        &mut self,
        role: alloy_primitives::B256,
        account: alloy_primitives::Address,
        sender: alloy_primitives::Address,
    ) -> base_precompile_storage::Result<()> {
        AssetV1.grant_role_unchecked(self, role, account, sender)
    }

    /// Decodes calldata, observes the decoded operation, and routes it to `version` with optional
    /// factory-init privilege.
    fn route<O>(
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
                self.handle_asset_call(ctx, call, version, privileged, asset_observer)
            });
        }

        // Fall through to inherited IB20 selectors.
        let call = decode_precompile_call!(calldata, IB20::IB20Calls);
        let label = call.as_label();

        observer.observe(label, || self.handle_b20_call(ctx, call, version, privileged))
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

            // --- Multiplier reads ---
            SC::multiplier(_) => logic.multiplier(self)?.abi_encode().into(),
            SC::toScaledBalance(c) => {
                logic.to_scaled_balance(self, c.rawBalance)?.abi_encode().into()
            }
            SC::toRawBalance(c) => logic.to_raw_balance(self, c.scaledBalance)?.abi_encode().into(),
            SC::scaledBalanceOf(c) => logic.scaled_balance_of(self, c.account)?.abi_encode().into(),

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

            // --- Announcement ---
            SC::announce(c) => {
                self.announce(ctx, c, version, privileged, &observer)?;
                Bytes::new()
            }

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
    /// Re-dispatching arbitrary sub-calls is a routing responsibility, so it stays in the
    /// dispatcher; the version's [`Asset::begin_announce`]/[`Asset::end_announce`] bracket the loop
    /// with the version-defined business steps. Each internal call routes at the same `version`.
    /// The selector check in the inner loop prevents recursive invocation.
    fn announce<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        call: IB20Asset::announceCall,
        version: AssetVersion,
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

        let logic = version.implementation();
        let id = call.id;
        logic.begin_announce(self, caller, id.clone(), call.description, call.uri, privileged)?;

        // Each internal call is dispatched via `route`, a direct Rust function call. Unlike the
        // base-std Solidity reference which routes each `internalCalls` entry through a DELEGATECALL
        // (~100 gas opcode overhead + memory expansion), the native precompile replaces the entire
        // EVM execution path so per-opcode call overhead does not apply. The cheaper batched cost is
        // intentional: the native precompile pays for the storage work of each sub-call (the same
        // SLOAD/SSTORE operations as the Solidity reference) but not for EVM call-frame overhead
        // that exists only in the interpreter.
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
            self.route(ctx, call_bytes, version, privileged, NoopPrecompileCallObserver).map_err(
                |err| {
                    if err.is_system_error() {
                        err
                    } else {
                        BasePrecompileError::revert(IB20Asset::InternalCallFailed {
                            call: call.clone(),
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
    use alloy_sol_types::{SolCall, SolError};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{HashMapStorageProvider, Result, StorageCtx};

    use crate::{
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        AssetV1, B20AssetStorage, B20AssetToken, B20TokenRole, BerylErrorKind,
        FakePolicyAccounting, IB20, IB20Asset, InMemoryTokenAccounting, NoopPrecompileCallObserver,
        PolicyVersion, PrecompileCallMetric, PrecompileCallObserver, PrecompileCallOutcome,
        PrecompileCallStatus, Token, TokenAccounting,
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
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata.as_ref(), UPGRADE))
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
}
