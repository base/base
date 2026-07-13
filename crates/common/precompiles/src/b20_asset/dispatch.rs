//! Shared precompile entry for the asset B-20 variant.
//!
//! This is the version-agnostic execution glue: the non-payable check,
//! calldata-gas deduction, initialization check, and observer/recorder
//! plumbing. After the init check it resolves to the active
//! [`B20AssetVersionId`] and hands off to that version's self-contained
//! selector routing + business logic (see [`crate::b20_asset::logic`]).

use alloy_primitives::Bytes;
use base_precompile_storage::{BasePrecompileError, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    AssetAccounting, B20AssetToken, B20AssetVersionId, BerylCallRecorder, BerylMetricLabels,
    IB20, NoopPrecompileCallObserver, Policy, PrecompileCallObserver, Token,
};

impl<S: AssetAccounting, P: Policy> B20AssetToken<S, P> {
    /// ABI-dispatches `calldata` using the current (V1) execution logic.
    pub fn dispatch(&mut self, ctx: StorageCtx<'_>, calldata: &[u8]) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` and observes the decoded asset B-20 operation,
    /// using the current (V1) execution logic.
    ///
    /// Convenience entry for callers and tests that exercise today's behavior
    /// directly. The fork-routed entry point is [`dispatch_with_version`], which
    /// the precompile install path uses to select the version active at the
    /// current hardfork.
    ///
    /// [`dispatch_with_version`]: Self::dispatch_with_version
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        self.dispatch_with_version(ctx, calldata, observer, B20AssetVersionId::V1)
    }

    /// Shared precompile entry: performs the non-payable check, calldata-gas
    /// deduction, initialization check, and observer/recorder plumbing, then
    /// delegates to `version`'s self-contained selector routing + business logic.
    ///
    /// Everything up to and including the init check is shared across versions;
    /// only the final `version.run` hand-off is version-specific.
    pub fn dispatch_with_version<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
        version: B20AssetVersionId,
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
        recorder.record_base_result(ctx, version.run(self, ctx, calldata, observer), |b| b)
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
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        B20AssetStorage, B20AssetToken, B20TokenRole, BerylErrorKind, IB20, IB20Asset,
        InMemoryPolicy, InMemoryTokenAccounting, NoopPrecompileCallObserver, PrecompileCallMetric,
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
    const ACTIVATION_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN));
    fn make_token() -> TestAssetToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN);
        accounting.multiplier = B20AssetStorage::WAD; // 1:1 multiplier
        TestAssetToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
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
