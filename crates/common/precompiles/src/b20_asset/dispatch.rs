//! ABI dispatch for the asset B-20 variant.
//!
//! Asset-specific selectors are tried first via `IB20Asset::IB20AssetCalls`.
//! The `IB20` match block handles inherited selectors.

use alloc::string::ToString;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent, SolInterface, SolValue};
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    AssetAccounting, B20AssetStorage, B20AssetToken, B20TokenRole, BerylAuxiliaryMetrics,
    BerylCallRecorder, BerylMetricLabels, BerylSelector, Burnable, Configurable,
    IB20::{self, IB20Calls as C},
    IB20Asset::{self, IB20AssetCalls as SC},
    Mintable, NoopPrecompileCallObserver, Pausable, PermitArgs, Permittable, Policy,
    PrecompileCallObserver, RoleManaged, Token, Transferable,
    macros::decode_precompile_call,
};

impl<S: AssetAccounting, P: Policy> B20AssetToken<S, P> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Asset` handler.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded asset B-20 operation.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
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

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError, SolEvent};
    use base_precompile_storage::{
        BasePrecompileError, HashMapStorageProvider, Result, StorageCtx,
    };

    use crate::{
        ActivationFeature, ActivationRegistryStorage, AssetAccounting, B20AssetStorage,
        B20AssetToken, B20TokenRole, BerylErrorKind, IB20, IB20Asset, InMemoryPolicy,
        InMemoryTokenAccounting, NoopPrecompileCallObserver, PrecompileCallMetric,
        PrecompileCallObserver, PrecompileCallOutcome, PrecompileCallStatus, Token,
        TokenAccounting,
    };

    type TestAssetToken = B20AssetToken<InMemoryTokenAccounting, InMemoryPolicy>;

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
    fn make_token() -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD; // 1:1 multiplier
        TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn activate_b20_asset(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationFeature::B20Asset.id(), Some(ACTIVATION_ADMIN))
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
        StorageCtx::enter(&mut storage, |ctx| token.inner(ctx, calldata.as_ref()))
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
            token.dispatch_with_observer(ctx, &calldata, observer.clone())
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
            token.dispatch_with_observer(ctx, &calldata, observer.clone())
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
    fn to_scaled_balance_one_to_one_multiplier() {
        let token = make_token();
        assert_eq!(token.to_scaled_balance(U256::from(100u64)).unwrap(), U256::from(100u64));
    }

    #[test]
    fn to_scaled_balance_two_to_one_multiplier() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD * U256::from(2u64);
        let token = TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_scaled_balance(U256::from(50u64)).unwrap(), U256::from(100u64));
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
        assert_eq!(
            token.accounting().events,
            alloc::vec![
                IB20::Transfer { from: Address::ZERO, to: ALICE, amount: U256::from(100u64) }
                    .encode_log_data(),
                IB20::Transfer { from: Address::ZERO, to: BOB, amount: U256::from(200u64) }
                    .encode_log_data()
            ]
        );
    }

    #[test]
    fn batch_mint_requires_mint_role() {
        let mut token = make_token();

        let err = call_asset(
            &mut token,
            ALICE,
            batch_mint_calldata(alloc::vec![BOB], alloc::vec![U256::from(100u64)]),
        )
        .unwrap_err();

        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: ALICE,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
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

    // --- batchMint: EmptyBatch / LengthMismatch ---

    #[test]
    fn batch_mint_rejects_empty() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_asset(&mut token, ALICE, batch_mint_calldata(alloc::vec![], alloc::vec![]))
                .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::EmptyBatch {})
        );
    }

    #[test]
    fn batch_mint_rejects_length_mismatch() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        assert_eq!(
            call_asset(
                &mut token,
                ALICE,
                batch_mint_calldata(alloc::vec![ALICE], alloc::vec![U256::ONE, U256::ONE]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::LengthMismatch {
                leftLen: U256::ONE,
                rightLen: U256::from(2u64),
            })
        );

        assert_eq!(
            call_asset(
                &mut token,
                ALICE,
                batch_mint_calldata(alloc::vec![], alloc::vec![U256::ONE]),
            )
            .unwrap_err(),
            BasePrecompileError::revert(IB20Asset::LengthMismatch {
                leftLen: U256::ZERO,
                rightLen: U256::ONE,
            })
        );
    }

    // --- toScaledBalance: zero balance / sub-WAD truncation / scaledBalanceOf delegation ---
    #[test]
    fn to_scaled_balance_zero_balance_yields_zero() {
        let token = make_token();
        assert_eq!(token.to_scaled_balance(U256::ZERO).unwrap(), U256::ZERO);
    }

    #[test]
    fn to_scaled_balance_sub_wad_multiplier_truncates_to_zero() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // 0.5 WAD: 1 token → 0.5 scaled → truncates to 0 via integer division
        accounting.multiplier = B20AssetStorage::WAD / U256::from(2u64);
        let token = TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new());
        assert_eq!(token.to_scaled_balance(U256::from(1u64)).unwrap(), U256::ZERO);
    }

    #[test]
    fn scaled_balance_of_derives_from_balance() {
        let mut token = make_token(); // 1:1 multiplier
        token.accounting_mut().balances.insert(ALICE, U256::from(75u64));
        // scaledBalanceOf(account) = toScaledBalance(balanceOf(account))
        let balance = token.accounting().balance_of(ALICE).unwrap();
        assert_eq!(token.to_scaled_balance(balance).unwrap(), U256::from(75u64));
    }

    // --- updateMultiplier: persistence ---

    #[test]
    fn multiplier_update_persists() {
        let mut token = make_token();
        let new_multiplier = B20AssetStorage::WAD * U256::from(3u64);
        token.accounting_mut().set_multiplier(new_multiplier).unwrap();
        assert_eq!(token.accounting().multiplier().unwrap(), new_multiplier);
    }

    // --- extraMetadata / updateExtraMetadata ---

    #[test]
    fn extra_metadata_missing_key_returns_empty() {
        let token = make_token();
        // "Returns the empty string if not set"
        assert_eq!(token.accounting().extra_metadata("category").unwrap(), "");
    }

    #[test]
    fn extra_metadata_empty_value_clears_entry() {
        let mut token = make_token();
        token.accounting_mut().set_extra_metadata_value("region", "us-east".to_string()).unwrap();
        assert_eq!(token.accounting().extra_metadata("region").unwrap(), "us-east");
        // "passing an empty value removes the entry"
        token.accounting_mut().set_extra_metadata_value("region", String::new()).unwrap();
        assert_eq!(token.accounting().extra_metadata("region").unwrap(), "");
    }

    // --- isAnnouncementIdUsed: fresh state ---

    #[test]
    fn announcement_id_not_used_initially() {
        let token = make_token();
        let id = "2026-Q1-split";
        // "Returns true if id has previously been consumed by announce" → false for new id
        assert!(!token.accounting().is_announcement_id_used(id).unwrap());
    }

    /// `to_scaled_balance` must return an arithmetic overflow panic rather than silently
    /// saturating when `balance * multiplier` exceeds `U256::MAX`.
    #[test]
    fn to_scaled_balance_overflows_when_product_exceeds_u256_max() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        // Any balance > 1 overflows when multiplied by this multiplier.
        accounting.multiplier = U256::MAX / U256::from(2u64) + U256::ONE;
        let token = TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.to_scaled_balance(U256::from(2u64)).unwrap_err(),
            BasePrecompileError::under_overflow()
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
        token.accounting_mut().roles.insert((TestAssetToken::OPERATOR_ROLE, ALICE), true);

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

        assert_eq!(err, BasePrecompileError::under_overflow());
    }

    /// A non-system revert produced by an inner `announce` call must be wrapped as
    /// [`IB20Asset::InternalCallFailed`], preserving the original calldata in the error field.
    #[test]
    fn announce_inner_ordinary_revert_wraps_as_internal_call_failed() {
        let mut token = make_token();
        // ALICE has OPERATOR_ROLE (needed for announce) but not MINT_ROLE (needed for mint).
        token.accounting_mut().roles.insert((TestAssetToken::OPERATOR_ROLE, ALICE), true);

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
            BasePrecompileError::revert(IB20Asset::InternalCallFailed { call: inner_call })
        );
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
        let mut storage = storage_with_caller(ALICE);
        storage.set_call_value(U256::from(1u64));

        let out = StorageCtx::enter(&mut storage, |ctx| {
            token.dispatch_with_observer(ctx, &calldata, NoopPrecompileCallObserver)
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
    }
}
