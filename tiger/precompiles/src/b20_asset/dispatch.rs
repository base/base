//! ABI dispatch for the asset B-20 variant.
//!
//! Resolves the active version from the hardfork, decodes against that version's asset and common
//! surfaces, and routes common calls to the shared B-20 dispatcher. The `announce` internal-call
//! loop stays here because re-dispatching arbitrary sub-calls is a routing responsibility.

use alloc::{string::String, vec::Vec};

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolType, SolValue, abi};
use base_common_genesis::BaseUpgrade;
use base_tiger_precompile_storage::{BasePrecompileError, PrecompileResult, StorageCtx};

use crate::{
    AssetAbiPair, AssetAccounting, AssetCall, B20AssetStorage, B20Logic, B20Token, B20Version,
    IB20Asset::{self, IB20AssetCalls as SC},
    PolicyAccounting, Token,
};

impl<S: AssetAccounting, A: PolicyAccounting> B20Token<S, A> {
    /// ABI-dispatches `calldata` to the asset logic active at `upgrade`.
    pub fn dispatch_asset(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with(ctx, calldata, B20Version::asset(upgrade), |token, version| {
            token.route_asset(ctx, calldata, version, false)
        })
    }

    /// Decodes calldata and routes it to `version` with optional factory-init privilege.
    pub fn route_asset(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: B20Version,
        privileged: bool,
    ) -> base_tiger_precompile_storage::Result<Bytes> {
        // Fast-path `announce` before the generic decode; `announce` is the only B-20 selector an
        // aliased payload can amplify. `DecodedAnnounce::decode_if_announce` keeps the `bytes[]`
        // entries as slices into `calldata`, never owned copies. A call this version doesn't
        // recognize as `announce` falls through to the generic decode below, unchanged.
        match DecodedAnnounce::decode_if_announce(calldata, version) {
            Some(Ok(announce)) => {
                self.run_announce(ctx, version, privileged, announce)?;
                return Ok(Bytes::new());
            }
            Some(Err(error)) => return Err(error),
            None => {}
        }
        let logic = B20Logic::asset(version);
        match AssetAbiPair::of(version).decode(calldata)? {
            AssetCall::Asset(call) => self.handle_asset_call(ctx, call, version, privileged),
            AssetCall::Common(call) => {
                self.handle_b20_call(ctx, call, logic, privileged, AssetAccounting::decimals)
            }
        }
    }

    fn handle_asset_call(
        &mut self,
        ctx: StorageCtx<'_>,
        call: SC,
        version: B20Version,
        privileged: bool,
    ) -> base_tiger_precompile_storage::Result<Bytes> {
        let logic = B20Logic::asset(version);
        let caller = ctx.caller();
        let encoded: Bytes = match call {
            SC::OPERATOR_ROLE(_) => B20Logic::OPERATOR_ROLE.abi_encode().into(),
            SC::WAD_PRECISION(_) => B20AssetStorage::WAD.abi_encode().into(),
            SC::MAX_UI_MULTIPLIER(_) => B20Logic::MAX_UI_MULTIPLIER.abi_encode().into(),

            // --- Multiplier reads ---
            SC::multiplier(_) | SC::uiMultiplier(_) => {
                logic.effective_multiplier(self)?.abi_encode().into()
            }
            SC::newUIMultiplier(_) => logic.new_ui_multiplier(self)?.abi_encode().into(),
            SC::effectiveAt(_) => {
                U256::from(self.accounting().pending_effective_at()?).abi_encode().into()
            }
            SC::toScaledBalance(c) => {
                logic.to_scaled_balance(self, c.rawBalance)?.abi_encode().into()
            }
            SC::toRawBalance(c) => logic.to_raw_balance(self, c.scaledBalance)?.abi_encode().into(),
            // ERC-8056 Conversion extension: aliases of `toScaledBalance` / `toRawBalance`.
            SC::toUIAmount(c) => logic.to_scaled_balance(self, c.rawAmount)?.abi_encode().into(),
            SC::fromUIAmount(c) => logic.to_raw_balance(self, c.uiAmount)?.abi_encode().into(),
            SC::scaledBalanceOf(c) => logic.scaled_balance_of(self, c.account)?.abi_encode().into(),
            SC::balanceOfUI(c) => logic.scaled_balance_of(self, c.account)?.abi_encode().into(),
            SC::totalSupplyUI(_) => logic.total_supply_ui(self)?.abi_encode().into(),

            // --- ERC-165 ---
            SC::supportsInterface(c) => {
                B20Logic::supports_interface(c.interfaceId).abi_encode().into()
            }

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

            // --- Announcement ---
            // Bounded safety net. Every accepted `announce` takes the borrowed fast path in `route`
            // (identical accept-set to this owned decode). If that invariant broke, this arm still
            // executes correctly by feeding the owned call into the same runner via
            // `DecodedAnnounce::from_owned`.
            SC::announce(c) => {
                self.run_announce(ctx, version, privileged, DecodedAnnounce::from_owned(&c))?;
                Bytes::new()
            }

            // --- Batched mint ---
            SC::batchMint(c) => {
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

    /// Posts an announcement and atomically executes its internal calls via self-dispatch.
    ///
    /// One body for both entry paths: the borrowed fast path in [`Self::route`] and the owned
    /// `SC::announce` safety net both feed a [`DecodedAnnounce`] through here. The internal-call
    /// loop lives here because re-dispatching sub-calls is a routing responsibility. The version's
    /// [`B20Logic::begin_announce`]/[`B20Logic::end_announce`] bracket the loop with the version-defined
    /// business steps, and the in-loop selector check blocks recursive `announce`.
    fn run_announce(
        &mut self,
        ctx: StorageCtx<'_>,
        version: B20Version,
        privileged: bool,
        announce: DecodedAnnounce<'_>,
    ) -> base_tiger_precompile_storage::Result<()> {
        let logic = B20Logic::asset(version);
        let caller = ctx.caller();
        let DecodedAnnounce { id, description, uri, internal_calls } = announce;
        logic.begin_announce(self, caller, id.clone(), description, uri, privileged)?;

        // Each internal call is dispatched via `route`, a direct Rust function call. Unlike the
        // base-std Solidity reference which routes each `internalCalls` entry through a DELEGATECALL
        // (~100 gas opcode overhead + memory expansion), the native precompile replaces the entire
        // EVM execution path so per-opcode call overhead does not apply. The cheaper batched cost is
        // intentional: the native precompile pays for the storage work of each sub-call (the same
        // SLOAD/SSTORE operations as the Solidity reference) but not for EVM call-frame overhead
        // that exists only in the interpreter.
        for call_bytes in internal_calls {
            if call_bytes.len() < 4 {
                return Err(BasePrecompileError::revert(IB20Asset::InternalCallMalformed {
                    call: Bytes::copy_from_slice(call_bytes),
                }));
            }
            if call_bytes[..4] == IB20Asset::announceCall::SELECTOR {
                return Err(BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
            }
            self.route_asset(ctx, call_bytes, version, privileged).map_err(|err| {
                if err.is_system_error() {
                    err
                } else {
                    BasePrecompileError::revert(IB20Asset::InternalCallFailed {
                        call: Bytes::copy_from_slice(call_bytes),
                    })
                }
            })?;
        }

        logic.end_announce(self, id)
    }
}

/// The announcement entity `run_announce` operates on. Both entry paths produce this shape:
/// the borrowed fast path validates aliased calldata without materializing the `bytes[]` blobs
/// (Cantina #16); the owned safety net feeds an already-decoded [`IB20Asset::announceCall`]
/// through the same runner. `internal_calls` holds slices into the source calldata, never owned
/// copies, so aliased offsets cost fat-pointers instead of blob copies.
struct DecodedAnnounce<'a> {
    id: String,
    description: String,
    uri: String,
    internal_calls: Vec<&'a [u8]>,
}

impl<'a> DecodedAnnounce<'a> {
    /// Tries to interpret `calldata` as an `announce` dialable at `version`.
    ///
    /// `None` skips the fast path entirely; `Some` decides the call, successfully or not.
    /// - `None`: the leading 4 bytes aren't the `announce` selector, `version` doesn't dial it, or
    ///   `decode_sequence` fails. All three are cheap; no token exists yet to re-encode. The
    ///   caller falls through to the generic decode, unchanged.
    /// - `Some(Ok(_))`: `decode_sequence` and `valid_token` both pass. This is exactly what
    ///   alloy's owned `abi_decode_validate` checks, minus the copying `detokenize` step, so the
    ///   accept-set matches the owned path. `valid_token` catches non-UTF-8 strings; skipping it
    ///   would accept an `id`/`description`/`uri` the owned path rejects, a divergence the
    ///   fallback couldn't catch. Checking `valid_token` directly, instead of `type_check`, also
    ///   skips building an `Err` that would re-ABI-encode the whole token, aliases included, and
    ///   then get discarded.
    /// - `Some(Err(_))`: `valid_token` rejects the payload. V1 (Beryl) returns `None` here
    ///   instead: a cheap rejection would change consensus-frozen revert bytes, so it falls
    ///   through to the same expensive owned diagnostic it always has. V2 (Cobalt, not live on
    ///   any network) returns the bounded rejection directly, skipping that diagnostic's
    ///   O(aliases · `tail_bytes`) cost (Cantina #16 follow-up).
    ///
    /// `valid_selector` future-proofs a fork that drops `announce`.
    fn decode_if_announce(
        calldata: &'a [u8],
        version: B20Version,
    ) -> Option<Result<Self, BasePrecompileError>> {
        let selector = calldata.first_chunk::<4>().copied()?;
        if selector != IB20Asset::announceCall::SELECTOR {
            return None;
        }
        if !AssetAbiPair::of(version).asset.valid_selector(selector) {
            return None;
        }
        let rest = &calldata[4..];
        let token =
            abi::decode_sequence::<<IB20Asset::announceCall as SolCall>::Token<'a>>(rest).ok()?;
        if !<<IB20Asset::announceCall as SolCall>::Parameters<'a> as SolType>::valid_token(&token) {
            // V1/Beryl revert bytes are consensus-frozen: keep falling through to the owned
            // decoder's diagnostic, however expensive. Later versions short-circuit cheaply.
            if version == B20Version::V1 {
                return None;
            }
            return Some(Err(BasePrecompileError::AbiDecodeFailed {
                selector,
                error: String::from("announce: malformed bytes[] payload"),
            }));
        }
        // Field `.0` of `PackedSeqToken<'a>` is `&'a [u8]`, so each iter yields a slice with the
        // full calldata lifetime. `as_slice()` would tie borrows to `token` (a local) instead.
        Some(Ok(Self {
            id: Self::string_from_utf8(token.1.0),
            description: Self::string_from_utf8(token.2.0),
            uri: Self::string_from_utf8(token.3.0),
            internal_calls: token.0.0.iter().map(|call| call.0).collect(),
        }))
    }

    /// Builds a `DecodedAnnounce` from an already owned-decoded call. `internal_calls` borrow from
    /// `c.internalCalls`; strings clone once. Only the safety-net arm reaches this: every accepted
    /// `announce` takes the borrowed fast path first.
    fn from_owned(c: &'a IB20Asset::announceCall) -> Self {
        Self {
            id: c.id.clone(),
            description: c.description.clone(),
            uri: c.uri.clone(),
            internal_calls: c.internalCalls.iter().map(|call| call.as_ref()).collect(),
        }
    }

    /// `type_check` in `try_from_calldata` already validated UTF-8 for every `string` token, so
    /// this conversion is total in practice. Panicking on the impossible case beats the silent
    /// U+FFFD substitution `from_utf8_lossy` would perform if that invariant ever broke; a silent
    /// divergence from the owned `detokenize` path would be a consensus fork.
    fn string_from_utf8(bytes: &[u8]) -> String {
        String::from(core::str::from_utf8(bytes).expect("type_check validated UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_tiger_precompile_storage::{HashMapStorageProvider, Result, StorageCtx};

    use crate::{
        B20Logic,
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        B20Version, B20AssetStorage, B20Token, B20TokenRole, FakePolicyAccounting,
        IB20, IB20Asset, InMemoryTokenAccounting, PolicyVersion, Token, TokenAccounting,
    };

    type TestAssetToken = B20Token<InMemoryTokenAccounting, FakePolicyAccounting>;

    /// Upgrade at which the asset precompile is active for every dispatch test.
    const UPGRADE: BaseUpgrade = BaseUpgrade::Beryl;

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
            token.route_asset(ctx, calldata.as_ref(), B20Version::V1, false)
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
            token.route_asset(ctx, calldata.as_ref(), B20Version::V2, false)
        })
    }

    fn batch_mint_calldata(recipients: Vec<Address>, amounts: Vec<U256>) -> Vec<u8> {
        IB20Asset::batchMintCall { recipients, amounts }.abi_encode()
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
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

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

        assert_eq!(err, base_tiger_precompile_storage::BasePrecompileError::under_overflow());
    }

    /// A non-system revert produced by an inner `announce` call must be wrapped as
    /// [`IB20Asset::InternalCallFailed`], preserving the original calldata in the error field.
    #[test]
    fn announce_inner_ordinary_revert_wraps_as_internal_call_failed() {
        let mut token = make_token();
        // ALICE has OPERATOR_ROLE (needed for announce) but not MINT_ROLE (needed for mint).
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

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
            base_tiger_precompile_storage::BasePrecompileError::revert(
                IB20Asset::InternalCallFailed { call: inner_call }
            )
        );
    }

    #[test]
    fn route_v2_schedules_and_flips_multiplier_lazily() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);
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
            base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector)
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
            base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector(selector)
        );
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut token = make_token();
        let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
        let mut storage = storage_with_caller(ALICE);
        storage.set_call_value(U256::from(1u64));

        let out = StorageCtx::enter(&mut storage, |ctx| token.dispatch_asset(ctx, &calldata, UPGRADE))
            .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
    }

    // --- Cantina #16: aliased-`bytes[]` announce borrowed decode -------------------------------

    /// Builds `announce` calldata where `n` `bytes[]` element offsets all alias one shared `tail`.
    ///
    /// The helper starts from a valid one-element alloy encoding, widens the array offset table so
    /// every element offset points at the same tail blob, and shifts the trailing string offsets to
    /// match. The resulting wire is smaller than an `n`-copy owned-style encoding. That size gap is
    /// the heap amplification a naive owned decode would materialize and the borrowed decode avoids.
    fn aliased_announce_calldata(n: usize, tail: &[u8]) -> Vec<u8> {
        assert!(n >= 1, "need at least one aliased entry");
        let base = IB20Asset::announceCall {
            internalCalls: alloc::vec![Bytes::copy_from_slice(tail)],
            id: String::from("aliased-id"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();

        let args = &base[4..];
        let read_off = |at: usize| -> usize {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&args[at + 24..at + 32]);
            u64::from_be_bytes(buf) as usize
        };
        let write_off = |out: &mut [u8], at: usize, v: usize| {
            out[at..at + 32].fill(0);
            out[at + 24..at + 32].copy_from_slice(&(v as u64).to_be_bytes());
        };

        let off_calls = read_off(0);
        let off_id = read_off(32);
        let off_desc = read_off(64);
        let off_uri = read_off(96);
        assert_eq!(read_off(off_calls), 1, "base encoding must be one element");

        // Base array section (n == 1): [len=1][off0][blob..]; the three strings begin at off_id.
        let blob = &args[off_calls + 64..off_id];
        let strings = &args[off_id..];

        let extra = (n - 1) * 32; // widening the offset table shifts everything after it
        let shared_elem_off = n * 32; // offset (from after the length word) of the shared blob

        let mut out = base[..4].to_vec();
        out.resize(4 + off_calls + 32 + n * 32 + blob.len() + strings.len(), 0);
        let a = &mut out[4..];
        write_off(a, 0, off_calls);
        write_off(a, 32, off_id + extra);
        write_off(a, 64, off_desc + extra);
        write_off(a, 96, off_uri + extra);
        write_off(a, off_calls, n); // array length
        for i in 0..n {
            write_off(a, off_calls + 32 + i * 32, shared_elem_off);
        }
        let blob_at = off_calls + 32 + n * 32;
        a[blob_at..blob_at + blob.len()].copy_from_slice(blob);
        let strings_at = off_id + extra;
        a[strings_at..strings_at + strings.len()].copy_from_slice(strings);
        out
    }

    /// A 1,024-way aliased-offset payload gets accepted, executes, and marks its id used. This is
    /// the front-door behavioral counterpart to the denial-of-service regression.
    #[test]
    fn aliased_announce_valid_marks_id_used() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

        let tail = IB20Asset::multiplierCall {}.abi_encode(); // zero-arg read: re-dispatches Ok
        let calldata = aliased_announce_calldata(1_024, &tail);

        call_asset(&mut token, ALICE, calldata).expect("aliased announce must succeed");
        assert!(token.accounting().is_announcement_id_used("aliased-id").unwrap());
    }

    /// Borrowed slices route to real state effects. `n` aliased entries produce `n × effect`.
    #[test]
    fn aliased_announce_redispatches_each_entry() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);
        token.accounting_mut().roles.insert((B20TokenRole::Mint.id(), ALICE), true);

        let n = 8usize;
        let tail = IB20::mintCall { to: BOB, amount: U256::ONE }.abi_encode();
        let calldata = aliased_announce_calldata(n, &tail);

        call_asset(&mut token, ALICE, calldata).expect("aliased mints must succeed");
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(n as u64));
    }

    /// An aliased entry whose bytes invoke `announce` trips the in-progress guard.
    #[test]
    fn aliased_announce_nested_announce_reverts() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

        let tail = IB20Asset::announceCall::SELECTOR.to_vec(); // first 4 bytes == announce selector
        let calldata = aliased_announce_calldata(2, &tail);

        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();
        assert_eq!(
            err,
            base_tiger_precompile_storage::BasePrecompileError::revert(
                IB20Asset::AnnouncementInProgress {}
            )
        );
    }

    /// Behavioral parity check against the ABI oracle. For every payload row, at both V1 and V2:
    ///
    /// * `route` accepts iff `announceCall::abi_decode_validate(payload).is_ok()`;
    /// * on decode-time rejection, the returned error equals `AssetAbiPair::of(version).decode(payload)
    ///   .unwrap_err()`, the same fall-through the fix guarantees.
    ///
    /// Any consensus-relevant drift (accept-set change, error-byte change, V1/V2 divergence) makes
    /// the offending row fail with its name in the assertion. The aliased-offsets row is the
    /// front-door counterpart to the denial-of-service regression. Rows carrying an inner call use a
    /// zero-arg view (`multiplier`) so ABI-accepted payloads succeed end-to-end.
    #[test]
    fn announce_dispatch_matches_owned_abi_oracle() {
        let inner = IB20Asset::multiplierCall {}.abi_encode(); // dispatches Ok on re-entry.
        let one_element = IB20Asset::announceCall {
            internalCalls: alloc::vec![Bytes::from(inner.clone())],
            id: String::from("id-token"),
            description: String::from("description-value"),
            uri: String::from("uri-value"),
        }
        .abi_encode();
        let multi_element = IB20Asset::announceCall {
            internalCalls: alloc::vec![
                Bytes::from(inner.clone()),
                Bytes::from(inner.clone()),
                Bytes::from(inner.clone()),
            ],
            id: String::from("multi-element"),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode();

        let aliased = aliased_announce_calldata(1_024, &inner);

        // The `bytes[]` length word sits right after the 4-word head. Setting it to all-`ff` makes
        // the implied offset table overrun the buffer.
        let mut past_end_length = aliased_announce_calldata(4, &inner);
        past_end_length[4 + 128..4 + 128 + 32].fill(0xff);

        // Distinct string content lets the closure pinpoint each string's byte range.
        let poison_at = |bytes: &[u8], needle: &[u8]| -> usize {
            bytes.windows(needle.len()).position(|w| w == needle).expect("needle present")
        };
        let mut non_utf8_id = one_element.clone();
        let at = poison_at(&non_utf8_id, b"id-token");
        non_utf8_id[at..at + 8].fill(0xff);

        let mut non_utf8_desc = one_element.clone();
        let at = poison_at(&non_utf8_desc, b"description-value");
        non_utf8_desc[at..at + 17].fill(0xff);

        let mut non_utf8_uri = one_element.clone();
        let at = poison_at(&non_utf8_uri, b"uri-value");
        non_utf8_uri[at..at + 9].fill(0xff);

        // Cantina #16 follow-up: a `type_check` rejection on an aliased payload must reject exactly
        // like a non-aliased one, not fall through with different error bytes.
        let mut non_utf8_aliased_id = aliased_announce_calldata(128, &inner);
        let at = poison_at(&non_utf8_aliased_id, b"aliased-id");
        non_utf8_aliased_id[at..at + 10].fill(0xff);

        let mut trailing_garbage = one_element.clone();
        trailing_garbage.extend_from_slice(&[0u8; 16]);

        let truncated_head = IB20Asset::announceCall::SELECTOR.to_vec();
        let no_calldata: Vec<u8> = Vec::new();

        // Each row: (name, payload, must_accept, rejects_via_valid_token). `rejects_via_valid_token`
        // marks the one input class where V2 legitimately diverges from the owned-decode oracle by
        // design: `valid_token` rejecting a structurally-valid-but-malformed token (non-UTF-8
        // strings), where V2/Cobalt short-circuits cheaply instead of falling through (Cantina #16
        // follow-up). Every other row must still match the oracle identically at both versions.
        let rows: alloc::vec::Vec<(&'static str, Vec<u8>, bool, bool)> = alloc::vec![
            ("honest single-element valid", one_element, true, false),
            ("honest multi-element valid", multi_element, true, false),
            ("aliased offsets valid", aliased, true, false),
            ("length word overruns buffer", past_end_length, false, false),
            ("non-UTF-8 in id", non_utf8_id, false, true),
            ("non-UTF-8 in description", non_utf8_desc, false, true),
            ("non-UTF-8 in uri", non_utf8_uri, false, true),
            ("aliased offsets with non-UTF-8 id", non_utf8_aliased_id, false, true),
            // alloy follows absolute offsets, so bytes past the last tail get ignored. The oracle
            // accepts, and the fast path must match. This row pins that parity.
            ("trailing garbage after valid payload", trailing_garbage, true, false),
            ("truncated head (only selector)", truncated_head, false, false),
            ("no calldata at all", no_calldata, false, false),
        ];

        for (name, calldata, must_accept, rejects_via_valid_token) in rows {
            // Oracle: alloy's owned validator is the ABI spec, independent of the fix. It reads
            // full calldata (selector included) since `abi_decode_validate` peels the selector.
            let oracle_accepts = IB20Asset::announceCall::abi_decode_validate(&calldata).is_ok();
            assert_eq!(
                oracle_accepts, must_accept,
                "row `{name}`: oracle disagrees; refresh the test if the payload changed"
            );

            for version in [B20Version::V1, B20Version::V2, B20Version::V3] {
                let mut token = make_token();
                token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);
                let mut storage = storage_with_caller(ALICE);
                let outcome = StorageCtx::enter(&mut storage, |ctx| {
                    token.route_asset(ctx, &calldata, version, false)
                });

                assert_eq!(
                    outcome.is_ok(),
                    must_accept,
                    "row `{name}` at {version:?}: accept-set disagrees with the oracle",
                );

                if let Err(err) = outcome {
                    let control = crate::AssetAbiPair::of(version).decode(&calldata).unwrap_err();
                    if rejects_via_valid_token
                        && matches!(version, B20Version::V2 | B20Version::V3)
                    {
                        assert_ne!(
                            err, control,
                            "row `{name}` at {version:?}: expected the bounded V2 short-circuit \
                             to fire, but the error still matches the owned-decode oracle",
                        );
                        assert_eq!(
                            err,
                            base_tiger_precompile_storage::BasePrecompileError::AbiDecodeFailed {
                                selector: IB20Asset::announceCall::SELECTOR,
                                error: String::from("announce: malformed bytes[] payload"),
                            },
                            "row `{name}` at {version:?}: V2 must reject with the bounded, \
                             calldata-size-independent error",
                        );
                    } else {
                        assert_eq!(
                            err, control,
                            "row `{name}` at {version:?}: error bytes must match owned decode",
                        );
                    }
                    // Decode-time rejections target the announce selector. Payloads too short to
                    // carry a selector hit the shared unknown-selector path instead.
                    match err {
                        base_tiger_precompile_storage::BasePrecompileError::AbiDecodeFailed {
                            selector,
                            ..
                        } => assert_eq!(selector, IB20Asset::announceCall::SELECTOR),
                        base_tiger_precompile_storage::BasePrecompileError::UnknownFunctionSelector(
                            _,
                        ) => {}
                        other => panic!("row `{name}` at {version:?}: unexpected error {other:?}"),
                    }
                }
            }
        }
    }

    /// Empty `internalCalls` runs begin/end with no loop iterations.
    #[test]
    fn empty_internal_calls_announce_succeeds() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

        let calldata = IB20Asset::announceCall {
            internalCalls: Vec::new(),
            id: String::from("empty-calls"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();

        call_asset(&mut token, ALICE, calldata).expect("empty announce must succeed");
        assert!(token.accounting().is_announcement_id_used("empty-calls").unwrap());
    }

    /// A large non-aliased array succeeds. Peak allocation stays O(calldata), not O(n × tail).
    #[test]
    fn large_non_aliased_announce_succeeds() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

        let entry = Bytes::from(IB20Asset::multiplierCall {}.abi_encode());
        let calldata = IB20Asset::announceCall {
            internalCalls: alloc::vec![entry; 64],
            id: String::from("many-distinct"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();

        call_asset(&mut token, ALICE, calldata).expect("large announce must succeed");
        assert!(token.accounting().is_announcement_id_used("many-distinct").unwrap());
    }

    /// An internal call shorter than a selector reverts and preserves the offending bytes.
    #[test]
    fn short_internal_call_reverts_as_malformed() {
        let mut token = make_token();
        token.accounting_mut().roles.insert((B20Logic::OPERATOR_ROLE, ALICE), true);

        let calldata = IB20Asset::announceCall {
            internalCalls: alloc::vec![Bytes::from_static(&[0x01, 0x02])],
            id: String::from("short-call"),
            description: String::from("desc"),
            uri: String::new(),
        }
        .abi_encode();

        let err = call_asset(&mut token, ALICE, calldata).unwrap_err();
        assert_eq!(
            err,
            base_tiger_precompile_storage::BasePrecompileError::revert(
                IB20Asset::InternalCallMalformed { call: Bytes::copy_from_slice(&[0x01, 0x02]) }
            )
        );
    }
}
