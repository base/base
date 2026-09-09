//! Golden tests pinning Asset **V1** behavior of the B-20 precompile.
//!
//! These are authored and pinned against the shipped **v1.1.1** (pre-versioned) asset
//! implementation; the conversion to the versioned precompile structure is behavior-preserving
//! and continues to satisfy every pin below unchanged.
//!
//! Every op (mutations, computed reads, direct/const reads) is driven through the
//! **version-resolver-gated** dispatch path (`BaseUpgrade::Beryl` -> `AssetVersion::V1`) against
//! the real EVM-backed `B20AssetStorage` over `HashMapStorageProvider`, with an `FakePolicyAccounting`
//! for deterministic allow/block decisions. Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting state (balances / supply / roles / allowances / multiplier / metadata / storage),
//!   3. emitted events, and
//!   4. a per-case keccak storage **hash** snapshot (the frozen-manifest baseline).
//!
//! Because the per-op suite resolves the version via `AssetVersions::from_base_upgrade`, it breaks
//! if dispatch ever routes to the wrong version. Privileged behavior is exercised via
//! `route` with `privileged = true`; the guard envelope (nonpayable / uninitialized / pre-Beryl)
//! via the full `dispatch_with_observer`.
//!
//! ## Blessing storage hashes
//! State-root constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_asset_v1_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolInterface, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    Asset, AssetAccounting, AssetV1, AssetVersion, AssetVersions, B20_MAX_SUPPLY_CAP, B20AssetInit,
    B20AssetStorage, B20AssetToken, B20PolicyType, B20TokenRole, FakePolicyAccounting, IB20,
    IB20Asset, NoopPrecompileCallObserver, PolicyVersion, TokenAccounting,
    UpgradeGatedStorageFeatures,
};
use base_precompile_storage::{BasePrecompileError, Handler, HashMapStorageProvider, StorageCtx};

mod common;
use common::{
    ADMIN, ALICE, BOB, CAROL, CHAIN_ID, MEMO, POLICY_ID, TOKEN, anvil_owner, bless_or_assert_gas,
    bless_or_assert_root, hash_token_state, ok_true, signed_permit, u,
};

// --- fixtures ---------------------------------------------------------------

const NAME: &str = "Base Asset";
const SYMBOL: &str = "bASSET";
const DECIMALS: u8 = 6;
const LOGIC: AssetV1 = AssetV1;

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------

const ROOT_FRESH: B256 = b256!("e29cabf2f5d0e0eedebf4697b61ad93a4a24fa2d911d4004d641bb0b123fa091");
const ROOT_TRANSFER_PRIV: B256 =
    b256!("8a8c95a5a46fd1968f924479897846bdd2a1cc88c8365741c782c47b0d3ffa81");
const ROOT_TRANSFER_UNPRIV: B256 =
    b256!("63c542ccf49f3be8d67a0e5fc4d1d0f6126e81157d1b1b53b969154ddf9a4f8b");
const ROOT_TRANSFER_WITH_MEMO: B256 =
    b256!("1373d29ddaf8cf795c185c179ccd72a3be6a518eaf6e909bc660f0d9c430faa2");
const ROOT_TRANSFER_FROM_FINITE: B256 =
    b256!("826132aed4b216cb6157dfbe2eee15c5529b40c640532dc1edd39023d7bfe43e");
const ROOT_TRANSFER_FROM_INFINITE: B256 =
    b256!("c7c98558f1eefd845c4c8ae6994cfe27921f7e1f75b32a06b1042aea23eedbed");
const ROOT_TRANSFER_FROM_WITH_MEMO: B256 =
    b256!("d8a423aeeb838ccf840c3304426a44affa08f7e4c73dab6b6d0355ea9a93cb55");
const ROOT_APPROVE: B256 =
    b256!("eb54df6e31bdfe6ff3b9c898e776c7a9aa6249604c421c247014bfe38c5320d5");
const ROOT_MINT_PRIV: B256 =
    b256!("9028416ca6edfb4e75add6f4f9a93b5bf19fe6a04d5b57c5a49ded3009cf3f39");
const ROOT_MINT_UNPRIV: B256 =
    b256!("3374c9af2e12ba5ec092f1c4f1f52142110af23ac420f0eb5153bc18b2788666");
const ROOT_MINT_WITH_MEMO: B256 =
    b256!("7d1fce256019cc70fd7f68fb4f6070aca3fb313f4a31ed99aa42acacc65affe8");
const ROOT_BURN: B256 = b256!("ade155c4138f7b71f96ace7ac3a5804c10e94c421d79331c790867ba79bf66c1");
const ROOT_BURN_WITH_MEMO: B256 =
    b256!("f2dc0e262f62cde4e299e8d2bba339f9cd2d28cc25607a327553d324db6e7596");
const ROOT_BURN_BLOCKED: B256 =
    b256!("5249765fc1a60e0cca911b0dfca10bcc7c4afd23e15b1b7dbc3d4e91bb9e4603");
const ROOT_PAUSE: B256 = b256!("1c2af409faaa966d7e866226209ad6e34784c8f6b34771a5857f3592cdd269fa");
const ROOT_UNPAUSE: B256 =
    b256!("6487edf62f45d0209b1d93e668a6d7b7879133062927908c2fd2addd0d7c649f");
const ROOT_UPDATE_SUPPLY_CAP: B256 =
    b256!("b7da26ed4139cc7f730792550ee8ce46e0c1d9d7fe480e580d6772ea5000d09b");
const ROOT_UPDATE_NAME: B256 =
    b256!("1d4993119f19447ea2738eafadf0dd6fd4d9843ebff11af78f6140c60522f3cd");
const ROOT_UPDATE_SYMBOL: B256 =
    b256!("5948dd0afd2cfe783d54fa684a0e79f74afb28ebf3a699868f0e43a607039b9d");
const ROOT_UPDATE_CONTRACT_URI: B256 =
    b256!("b9110c98a36b52516f37b6e7b2f1ca7bf45a7a69d882bd73fbd12307d0c24aa7");
const ROOT_GRANT_ROLE: B256 =
    b256!("db7327c728187786e000a33766fc33d14f6e990b40b467388e46bf92fe1270f4");
const ROOT_REVOKE_ROLE: B256 =
    b256!("bae23d9d6012567505cb15687219c903a7f6264221ae1ce651b157f796116791");
const ROOT_RENOUNCE_ROLE: B256 =
    b256!("d51de5372be373c54b010cf92edd87a100b94142641b251e1cc0d3a38d35a50a");
const ROOT_RENOUNCE_LAST_ADMIN: B256 =
    b256!("4e3aaeac81242cb1de6434972071d768a40283dc2b428b915125db89b5a35d5d");
const ROOT_SET_ROLE_ADMIN: B256 =
    b256!("8b9c7bb4b91bea8469409833f010f0e7d599bec804863b28f8d7c643d2e8f9ee");
const ROOT_UPDATE_POLICY: B256 =
    b256!("17c384f3c7009cde9c64520ca48e735f0e74ff44a1c20f5f835252ed5316d633");
const ROOT_PERMIT: B256 = b256!("073a87de0e60125aeb9df668b6d9462d33559bd8696360dc0fc87ad062545803");
const ROOT_GRANT_DEFAULT_ADMIN: B256 =
    b256!("e47421a3157f752161dd7daaa3a042639cb27e92a7d6045a0a541f014feb03ec");
const ROOT_GRANT_IDEMPOTENT: B256 =
    b256!("05e26defbbd563cd07319fe3155331476630d5ea43ff8da11ac2283c01253cb8");
const ROOT_GRANT_UNCHECKED: B256 =
    b256!("3a3145f737b772e3f4cee5cf2c36531117a1e7b9891775d09371c777ed3922a9");

// asset-specific (blessed against v1.1.1)
const ROOT_MULTIPLIER_SCALED: B256 =
    b256!("686201a1ea687d57677c2b0bbf82da29be8e43b7028935651294f77cbd4e32e2");
const ROOT_ANNOUNCE_ID_USED: B256 =
    b256!("37d104dac2c6bafec2e0eb2ad0c48e463dfae4d239102367249be33c55446cdb");
const ROOT_EXTRA_METADATA_READ: B256 =
    b256!("a7235b360a5c2cf33b1706c9c37c25aff8cf029b6072335922e9bda7616c632f");
const ROOT_UPDATE_MULTIPLIER: B256 =
    b256!("797f188e28956f02934ecb539834a0a7577f20d46ba2e62cc3287cac6959b468");
const ROOT_BATCH_MINT: B256 =
    b256!("f3c6787115aee5f808fae895bc4a6f1eb895a476e273de0e4e2e35de70bc84e8");
const ROOT_METADATA_SET: B256 =
    b256!("eb23e81c02299fc34cf9471b38d9f606e59fcf6dd6d0aff446b9f831e90eba8a");
const ROOT_METADATA_REMOVE: B256 =
    b256!("28c8bef2b1b23198ab03bef1cae027031f9e993be2630f16218f0cf8a73422eb");
const ROOT_ANNOUNCE: B256 =
    b256!("bc541cb4aecf8c93e8bc0b495eb7d1011c614fcb65704b848a961bb7207dcb83");

// --- harness ----------------------------------------------------------------

/// Fresh provider with an initialized `Base Asset` at [`TOKEN`], matching the factory
/// bootstrap: the multiplier slot is left physically zero and the getter normalizes it to WAD.
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Beryl),
    );
    StorageCtx::enter(&mut storage, |ctx| {
        let mut token = B20AssetStorage::from_address(TOKEN, ctx);
        token
            .initialize(B20AssetInit {
                name: NAME.into(),
                symbol: SYMBOL.into(),
                supply_cap: B20_MAX_SUPPLY_CAP,
                multiplier: U256::ZERO, // factory INITIAL_MULTIPLIER: physical zero, getter returns WAD
                decimals: DECIMALS,
            })
            .expect("initialize asset");
    });
    storage
}

/// Mutates raw token storage through the accounting port (test setup only).
fn seed(storage: &mut HashMapStorageProvider, f: impl FnOnce(&mut B20AssetStorage<'_>)) {
    StorageCtx::enter(storage, |ctx| {
        let mut token = B20AssetStorage::from_address(TOKEN, ctx);
        f(&mut token);
    });
}

/// Reads token state through the accounting port.
fn read<R>(storage: &mut HashMapStorageProvider, f: impl FnOnce(&B20AssetStorage<'_>) -> R) -> R {
    StorageCtx::enter(storage, |ctx| f(&B20AssetStorage::from_address(TOKEN, ctx)))
}

/// Drives one op through the resolver-gated (`Beryl` -> V1) unprivileged path.
fn op(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        let version =
            AssetVersions::from_base_upgrade(BaseUpgrade::Beryl).expect("Beryl activates V1");
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V1,
        )
        .route(ctx, &calldata, version, false, NoopPrecompileCallObserver)
    })
}

/// Drives one op through V1 with factory-init privilege (guards skipped).
fn op_privileged(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V1,
        )
        .route(ctx, &calldata, AssetVersion::V1, true, NoopPrecompileCallObserver)
    })
}

/// Topic-0 (signature hash) of the last event emitted by the token.
fn last_topic0(storage: &HashMapStorageProvider) -> B256 {
    storage.get_events(TOKEN).last().expect("an emitted event").topics()[0]
}

/// Asserts the token's storage hash, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
fn assert_root(label: &str, storage: HashMapStorageProvider, expected: B256) {
    bless_or_assert_root(label, hash_token_state(storage, TOKEN), expected);
}

/// Grants `role` to `who` and bumps the role member count (setup only).
fn give_role(token: &mut B20AssetStorage<'_>, role: B256, who: Address) {
    token.set_role(role, who, true).unwrap();
    let next = token.role_member_count(role).unwrap() + U256::ONE;
    token.set_role_member_count(role, next).unwrap();
}

/// Credits `who` with `amount` and grows total supply to match (setup only).
fn fund(token: &mut B20AssetStorage<'_>, who: Address, amount: U256) {
    let balance = token.balance_of(who).unwrap();
    token.set_balance(who, balance + amount).unwrap();
    let supply = token.total_supply().unwrap();
    token.set_total_supply(supply + amount).unwrap();
}

/// The asset operator role id: `keccak256("OPERATOR_ROLE")` (V1 pins this equality).
fn operator_role() -> B256 {
    keccak256("OPERATOR_ROLE")
}

/// The V1 EIP-712 domain separator for the token at [`TOKEN`] on [`CHAIN_ID`].
fn domain_separator(storage: &mut HashMapStorageProvider) -> B256 {
    StorageCtx::enter(storage, |ctx| {
        let token = B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        );
        LOGIC.domain_separator(&token, CHAIN_ID).unwrap()
    })
}

// ============================================================================
// Version resolver
// ============================================================================

#[test]
fn resolver_maps_forks_to_versions() {
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(AssetVersion::V1));
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(AssetVersion::V2));
}

/// The ERC-8056 scheduled-multiplier selectors were introduced at Cobalt (`AssetV2`). At V1 (Beryl)
/// they are absent from the frozen asset wire surface, so `route` falls through to the disjoint
/// inherited `IB20` decode and rejects them as `UnknownFunctionSelector`, byte-identically to the
/// deleted hand-written fork gate.
#[test]
fn golden_v2_selectors_unknown_at_v1() {
    let mut s = fresh();
    let calls: Vec<Vec<u8>> = vec![
        IB20Asset::uiMultiplierCall {}.abi_encode(),
        IB20Asset::newUIMultiplierCall {}.abi_encode(),
        IB20Asset::effectiveAtCall {}.abi_encode(),
        IB20Asset::balanceOfUICall { account: ALICE }.abi_encode(),
        IB20Asset::totalSupplyUICall {}.abi_encode(),
        IB20Asset::updateUIMultiplierCall { newMultiplier: u(2), effectiveAt: u(1) }.abi_encode(),
        IB20Asset::cancelUIMultiplierUpdateCall {}.abi_encode(),
        IB20Asset::toUIAmountCall { rawAmount: u(100) }.abi_encode(),
        IB20Asset::fromUIAmountCall { uiAmount: u(200) }.abi_encode(),
        IB20Asset::MAX_UI_MULTIPLIERCall {}.abi_encode(),
        IB20Asset::supportsInterfaceCall {
            interfaceId: alloy_primitives::FixedBytes::new([0x01, 0xff, 0xc9, 0xa7]),
        }
        .abi_encode(),
    ];
    for calldata in calls {
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        let err = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap_err();
        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector(selector));
    }
}

/// The seize common selectors (`seizeWithMemo` and the `SEIZE_ROLE` / `SEIZE_EXEMPT_POLICY` /
/// `SEIZE_RECEIVER_POLICY` getters) were introduced at Cobalt (`AssetV2`). At V1 (Beryl) they are
/// absent from the frozen common `IB20` surface, so `route` rejects them as `UnknownFunctionSelector`.
#[test]
fn golden_seize_selectors_unknown_at_v1() {
    let mut s = fresh();
    let calls: Vec<Vec<u8>> = vec![
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
        IB20::SEIZE_ROLECall {}.abi_encode(),
        IB20::SEIZE_EXEMPT_POLICYCall {}.abi_encode(),
        IB20::SEIZE_RECEIVER_POLICYCall {}.abi_encode(),
    ];
    for calldata in calls {
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        let err = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap_err();
        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector(selector));
    }
}

/// The seize policy scopes were introduced at Cobalt (`AssetV2`). Although the `SEIZE_EXEMPT_POLICY()`
/// / `SEIZE_RECEIVER_POLICY()` getter selectors are absent from V1, the scope *values* must also not
/// leak through the common `updatePolicy` selector, which is dialable on V1: V1 rejects them with
/// `UnsupportedPolicyType`, matching the base-std `v1.0.0` reference.
#[test]
fn golden_update_policy_rejects_seize_scopes_at_v1() {
    for scope in [B20PolicyType::SeizeExempt.id(), B20PolicyType::SeizeReceiver.id()] {
        let mut s = fresh();
        let mut policy = FakePolicyAccounting::new();
        policy.create_existing_policy(7);
        let err = op_privileged(
            &mut s,
            ADMIN,
            policy,
            IB20::updatePolicyCall { policyScope: scope, newPolicyId: 7 }.abi_encode(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: scope })
        );
    }
}

/// Companion to [`golden_update_policy_rejects_seize_scopes_at_v1`] on the read path: the common
/// `policyId` selector is dialable on V1 but must reject the V2-only seize scopes.
#[test]
fn golden_policy_id_rejects_seize_scopes_at_v1() {
    for scope in [B20PolicyType::SeizeExempt.id(), B20PolicyType::SeizeReceiver.id()] {
        let mut s = fresh();
        let err = op(
            &mut s,
            ALICE,
            FakePolicyAccounting::new(),
            IB20::policyIdCall { policyScope: scope }.abi_encode(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: scope })
        );
    }
}

// ============================================================================
// transfer
// ============================================================================

#[test]
fn golden_transfer_privileged() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(100)));

    let out = op_privileged(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(30) }.abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(70));
        assert_eq!(t.balance_of(BOB).unwrap(), u(30));
    });
    assert_eq!(last_topic0(&s), IB20::Transfer::SIGNATURE_HASH);
    assert_root("transfer_privileged", s, ROOT_TRANSFER_PRIV);
}

#[test]
fn golden_transfer_unprivileged_allowed() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
        t.set_policy_id(B20PolicyType::TransferReceiver.id(), POLICY_ID).unwrap();
    });
    // Authorize sender + receiver under the configured policy => guards pass.
    let mut policy = FakePolicyAccounting::new();
    policy.allow(POLICY_ID, ALICE);
    policy.allow(POLICY_ID, BOB);
    let out = op(&mut s, ALICE, policy, IB20::transferCall { to: BOB, amount: u(10) }.abi_encode())
        .unwrap();

    assert_eq!(out, ok_true());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(90));
        assert_eq!(t.balance_of(BOB).unwrap(), u(10));
    });
    assert_root("transfer_unprivileged", s, ROOT_TRANSFER_UNPRIV);
}

#[test]
fn golden_transfer_unprivileged_blocked_sender_reverts() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        // Configure a real sender policy that authorizes nobody => sender blocked.
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
    });
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(10) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::PolicyForbids {
            policyScope: B20PolicyType::TransferSender.id(),
            policyId: POLICY_ID,
        })
    );
}

#[test]
fn golden_transfer_reverts_zero_receiver() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(10)));
    let err = op_privileged(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: Address::ZERO, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }));
}

#[test]
fn golden_transfer_reverts_insufficient_balance() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(10)));
    let err = op_privileged(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(50) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::InsufficientBalance {
            sender: ALICE,
            balance: u(10),
            needed: u(50),
        })
    );
}

#[test]
fn golden_transfer_reverts_when_paused() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(10)));
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] }.abi_encode(),
    )
    .unwrap();
    let err = op_privileged(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::ContractPaused {
            feature: IB20::PausableFeature::TRANSFER
        })
    );
}

#[test]
fn golden_transfer_with_memo_emits_transfer_then_memo() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(100)));
    let out = op_privileged(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferWithMemoCall { to: BOB, amount: u(30), memo: MEMO }.abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::Transfer::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20::Memo::SIGNATURE_HASH);
    assert_root("transfer_with_memo", s, ROOT_TRANSFER_WITH_MEMO);
}

// ============================================================================
// transferFrom
// ============================================================================

#[test]
fn golden_transfer_from_finite_allowance_decrements() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_allowance(ALICE, BOB, u(40)).unwrap();
    });
    let out = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: ALICE, to: BOB, amount: u(30) }.abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    read(&mut s, |t| {
        assert_eq!(t.allowance(ALICE, BOB).unwrap(), u(10));
        assert_eq!(t.balance_of(BOB).unwrap(), u(30));
    });
    assert_root("transfer_from_finite", s, ROOT_TRANSFER_FROM_FINITE);
}

#[test]
fn golden_transfer_from_infinite_allowance_not_decremented() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_allowance(ALICE, BOB, U256::MAX).unwrap();
    });
    let out = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: ALICE, to: BOB, amount: u(30) }.abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    read(&mut s, |t| assert_eq!(t.allowance(ALICE, BOB).unwrap(), U256::MAX));
    assert_root("transfer_from_infinite", s, ROOT_TRANSFER_FROM_INFINITE);
}

#[test]
fn golden_transfer_from_reverts_insufficient_allowance() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_allowance(ALICE, BOB, u(5)).unwrap();
    });
    let err = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: ALICE, to: BOB, amount: u(30) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::InsufficientAllowance {
            spender: BOB,
            allowance: u(5),
            needed: u(30),
        })
    );
}

#[test]
fn golden_transfer_from_unprivileged_enforces_executor_policy() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_allowance(ALICE, BOB, u(40)).unwrap();
    });
    seed(&mut s, |t| {
        t.set_policy_id(B20PolicyType::TransferExecutor.id(), POLICY_ID).unwrap();
    });
    // BOB (executor, != from) is not authorized under the executor policy => forbidden.
    let err = op(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: ALICE, to: CAROL, amount: u(10) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::PolicyForbids {
            policyScope: B20PolicyType::TransferExecutor.id(),
            policyId: POLICY_ID,
        })
    );
}

#[test]
fn golden_transfer_from_with_memo() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_allowance(ALICE, BOB, u(40)).unwrap();
    });
    let out = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromWithMemoCall { from: ALICE, to: CAROL, amount: u(30), memo: MEMO }
            .abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::Transfer::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20::Memo::SIGNATURE_HASH);
    assert_root("transfer_from_with_memo", s, ROOT_TRANSFER_FROM_WITH_MEMO);
}

// ============================================================================
// approve
// ============================================================================

#[test]
fn golden_approve_sets_allowance_and_emits() {
    let mut s = fresh();
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::approveCall { spender: BOB, amount: u(50) }.abi_encode(),
    )
    .unwrap();

    assert_eq!(out, ok_true());
    read(&mut s, |t| assert_eq!(t.allowance(ALICE, BOB).unwrap(), u(50)));
    assert_eq!(last_topic0(&s), IB20::Approval::SIGNATURE_HASH);
    assert_root("approve", s, ROOT_APPROVE);
}

#[test]
fn golden_approve_reverts_zero_spender() {
    let mut s = fresh();
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::approveCall { spender: Address::ZERO, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidSpender { spender: Address::ZERO }));
}

// ============================================================================
// mint
// ============================================================================

#[test]
fn golden_mint_privileged_still_enforces_receiver_policy() {
    let mut s = fresh();
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB); // MintReceiver enforced even when privileged
    let out = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::mintCall { to: BOB, amount: u(100) }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(BOB).unwrap(), u(100));
        assert_eq!(t.total_supply().unwrap(), u(100));
    });
    assert_eq!(last_topic0(&s), IB20::Transfer::SIGNATURE_HASH);
    assert_root("mint_privileged", s, ROOT_MINT_PRIV);
}

#[test]
fn golden_mint_unprivileged_requires_role_and_policy() {
    let mut s = fresh();
    // Missing MINT_ROLE => unauthorized.
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
    let err = op(&mut s, ALICE, policy, IB20::mintCall { to: BOB, amount: u(1) }.abi_encode())
        .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::Mint.id(),
        })
    );

    // With MINT_ROLE + authorized receiver => succeeds.
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
    let out =
        op(&mut s, ALICE, policy, IB20::mintCall { to: BOB, amount: u(75) }.abi_encode()).unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.balance_of(BOB).unwrap(), u(75)));
    assert_root("mint_unprivileged", s, ROOT_MINT_UNPRIV);
}

#[test]
fn golden_mint_reverts_over_supply_cap() {
    let mut s = fresh();
    seed(&mut s, |t| t.set_supply_cap(u(50)).unwrap());
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
    let err = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::mintCall { to: BOB, amount: u(100) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::SupplyCapExceeded { cap: u(50), attempted: u(100) })
    );
}

#[test]
fn golden_mint_with_memo() {
    let mut s = fresh();
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
    let out = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::mintWithMemoCall { to: BOB, amount: u(40), memo: MEMO }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::Transfer::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20::Memo::SIGNATURE_HASH);
    assert_root("mint_with_memo", s, ROOT_MINT_WITH_MEMO);
}

// ============================================================================
// burn / burnBlocked
// ============================================================================

#[test]
fn golden_burn_requires_role_then_reduces_supply() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(100)));

    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnCall { amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::Burn.id(),
        })
    );

    seed(&mut s, |t| give_role(t, B20TokenRole::Burn.id(), ALICE));
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnCall { amount: u(40) }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(60));
        assert_eq!(t.total_supply().unwrap(), u(60));
    });
    assert_eq!(last_topic0(&s), IB20::Transfer::SIGNATURE_HASH);
    assert_root("burn", s, ROOT_BURN);
}

#[test]
fn golden_burn_with_memo() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Burn.id(), ALICE);
    });
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnWithMemoCall { amount: u(40), memo: MEMO }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::Transfer::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20::Memo::SIGNATURE_HASH);
    assert_root("burn_with_memo", s, ROOT_BURN_WITH_MEMO);
}

#[test]
fn golden_burn_blocked_destroys_from_blocked_account() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        // Configure a real transfer-sender policy that does not authorize ALICE => blocked.
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
    });
    // ALICE blocked; privileged skips the role check.
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::burnBlockedCall { from: ALICE, amount: u(40) }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.balance_of(ALICE).unwrap(), u(60)));
    assert_eq!(last_topic0(&s), IB20::BurnedBlocked::SIGNATURE_HASH);
    assert_root("burn_blocked", s, ROOT_BURN_BLOCKED);
}

#[test]
fn golden_burn_blocked_reverts_when_not_blocked() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
    });
    let mut policy = FakePolicyAccounting::new();
    policy.allow(POLICY_ID, ALICE); // authorized => not blocked
    let err = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::burnBlockedCall { from: ALICE, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotBlocked { account: ALICE }));
}

// ============================================================================
// pause / unpause
// ============================================================================

#[test]
fn golden_pause_sets_feature_bit() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    assert_eq!(last_topic0(&s), IB20::Paused::SIGNATURE_HASH);
    assert_root("pause", s, ROOT_PAUSE);
}

#[test]
fn golden_unpause_clears_feature_bit() {
    let mut s = fresh();
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::unpauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    assert_eq!(last_topic0(&s), IB20::Unpaused::SIGNATURE_HASH);
    assert_root("unpause", s, ROOT_UNPAUSE);
}

/// The `SEIZE` pause feature was introduced at Cobalt (`AssetV2`) as enum member 3. On the frozen V1
/// (Beryl) common wire the `PausableFeature` enum has only three members, so `pause`/`unpause`/
/// `isPaused` carrying `SEIZE` fail to decode (`AbiDecodeFailed`) before any shared validation runs.
/// Unlike the `bytes32` policy scope, the enum argument is range-checked by the wire decode itself.
#[test]
fn golden_seize_pause_feature_unknown_at_v1() {
    let calls: Vec<Vec<u8>> = vec![
        IB20::pauseCall { features: vec![IB20::PausableFeature::SEIZE] }.abi_encode(),
        IB20::unpauseCall { features: vec![IB20::PausableFeature::SEIZE] }.abi_encode(),
        IB20::isPausedCall { feature: IB20::PausableFeature::SEIZE }.abi_encode(),
    ];
    for calldata in calls {
        let mut s = fresh();
        let selector: [u8; 4] = calldata[..4].try_into().unwrap();
        let err = op_privileged(&mut s, ADMIN, FakePolicyAccounting::new(), calldata).unwrap_err();
        assert!(
            matches!(err, BasePrecompileError::AbiDecodeFailed { selector: s, .. } if s == selector),
            "expected AbiDecodeFailed for selector {selector:?}, got {err:?}"
        );
    }
}

#[test]
fn golden_pause_reverts_empty_feature_set() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
}

#[test]
fn golden_pause_unprivileged_requires_role() {
    let mut s = fresh();
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::Pause.id(),
        })
    );
}

// ============================================================================
// config / metadata
// ============================================================================

#[test]
fn golden_update_supply_cap() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updateSupplyCapCall { newSupplyCap: u(1_000) }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.supply_cap().unwrap(), u(1_000)));
    assert_eq!(last_topic0(&s), IB20::SupplyCapUpdated::SIGNATURE_HASH);
    assert_root("update_supply_cap", s, ROOT_UPDATE_SUPPLY_CAP);
}

#[test]
fn golden_update_supply_cap_reverts_below_supply() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(500)));
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updateSupplyCapCall { newSupplyCap: u(100) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::InvalidSupplyCap {
            currentSupply: u(500),
            proposedCap: u(100),
        })
    );
}

#[test]
fn golden_update_name_emits_name_and_domain_changed() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updateNameCall { newName: "New Name".into() }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.name().unwrap(), "New Name"));
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::NameUpdated::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20::EIP712DomainChanged::SIGNATURE_HASH);
    assert_root("update_name", s, ROOT_UPDATE_NAME);
}

#[test]
fn golden_update_symbol() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updateSymbolCall { newSymbol: "USDX".into() }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.symbol().unwrap(), "USDX"));
    assert_eq!(last_topic0(&s), IB20::SymbolUpdated::SIGNATURE_HASH);
    assert_root("update_symbol", s, ROOT_UPDATE_SYMBOL);
}

#[test]
fn golden_update_contract_uri() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updateContractURICall { newURI: "ipfs://x".into() }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.contract_uri().unwrap(), "ipfs://x"));
    assert_eq!(last_topic0(&s), IB20::ContractURIUpdated::SIGNATURE_HASH);
    assert_root("update_contract_uri", s, ROOT_UPDATE_CONTRACT_URI);
}

// ============================================================================
// roles
// ============================================================================

#[test]
fn golden_grant_role() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert!(t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_eq!(last_topic0(&s), IB20::RoleGranted::SIGNATURE_HASH);
    assert_root("grant_role", s, ROOT_GRANT_ROLE);
}

#[test]
fn golden_revoke_role() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::revokeRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert!(!t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_eq!(last_topic0(&s), IB20::RoleRevoked::SIGNATURE_HASH);
    assert_root("revoke_role", s, ROOT_REVOKE_ROLE);
}

#[test]
fn golden_revoke_last_admin_rejected() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::revokeRoleCall { role: B20TokenRole::DefaultAdmin.id(), account: ADMIN }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
}

#[test]
fn golden_renounce_role() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::renounceRoleCall { role: B20TokenRole::Mint.id(), callerConfirmation: ALICE }
            .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert!(!t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_eq!(last_topic0(&s), IB20::RoleRevoked::SIGNATURE_HASH);
    assert_root("renounce_role", s, ROOT_RENOUNCE_ROLE);
}

#[test]
fn golden_renounce_role_bad_confirmation() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::renounceRoleCall { role: B20TokenRole::Mint.id(), callerConfirmation: BOB }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::AccessControlBadConfirmation {}));
}

#[test]
fn golden_renounce_last_admin() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let out =
        op(&mut s, ADMIN, FakePolicyAccounting::new(), IB20::renounceLastAdminCall {}.abi_encode())
            .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert!(!t.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
        assert_eq!(t.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(), U256::ZERO);
    });
    assert_eq!(last_topic0(&s), IB20::LastAdminRenounced::SIGNATURE_HASH);
    assert_root("renounce_last_admin", s, ROOT_RENOUNCE_LAST_ADMIN);
}

#[test]
fn golden_renounce_last_admin_reverts_when_not_sole() {
    let mut s = fresh();
    seed(&mut s, |t| {
        give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN);
        give_role(t, B20TokenRole::DefaultAdmin.id(), BOB);
    });
    let err =
        op(&mut s, ADMIN, FakePolicyAccounting::new(), IB20::renounceLastAdminCall {}.abi_encode())
            .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::NotSoleAdmin {}));
}

#[test]
fn golden_set_role_admin() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::setRoleAdminCall {
            role: B20TokenRole::Mint.id(),
            newAdminRole: B20TokenRole::Metadata.id(),
        }
        .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.role_admin(B20TokenRole::Mint.id()).unwrap(), B20TokenRole::Metadata.id())
    });
    assert_eq!(last_topic0(&s), IB20::RoleAdminChanged::SIGNATURE_HASH);
    assert_root("set_role_admin", s, ROOT_SET_ROLE_ADMIN);
}

// ============================================================================
// policy
// ============================================================================

#[test]
fn golden_update_policy() {
    let mut s = fresh();
    let mut policy = FakePolicyAccounting::new();
    policy.create_existing_policy(7);
    let out = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::updatePolicyCall { policyScope: B20PolicyType::TransferSender.id(), newPolicyId: 7 }
            .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.policy_id(B20PolicyType::TransferSender.id()).unwrap(), 7));
    assert_eq!(last_topic0(&s), IB20::PolicyUpdated::SIGNATURE_HASH);
    assert_root("update_policy", s, ROOT_UPDATE_POLICY);
}

#[test]
fn golden_update_policy_reverts_missing_policy() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::updatePolicyCall { policyScope: B20PolicyType::TransferSender.id(), newPolicyId: 99 }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::PolicyNotFound { policyId: 99 }));
}

// ============================================================================
// permit
// ============================================================================

#[test]
fn golden_permit_sets_allowance_and_increments_nonce() {
    let mut s = fresh();
    let owner = anvil_owner();
    let domain = domain_separator(&mut s);
    let call = signed_permit(domain, U256::ZERO, owner, BOB, u(500), U256::MAX);
    s.set_timestamp(U256::ZERO);
    let out = op(&mut s, owner, FakePolicyAccounting::new(), call.abi_encode()).unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.allowance(owner, BOB).unwrap(), u(500));
        assert_eq!(t.nonce(owner).unwrap(), U256::ONE);
    });
    assert_eq!(last_topic0(&s), IB20::Approval::SIGNATURE_HASH);
    assert_root("permit", s, ROOT_PERMIT);
}

#[test]
fn golden_permit_reverts_when_expired() {
    let mut s = fresh();
    let owner = anvil_owner();
    let domain = domain_separator(&mut s);
    let call = signed_permit(domain, U256::ZERO, owner, BOB, u(1), u(10));
    s.set_timestamp(u(11));
    let err = op(&mut s, owner, FakePolicyAccounting::new(), call.abi_encode()).unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::ExpiredSignature { deadline: u(10) }));
}

/// V1 `permit` is deliberately UNMETERED (Beryl's gas schedule is frozen): it hashes with plain
/// `keccak256` and recovers without a `deduct_gas` charge, so `gas_deducted()` stays `0`. The V2
/// golden pins this at `3000`; that metered/unmetered contrast is the point of this pair.
#[test]
fn golden_permit_charges_no_recovery_gas() {
    let mut s = fresh();
    let owner = anvil_owner();
    let calldata =
        signed_permit(domain_separator(&mut fresh()), U256::ZERO, owner, BOB, u(500), U256::MAX)
            .abi_encode();
    s.set_caller(owner);
    s.set_timestamp(U256::ZERO);
    StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .route(ctx, &calldata, AssetVersion::V1, true, NoopPrecompileCallObserver)
    })
    .expect("permit must succeed");
    assert_eq!(s.gas_deducted(), 0, "V1 permit must not charge any recovery gas (unmetered)");
}

// ============================================================================
// computed reads
// ============================================================================

#[test]
fn golden_read_is_paused_and_paused_features() {
    let mut s = fresh();
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();

    let paused_mint = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::isPausedCall { feature: IB20::PausableFeature::MINT }.abi_encode(),
    )
    .unwrap();
    assert_eq!(paused_mint, Bytes::from(true.abi_encode()));

    let paused_transfer = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::isPausedCall { feature: IB20::PausableFeature::TRANSFER }.abi_encode(),
    )
    .unwrap();
    assert_eq!(paused_transfer, Bytes::from(false.abi_encode()));

    let features =
        op(&mut s, ALICE, FakePolicyAccounting::new(), IB20::pausedFeaturesCall {}.abi_encode())
            .unwrap();
    assert_eq!(features, Bytes::from(vec![IB20::PausableFeature::MINT].abi_encode()));
}

#[test]
fn golden_read_policy_id_and_unsupported_scope() {
    let mut s = fresh();
    let ok = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::policyIdCall { policyScope: B20PolicyType::TransferSender.id() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(ok, Bytes::from(0u64.abi_encode()));

    let bad_scope = B256::repeat_byte(0xEE);
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::policyIdCall { policyScope: bad_scope }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: bad_scope })
    );
}

#[test]
fn golden_read_domain_separator() {
    let mut s = fresh();
    let expected = domain_separator(&mut s);
    let out =
        op(&mut s, ALICE, FakePolicyAccounting::new(), IB20::DOMAIN_SEPARATORCall {}.abi_encode())
            .unwrap();
    assert_eq!(out, Bytes::from(expected.abi_encode()));
    assert_root("read_domain_separator", s, ROOT_FRESH);
}

#[test]
fn golden_read_eip712_domain() {
    let mut s = fresh();
    let out =
        op(&mut s, ALICE, FakePolicyAccounting::new(), IB20::eip712DomainCall {}.abi_encode())
            .unwrap();
    let decoded = IB20::eip712DomainCall::abi_decode_returns(&out).unwrap();
    assert_eq!(decoded.name, NAME);
    assert_eq!(decoded.version, "1");
    assert_eq!(decoded.chainId, U256::from(CHAIN_ID));
    assert_eq!(decoded.verifyingContract, TOKEN);
    assert_eq!(decoded.fields, alloy_primitives::FixedBytes::<1>::from([0x0f]));
}

// ============================================================================
// direct + constant reads
// ============================================================================

#[test]
fn golden_read_metadata_and_supply() {
    let mut s = fresh();
    let cases: Vec<(Vec<u8>, Bytes)> = vec![
        (IB20::nameCall {}.abi_encode(), Bytes::from(NAME.abi_encode())),
        (IB20::symbolCall {}.abi_encode(), Bytes::from(SYMBOL.abi_encode())),
        (IB20::decimalsCall {}.abi_encode(), Bytes::from(u(6).abi_encode())),
        (IB20::totalSupplyCall {}.abi_encode(), Bytes::from(U256::ZERO.abi_encode())),
        (IB20::supplyCapCall {}.abi_encode(), Bytes::from(B20_MAX_SUPPLY_CAP.abi_encode())),
        (IB20::contractURICall {}.abi_encode(), Bytes::from(String::new().abi_encode())),
        (IB20::balanceOfCall { account: ALICE }.abi_encode(), Bytes::from(U256::ZERO.abi_encode())),
        (
            IB20::allowanceCall { owner: ALICE, spender: BOB }.abi_encode(),
            Bytes::from(U256::ZERO.abi_encode()),
        ),
        (IB20::noncesCall { owner: ALICE }.abi_encode(), Bytes::from(U256::ZERO.abi_encode())),
        (
            IB20::hasRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
            Bytes::from(false.abi_encode()),
        ),
        (
            IB20::getRoleAdminCall { role: B20TokenRole::Mint.id() }.abi_encode(),
            Bytes::from(B256::ZERO.abi_encode()),
        ),
    ];
    for (calldata, expected) in cases {
        let out = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap();
        assert_eq!(out, expected);
    }
    assert_root("read_metadata", s, ROOT_FRESH);
}

#[test]
fn golden_read_role_and_policy_constants() {
    let mut s = fresh();
    let cases: Vec<(Vec<u8>, B256)> = vec![
        (IB20::DEFAULT_ADMIN_ROLECall {}.abi_encode(), B20TokenRole::DefaultAdmin.id()),
        (IB20::MINT_ROLECall {}.abi_encode(), B20TokenRole::Mint.id()),
        (IB20::BURN_ROLECall {}.abi_encode(), B20TokenRole::Burn.id()),
        (IB20::BURN_BLOCKED_ROLECall {}.abi_encode(), B20TokenRole::BurnBlocked.id()),
        (IB20::PAUSE_ROLECall {}.abi_encode(), B20TokenRole::Pause.id()),
        (IB20::UNPAUSE_ROLECall {}.abi_encode(), B20TokenRole::Unpause.id()),
        (IB20::METADATA_ROLECall {}.abi_encode(), B20TokenRole::Metadata.id()),
        (IB20::TRANSFER_SENDER_POLICYCall {}.abi_encode(), B20PolicyType::TransferSender.id()),
        (IB20::TRANSFER_RECEIVER_POLICYCall {}.abi_encode(), B20PolicyType::TransferReceiver.id()),
        (IB20::TRANSFER_EXECUTOR_POLICYCall {}.abi_encode(), B20PolicyType::TransferExecutor.id()),
        (IB20::MINT_RECEIVER_POLICYCall {}.abi_encode(), B20PolicyType::MintReceiver.id()),
    ];
    for (calldata, expected) in cases {
        let out = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap();
        assert_eq!(out, Bytes::from(expected.abi_encode()));
    }
    assert_root("read_constants", s, ROOT_FRESH);
}

// ============================================================================
// dispatch envelope (full path: nonpayable / uninitialized / pre-Beryl)
// ============================================================================

#[test]
fn dispatch_rejects_nonzero_value() {
    let mut s = fresh();
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    s.set_call_value(U256::ONE);
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .dispatch_with_observer(
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

#[test]
fn dispatch_reverts_before_beryl() {
    let mut s = fresh();
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .dispatch_with_observer(
            ctx,
            &calldata,
            BaseUpgrade::Azul,
            NoopPrecompileCallObserver,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
    assert!(out.bytes.is_empty());
}

#[test]
fn dispatch_reverts_when_uninitialized() {
    // No `fresh()` init and no marker bytecode => is_initialized is false.
    let mut s = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Beryl),
    );
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .dispatch_with_observer(
            ctx,
            &calldata,
            BaseUpgrade::Beryl,
            NoopPrecompileCallObserver,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
    assert!(out.bytes.is_empty());
}

// ============================================================================
// additional branch coverage: unprivileged auth guards + revert edges
// ============================================================================

#[test]
fn golden_transfer_reverts_zero_sender() {
    let mut s = fresh();
    // caller (the sender) is the zero address.
    let err = op_privileged(
        &mut s,
        Address::ZERO,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidSender { sender: Address::ZERO }));
}

#[test]
fn golden_transfer_from_reverts_zero_receiver() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: ALICE, to: Address::ZERO, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }));
}

#[test]
fn golden_transfer_from_reverts_zero_sender() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::transferFromCall { from: Address::ZERO, to: CAROL, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidSender { sender: Address::ZERO }));
}

#[test]
fn golden_approve_reverts_zero_approver() {
    let mut s = fresh();
    // caller (the approver) is the zero address.
    let err = op(
        &mut s,
        Address::ZERO,
        FakePolicyAccounting::new(),
        IB20::approveCall { spender: BOB, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidApprover { approver: Address::ZERO }));
}

#[test]
fn golden_mint_reverts_zero_receiver() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::mintCall { to: Address::ZERO, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }));
}

#[test]
fn golden_burn_reverts_insufficient_balance() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(10));
        give_role(t, B20TokenRole::Burn.id(), ALICE);
    });
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnCall { amount: u(50) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::InsufficientBalance {
            sender: ALICE,
            balance: u(10),
            needed: u(50),
        })
    );
}

#[test]
fn golden_burn_blocked_unprivileged_requires_role() {
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(100)));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnBlockedCall { from: BOB, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::BurnBlocked.id(),
        })
    );
}

#[test]
fn golden_unpause_reverts_empty_feature_set() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::unpauseCall { features: vec![] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::EmptyFeatureSet {}));
}

#[test]
fn golden_unpause_unprivileged_requires_role() {
    let mut s = fresh();
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::unpauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::Unpause.id(),
        })
    );
}

/// Asserts an unprivileged metadata/admin op reverts for a caller lacking `role`.
#[track_caller]
fn assert_unprivileged_requires_role(calldata: Vec<u8>, role: B256) {
    let mut s = fresh();
    let err = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: role,
        })
    );
}

#[test]
fn golden_update_supply_cap_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updateSupplyCapCall { newSupplyCap: u(1) }.abi_encode(),
        B20TokenRole::DefaultAdmin.id(),
    );
}

#[test]
fn golden_update_name_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updateNameCall { newName: "x".into() }.abi_encode(),
        B20TokenRole::Metadata.id(),
    );
}

#[test]
fn golden_update_symbol_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updateSymbolCall { newSymbol: "x".into() }.abi_encode(),
        B20TokenRole::Metadata.id(),
    );
}

#[test]
fn golden_update_contract_uri_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updateContractURICall { newURI: "x".into() }.abi_encode(),
        B20TokenRole::Metadata.id(),
    );
}

#[test]
fn golden_update_policy_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updatePolicyCall { policyScope: B20PolicyType::TransferSender.id(), newPolicyId: 1 }
            .abi_encode(),
        B20TokenRole::DefaultAdmin.id(),
    );
}

#[test]
fn golden_grant_role_unprivileged_no_admin_reverts() {
    // No admin exists yet → the admin-availability guard reverts.
    let mut s = fresh();
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: BOB }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::DefaultAdmin.id(),
        })
    );
}

#[test]
fn golden_grant_role_unprivileged_non_admin_caller_reverts() {
    // An admin exists, but ALICE is not the role's admin → the role check reverts.
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: BOB }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::DefaultAdmin.id(),
        })
    );
}

#[test]
fn golden_revoke_role_unprivileged_non_admin_caller_reverts() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let err = op(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::revokeRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: BOB,
            neededRole: B20TokenRole::DefaultAdmin.id(),
        })
    );
}

#[test]
fn golden_set_role_admin_unprivileged_non_admin_caller_reverts() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let err = op(
        &mut s,
        BOB,
        FakePolicyAccounting::new(),
        IB20::setRoleAdminCall {
            role: B20TokenRole::Mint.id(),
            newAdminRole: B20TokenRole::Metadata.id(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: BOB,
            neededRole: B20TokenRole::DefaultAdmin.id(),
        })
    );
}

#[test]
fn golden_renounce_role_reverts_last_admin() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let err = op(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::renounceRoleCall { role: B20TokenRole::DefaultAdmin.id(), callerConfirmation: ADMIN }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::LastAdminCannotRenounce {}));
}

#[test]
fn golden_grant_default_admin_bumps_member_count() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN));
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::DefaultAdmin.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert!(t.has_role(B20TokenRole::DefaultAdmin.id(), ALICE).unwrap());
        assert_eq!(t.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(), u(2));
    });
    assert_eq!(last_topic0(&s), IB20::RoleGranted::SIGNATURE_HASH);
    assert_root("grant_default_admin", s, ROOT_GRANT_DEFAULT_ADMIN);
}

#[test]
fn golden_grant_role_idempotent_when_already_held() {
    let mut s = fresh();
    seed(&mut s, |t| {
        give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN);
        give_role(t, B20TokenRole::Mint.id(), ALICE);
    });
    // ALICE already holds MINT_ROLE → grant is a no-op (no event, count unchanged).
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    // ALICE still holds MINT_ROLE; the grant emitted nothing (early return).
    read(&mut s, |t| assert!(t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_root("grant_idempotent", s, ROOT_GRANT_IDEMPOTENT);
}

#[test]
fn golden_revoke_role_noop_when_not_held() {
    let mut s = fresh();
    // ALICE does not hold MINT_ROLE → revoke is a no-op; state stays at fresh.
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::revokeRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert!(!t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_root("revoke_noop", s, ROOT_FRESH);
}

// ============================================================================
// dispatch harness wrappers (no-observer dispatch, version-resolver gating, factory bootstrap)
// ============================================================================

#[test]
fn golden_dispatch_no_observer_wrapper_reverts_uninitialized() {
    // Exercises the no-observer `dispatch()` wrapper + the is_initialized=false gate.
    let mut s = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Beryl),
    );
    s.set_caller(ALICE);
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .dispatch(ctx, &calldata, BaseUpgrade::Beryl)
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
}

#[test]
fn golden_inner_reverts_before_beryl() {
    // Exercises the version-resolution None branch (pre-introduction fork): before Beryl no
    // version is active, so the resolver-gated path reverts without routing.
    let mut s = fresh();
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let err = StorageCtx::enter(&mut s, |ctx| {
        let mut token = B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        );
        AssetVersions::from_base_upgrade(BaseUpgrade::Azul).map_or_else(
            || Err(BasePrecompileError::Revert(Bytes::new())),
            |version| token.route(ctx, &calldata, version, false, NoopPrecompileCallObserver),
        )
    })
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::Revert(Bytes::new()));
}

#[test]
fn golden_grant_role_unchecked_bootstraps_first_admin() {
    // The factory bootstrap path: grants DEFAULT_ADMIN with no caller-auth check.
    let mut s = fresh();
    s.set_caller(TOKEN);
    StorageCtx::enter(&mut s, |ctx| {
        let mut token = B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        );
        token
            .grant_role_unchecked(B20TokenRole::DefaultAdmin.id(), ADMIN, TOKEN, BaseUpgrade::Beryl)
            .unwrap();
    });
    read(&mut s, |t| {
        assert!(t.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
        assert_eq!(t.role_member_count(B20TokenRole::DefaultAdmin.id()).unwrap(), U256::ONE);
    });
    assert_eq!(last_topic0(&s), IB20::RoleGranted::SIGNATURE_HASH);
    assert_root("grant_unchecked", s, ROOT_GRANT_UNCHECKED);
}

// ============================================================================
// asset reads: OPERATOR_ROLE / WAD / multiplier / scaled balances / metadata
// ============================================================================

#[test]
fn golden_read_operator_role_and_wad() {
    let mut s = fresh();
    let role = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::OPERATOR_ROLECall {}.abi_encode(),
    )
    .unwrap();
    assert_eq!(role, Bytes::from(operator_role().abi_encode()));
    let wad = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::WAD_PRECISIONCall {}.abi_encode(),
    )
    .unwrap();
    assert_eq!(wad, Bytes::from(B20AssetStorage::WAD.abi_encode()));
    assert_root("read_operator_wad", s, ROOT_FRESH);
}

#[test]
fn golden_read_multiplier_and_scaled_balances() {
    let mut s = fresh();
    // A doubled multiplier exercises the `* multiplier / WAD` and `* WAD / multiplier` paths.
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        t.set_multiplier(B20AssetStorage::WAD * u(2)).unwrap();
    });
    let m =
        op(&mut s, ALICE, FakePolicyAccounting::new(), IB20Asset::multiplierCall {}.abi_encode())
            .unwrap();
    assert_eq!(m, Bytes::from((B20AssetStorage::WAD * u(2)).abi_encode()));

    let scaled = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::toScaledBalanceCall { rawBalance: u(100) }.abi_encode(),
    )
    .unwrap();
    assert_eq!(scaled, Bytes::from(u(200).abi_encode()));

    let raw = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::toRawBalanceCall { scaledBalance: u(200) }.abi_encode(),
    )
    .unwrap();
    assert_eq!(raw, Bytes::from(u(100).abi_encode()));

    let sbo = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::scaledBalanceOfCall { account: ALICE }.abi_encode(),
    )
    .unwrap();
    assert_eq!(sbo, Bytes::from(u(200).abi_encode()));

    assert_root("read_multiplier_scaled", s, ROOT_MULTIPLIER_SCALED);
}

#[test]
fn golden_read_is_announcement_id_used() {
    let mut s = fresh();
    let unused = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::isAnnouncementIdUsedCall { id: "evt-1".into() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(unused, Bytes::from(false.abi_encode()));

    seed(&mut s, |t| t.mark_announcement_id_used("evt-1").unwrap());
    let used = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::isAnnouncementIdUsedCall { id: "evt-1".into() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(used, Bytes::from(true.abi_encode()));
    assert_root("read_announcement_id_used", s, ROOT_ANNOUNCE_ID_USED);
}

#[test]
fn golden_read_extra_metadata() {
    let mut s = fresh();
    let empty = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::extraMetadataCall { key: "category".into() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(empty, Bytes::from(String::new().abi_encode()));

    seed(&mut s, |t| t.set_extra_metadata_value("category", "commodity".into()).unwrap());
    let set = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::extraMetadataCall { key: "category".into() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(set, Bytes::from("commodity".abi_encode()));
    assert_root("read_extra_metadata", s, ROOT_EXTRA_METADATA_READ);
}

// ============================================================================
// updateMultiplier
// ============================================================================

// Guards `fresh()` against drifting back to a physical-WAD prestate: a factory-bootstrapped
// token leaves the multiplier slot physically zero, and the getter normalizes it to WAD.
#[test]
fn golden_fresh_multiplier_slot_is_physical_zero() {
    let mut s = fresh();
    read(&mut s, |t| {
        assert_eq!(t.asset.multiplier.read().unwrap(), U256::ZERO); // raw slot
        assert_eq!(t.multiplier().unwrap(), B20AssetStorage::WAD); // normalized getter
    });
}

#[test]
fn golden_update_multiplier() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::updateMultiplierCall { newMultiplier: B20AssetStorage::WAD * u(2) }.abi_encode(),
    )
    .unwrap();
    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.multiplier().unwrap(), B20AssetStorage::WAD * u(2)));
    assert_eq!(
        *s.get_events(TOKEN).last().unwrap(),
        IB20Asset::MultiplierUpdated { multiplier: B20AssetStorage::WAD * u(2) }.encode_log_data()
    );
    assert_root("update_multiplier", s, ROOT_UPDATE_MULTIPLIER);
}

#[test]
fn golden_update_multiplier_reverts_zero() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::updateMultiplierCall { newMultiplier: U256::ZERO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
}

#[test]
fn golden_update_multiplier_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20Asset::updateMultiplierCall { newMultiplier: B20AssetStorage::WAD }.abi_encode(),
        operator_role(),
    );
}

// ============================================================================
// batchMint
// ============================================================================

#[test]
fn golden_batch_mint() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
    policy.allow(0, CAROL);
    let out = op(
        &mut s,
        ALICE,
        policy,
        IB20Asset::batchMintCall { recipients: vec![BOB, CAROL], amounts: vec![u(30), u(70)] }
            .abi_encode(),
    )
    .unwrap();
    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(BOB).unwrap(), u(30));
        assert_eq!(t.balance_of(CAROL).unwrap(), u(70));
        assert_eq!(t.total_supply().unwrap(), u(100));
    });
    assert_root("batch_mint", s, ROOT_BATCH_MINT);
}

#[test]
fn golden_batch_mint_reverts_length_mismatch() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::batchMintCall { recipients: vec![BOB, CAROL], amounts: vec![u(30)] }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20Asset::LengthMismatch { leftLen: u(2), rightLen: u(1) })
    );
}

#[test]
fn golden_batch_mint_reverts_empty() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::batchMintCall { recipients: vec![], amounts: vec![] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::EmptyBatch {}));
}

#[test]
fn golden_batch_mint_reverts_when_paused() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();
    let err = op(
        &mut s,
        ALICE,
        allow0(BOB),
        IB20Asset::batchMintCall { recipients: vec![BOB], amounts: vec![u(1)] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::ContractPaused { feature: IB20::PausableFeature::MINT })
    );
}

#[test]
fn golden_batch_mint_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20Asset::batchMintCall { recipients: vec![BOB], amounts: vec![u(1)] }.abi_encode(),
        B20TokenRole::Mint.id(),
    );
}

// ============================================================================
// updateExtraMetadata
// ============================================================================

#[test]
fn golden_update_extra_metadata_set() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Metadata.id(), ALICE));
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::updateExtraMetadataCall { key: "category".into(), value: "commodity".into() }
            .abi_encode(),
    )
    .unwrap();
    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.extra_metadata("category").unwrap(), "commodity"));
    assert_eq!(
        *s.get_events(TOKEN).last().unwrap(),
        IB20Asset::ExtraMetadataUpdated { key: "category".into(), value: "commodity".into() }
            .encode_log_data()
    );
    assert_root("update_extra_metadata_set", s, ROOT_METADATA_SET);
}

#[test]
fn golden_update_extra_metadata_remove() {
    let mut s = fresh();
    seed(&mut s, |t| {
        give_role(t, B20TokenRole::Metadata.id(), ALICE);
        t.set_extra_metadata_value("category", "commodity".into()).unwrap();
    });
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::updateExtraMetadataCall { key: "category".into(), value: String::new() }
            .abi_encode(),
    )
    .unwrap();
    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.extra_metadata("category").unwrap(), ""));
    assert_eq!(
        *s.get_events(TOKEN).last().unwrap(),
        IB20Asset::ExtraMetadataUpdated { key: "category".into(), value: String::new() }
            .encode_log_data()
    );
    assert_root("update_extra_metadata_remove", s, ROOT_METADATA_REMOVE);
}

#[test]
fn golden_update_extra_metadata_reverts_empty_key() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Metadata.id(), ALICE));
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::updateExtraMetadataCall { key: String::new(), value: "x".into() }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InvalidMetadataKey {}));
}

#[test]
fn golden_update_extra_metadata_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20Asset::updateExtraMetadataCall { key: "category".into(), value: "x".into() }
            .abi_encode(),
        B20TokenRole::Metadata.id(),
    );
}

// ============================================================================
// announce (posts announcement, atomically runs internalCalls)
// ============================================================================

#[test]
fn golden_announce_emits_and_runs_internal_calls() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    // An internal `updateMultiplier` runs under the operator role ALICE already holds.
    let inner =
        IB20Asset::updateMultiplierCall { newMultiplier: B20AssetStorage::WAD * u(2) }.abi_encode();
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::announceCall {
            internalCalls: vec![Bytes::from(inner)],
            id: "2026-split".into(),
            description: "2:1 split".into(),
            uri: "ipfs://split".into(),
        }
        .abi_encode(),
    )
    .unwrap();
    assert!(out.is_empty());
    read(&mut s, |t| {
        assert!(t.is_announcement_id_used("2026-split").unwrap());
        assert_eq!(t.multiplier().unwrap(), B20AssetStorage::WAD * u(2));
    });
    let events = s.get_events(TOKEN);
    // Announcement, MultiplierUpdated (internal call), EndAnnouncement.
    assert_eq!(
        events[events.len() - 3],
        IB20Asset::Announcement {
            caller: ALICE,
            id: "2026-split".into(),
            description: "2:1 split".into(),
            uri: "ipfs://split".into(),
        }
        .encode_log_data()
    );
    assert_eq!(events[events.len() - 2].topics()[0], IB20Asset::MultiplierUpdated::SIGNATURE_HASH);
    assert_eq!(
        *events.last().unwrap(),
        IB20Asset::EndAnnouncement { id: "2026-split".into() }.encode_log_data()
    );
    assert_root("announce", s, ROOT_ANNOUNCE);
}

#[test]
fn golden_announce_reverts_id_already_used() {
    let mut s = fresh();
    seed(&mut s, |t| {
        give_role(t, operator_role(), ALICE);
        t.mark_announcement_id_used("dup").unwrap();
    });
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::announceCall {
            internalCalls: vec![],
            id: "dup".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20Asset::AnnouncementIdAlreadyUsed { id: "dup".into() })
    );
}

/// Cantina #16 follow-up: a malformed `announce` (invalid UTF-8 in `id`) must keep falling
/// through to the owned decoder's diagnostic at V1, unchanged — V1's revert bytes are
/// consensus-frozen and must never take the new V2-only bounded-rejection path.
#[test]
fn golden_announce_malformed_id_stays_on_owned_diagnostic_at_v1() {
    let mut s = fresh();
    let marker = "malformed-id-marker";
    let mut calldata = IB20Asset::announceCall {
        internalCalls: vec![],
        id: marker.into(),
        description: String::new(),
        uri: String::new(),
    }
    .abi_encode();
    let at = calldata.windows(marker.len()).position(|w| w == marker.as_bytes()).unwrap();
    calldata[at..at + marker.len()].fill(0xff);

    let err = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata.clone()).unwrap_err();
    let oracle_error = IB20Asset::IB20AssetCalls::abi_decode_validate(&calldata).unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::AbiDecodeFailed {
            selector: IB20Asset::announceCall::SELECTOR,
            error: oracle_error.to_string(),
        },
        "V1 must keep falling through to the owned decoder's diagnostic, unchanged",
    );
}

#[test]
fn golden_announce_reverts_internal_call_malformed() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let malformed = Bytes::from(vec![0x01u8, 0x02]);
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::announceCall {
            internalCalls: vec![malformed.clone()],
            id: "x".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20Asset::InternalCallMalformed { call: malformed })
    );
}

#[test]
fn golden_announce_reverts_nested_announce() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let nested = Bytes::from(
        IB20Asset::announceCall {
            internalCalls: vec![],
            id: "inner".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    );
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::announceCall {
            internalCalls: vec![nested],
            id: "outer".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::AnnouncementInProgress {}));
}

#[test]
fn golden_announce_reverts_internal_call_failed() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    // A valid selector whose business logic reverts (zero multiplier) => wrapped.
    let inner =
        Bytes::from(IB20Asset::updateMultiplierCall { newMultiplier: U256::ZERO }.abi_encode());
    let err = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Asset::announceCall {
            internalCalls: vec![inner.clone()],
            id: "x".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InternalCallFailed { call: inner }));
}

#[test]
fn golden_announce_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20Asset::announceCall {
            internalCalls: vec![],
            id: "x".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
        operator_role(),
    );
}

// ============================================================================
// gas: storage-access footprint per op
// ============================================================================
//
// `gas_deducted` is 0 under the test gas schedule, so we pin the deterministic,
// schedule-independent signal instead: the SLOAD / SSTORE / KECCAK256 op counts a
// call performs. These are the storage-access footprint that drives real gas, so a
// change here (e.g. an extra SLOAD in V1) is caught even when bytes/state/events match.

/// Runs `calldata` privileged after `setup`, returning `(sload, sstore, keccak256)` counts.
fn gas(
    setup: impl FnOnce(&mut B20AssetStorage<'_>),
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> (u64, u64, u64) {
    let mut s = fresh();
    seed(&mut s, setup);
    s.set_caller(caller);
    s.reset_counters();
    StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V1,
        )
        .route(ctx, &calldata, AssetVersion::V1, true, NoopPrecompileCallObserver)
    })
    .expect("gas-footprint op must succeed");
    (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
}

/// An `FakePolicyAccounting` authorizing `who` under the default (0) scope.
fn allow0(who: Address) -> FakePolicyAccounting {
    let mut p = FakePolicyAccounting::new();
    p.allow(0, who);
    p
}

#[test]
fn golden_gas_footprints() {
    let actual: Vec<(&str, (u64, u64, u64))> = vec![
        (
            "transfer",
            gas(
                |t| fund(t, ALICE, u(100)),
                ALICE,
                FakePolicyAccounting::new(),
                IB20::transferCall { to: BOB, amount: u(30) }.abi_encode(),
            ),
        ),
        (
            "transfer_from",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    t.set_allowance(ALICE, BOB, u(40)).unwrap();
                },
                BOB,
                FakePolicyAccounting::new(),
                IB20::transferFromCall { from: ALICE, to: BOB, amount: u(30) }.abi_encode(),
            ),
        ),
        (
            "approve",
            gas(
                |_t| {},
                ALICE,
                FakePolicyAccounting::new(),
                IB20::approveCall { spender: BOB, amount: u(50) }.abi_encode(),
            ),
        ),
        (
            "mint",
            gas(
                |_t| {},
                ADMIN,
                allow0(BOB),
                IB20::mintCall { to: BOB, amount: u(100) }.abi_encode(),
            ),
        ),
        (
            "burn",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    give_role(t, B20TokenRole::Burn.id(), ALICE);
                },
                ALICE,
                FakePolicyAccounting::new(),
                IB20::burnCall { amount: u(40) }.abi_encode(),
            ),
        ),
        (
            "burn_blocked",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
                },
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::burnBlockedCall { from: ALICE, amount: u(40) }.abi_encode(),
            ),
        ),
        (
            "pause",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
            ),
        ),
        (
            "unpause",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::unpauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
            ),
        ),
        (
            "update_supply_cap",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::updateSupplyCapCall { newSupplyCap: u(1_000) }.abi_encode(),
            ),
        ),
        (
            "update_name",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::updateNameCall { newName: "New Name".into() }.abi_encode(),
            ),
        ),
        (
            "update_symbol",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::updateSymbolCall { newSymbol: "USDX".into() }.abi_encode(),
            ),
        ),
        (
            "update_contract_uri",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::updateContractURICall { newURI: "ipfs://x".into() }.abi_encode(),
            ),
        ),
        (
            "grant_role",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
            ),
        ),
        (
            "revoke_role",
            gas(
                |t| give_role(t, B20TokenRole::Mint.id(), ALICE),
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::revokeRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
            ),
        ),
        (
            "set_role_admin",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::setRoleAdminCall {
                    role: B20TokenRole::Mint.id(),
                    newAdminRole: B20TokenRole::Metadata.id(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_policy",
            gas(
                |_t| {},
                ADMIN,
                {
                    let mut p = FakePolicyAccounting::new();
                    p.create_existing_policy(7);
                    p
                },
                IB20::updatePolicyCall {
                    policyScope: B20PolicyType::TransferSender.id(),
                    newPolicyId: 7,
                }
                .abi_encode(),
            ),
        ),
        (
            "update_multiplier",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20Asset::updateMultiplierCall { newMultiplier: B20AssetStorage::WAD * u(2) }
                    .abi_encode(),
            ),
        ),
        (
            "batch_mint",
            gas(
                |_t| {},
                ADMIN,
                {
                    let mut p = FakePolicyAccounting::new();
                    p.allow(0, BOB);
                    p.allow(0, CAROL);
                    p
                },
                IB20Asset::batchMintCall {
                    recipients: vec![BOB, CAROL],
                    amounts: vec![u(30), u(70)],
                }
                .abi_encode(),
            ),
        ),
        (
            "announce",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20Asset::announceCall {
                    internalCalls: vec![],
                    id: "gas".into(),
                    description: String::new(),
                    uri: String::new(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_extra_metadata",
            gas(
                |_t| {},
                ADMIN,
                FakePolicyAccounting::new(),
                IB20Asset::updateExtraMetadataCall {
                    key: "category".into(),
                    value: "commodity".into(),
                }
                .abi_encode(),
            ),
        ),
        (
            "permit",
            gas(
                |_t| {},
                anvil_owner(),
                FakePolicyAccounting::new(),
                signed_permit(
                    domain_separator(&mut fresh()),
                    U256::ZERO,
                    anvil_owner(),
                    BOB,
                    u(500),
                    U256::MAX,
                )
                .abi_encode(),
            ),
        ),
    ];

    let expected: &[(&str, (u64, u64, u64))] = &[
        ("transfer", (3, 2, 0)),
        ("transfer_from", (4, 3, 0)),
        ("approve", (0, 1, 0)),
        ("mint", (5, 2, 0)),
        ("burn", (4, 2, 0)),
        ("burn_blocked", (4, 2, 0)),
        ("pause", (1, 1, 0)),
        ("unpause", (1, 1, 0)),
        ("update_supply_cap", (2, 1, 0)),
        ("update_name", (0, 1, 0)),
        ("update_symbol", (0, 1, 0)),
        ("update_contract_uri", (0, 1, 0)),
        ("grant_role", (1, 1, 0)),
        ("revoke_role", (1, 1, 0)),
        ("set_role_admin", (1, 1, 0)),
        ("update_policy", (2, 1, 0)),
        ("update_multiplier", (0, 1, 0)),
        ("batch_mint", (11, 4, 0)),
        ("announce", (1, 1, 0)),
        ("update_extra_metadata", (0, 1, 0)),
        ("permit", (3, 2, 0)),
    ];

    bless_or_assert_gas(&actual, expected);
}

// ============================================================================
// meta: op coverage checklist
// ============================================================================

/// Compile-time coverage checklist — never called; it exists only for its two
/// exhaustive `match`es (no `_` arm), each arm naming the golden `#[test]` fn(s) that
/// pin the op via [`covered`].
///
/// This gives two compile-time guarantees:
///   * add an op to the ABI (a new `IB20Calls` / `IB20AssetCalls` variant) → the
///     wildcard-free match fails to build until an arm (and thus a golden) is added;
///   * rename or remove a golden `#[test]` fn → the `covered(&[...])` reference fails
///     to build.
///
/// Because Asset V1 is **frozen**, this checklist is NOT expected to ever be
/// updated: a compile error here means the frozen V1 op surface changed, which must be
/// reviewed.
#[allow(dead_code)]
fn v1_op_coverage_checklist(call: IB20::IB20Calls, ext: IB20Asset::IB20AssetCalls) {
    use IB20::IB20Calls as C;
    use IB20Asset::IB20AssetCalls as SC;

    // No-op: forces each arm to name real golden `#[test]` fns by path.
    fn covered(_goldens: &[fn()]) {}

    match call {
        // ERC-20 core
        C::transfer(_) => covered(&[
            golden_transfer_privileged,
            golden_transfer_unprivileged_allowed,
            golden_transfer_unprivileged_blocked_sender_reverts,
            golden_transfer_reverts_zero_receiver,
            golden_transfer_reverts_insufficient_balance,
            golden_transfer_reverts_when_paused,
            golden_transfer_reverts_zero_sender,
        ]),
        C::transferFrom(_) => covered(&[
            golden_transfer_from_finite_allowance_decrements,
            golden_transfer_from_infinite_allowance_not_decremented,
            golden_transfer_from_reverts_insufficient_allowance,
            golden_transfer_from_unprivileged_enforces_executor_policy,
            golden_transfer_from_reverts_zero_receiver,
            golden_transfer_from_reverts_zero_sender,
        ]),
        C::approve(_) => covered(&[
            golden_approve_sets_allowance_and_emits,
            golden_approve_reverts_zero_spender,
            golden_approve_reverts_zero_approver,
        ]),
        C::transferWithMemo(_) => covered(&[golden_transfer_with_memo_emits_transfer_then_memo]),
        C::transferFromWithMemo(_) => covered(&[golden_transfer_from_with_memo]),

        // mint / burn
        C::mint(_) => covered(&[
            golden_mint_privileged_still_enforces_receiver_policy,
            golden_mint_unprivileged_requires_role_and_policy,
            golden_mint_reverts_over_supply_cap,
            golden_mint_reverts_zero_receiver,
        ]),
        C::mintWithMemo(_) => covered(&[golden_mint_with_memo]),
        C::burn(_) => covered(&[
            golden_burn_requires_role_then_reduces_supply,
            golden_burn_reverts_insufficient_balance,
        ]),
        C::burnWithMemo(_) => covered(&[golden_burn_with_memo]),
        C::burnBlocked(_) => covered(&[
            golden_burn_blocked_destroys_from_blocked_account,
            golden_burn_blocked_reverts_when_not_blocked,
            golden_burn_blocked_unprivileged_requires_role,
        ]),
        C::seizeWithMemo(_)
        | C::SEIZE_ROLE(_)
        | C::SEIZE_EXEMPT_POLICY(_)
        | C::SEIZE_RECEIVER_POLICY(_) => covered(&[golden_seize_selectors_unknown_at_v1]),

        // pause / config / roles / policy / permit
        C::pause(_) => covered(&[
            golden_pause_sets_feature_bit,
            golden_pause_reverts_empty_feature_set,
            golden_pause_unprivileged_requires_role,
            golden_seize_pause_feature_unknown_at_v1,
        ]),
        C::unpause(_) => covered(&[
            golden_unpause_clears_feature_bit,
            golden_unpause_reverts_empty_feature_set,
            golden_unpause_unprivileged_requires_role,
            golden_seize_pause_feature_unknown_at_v1,
        ]),
        C::updateSupplyCap(_) => covered(&[
            golden_update_supply_cap,
            golden_update_supply_cap_reverts_below_supply,
            golden_update_supply_cap_unprivileged_requires_role,
        ]),
        C::updateName(_) => covered(&[
            golden_update_name_emits_name_and_domain_changed,
            golden_update_name_unprivileged_requires_role,
        ]),
        C::updateSymbol(_) => {
            covered(&[golden_update_symbol, golden_update_symbol_unprivileged_requires_role])
        }
        C::updateContractURI(_) => covered(&[
            golden_update_contract_uri,
            golden_update_contract_uri_unprivileged_requires_role,
        ]),
        C::grantRole(_) => covered(&[
            golden_grant_role,
            golden_grant_role_unprivileged_no_admin_reverts,
            golden_grant_role_unprivileged_non_admin_caller_reverts,
            golden_grant_default_admin_bumps_member_count,
            golden_grant_role_idempotent_when_already_held,
        ]),
        C::revokeRole(_) => covered(&[
            golden_revoke_role,
            golden_revoke_last_admin_rejected,
            golden_revoke_role_unprivileged_non_admin_caller_reverts,
            golden_revoke_role_noop_when_not_held,
        ]),
        C::renounceRole(_) => covered(&[
            golden_renounce_role,
            golden_renounce_role_bad_confirmation,
            golden_renounce_role_reverts_last_admin,
        ]),
        C::renounceLastAdmin(_) => {
            covered(&[golden_renounce_last_admin, golden_renounce_last_admin_reverts_when_not_sole])
        }
        C::setRoleAdmin(_) => covered(&[
            golden_set_role_admin,
            golden_set_role_admin_unprivileged_non_admin_caller_reverts,
        ]),
        C::updatePolicy(_) => covered(&[
            golden_update_policy,
            golden_update_policy_reverts_missing_policy,
            golden_update_policy_unprivileged_requires_role,
            golden_update_policy_rejects_seize_scopes_at_v1,
        ]),
        C::permit(_) => covered(&[
            golden_permit_sets_allowance_and_increments_nonce,
            golden_permit_reverts_when_expired,
        ]),

        // computed reads
        C::isPaused(_) | C::pausedFeatures(_) => covered(&[
            golden_read_is_paused_and_paused_features,
            golden_seize_pause_feature_unknown_at_v1,
        ]),
        C::policyId(_) => covered(&[
            golden_read_policy_id_and_unsupported_scope,
            golden_policy_id_rejects_seize_scopes_at_v1,
        ]),
        C::DOMAIN_SEPARATOR(_) => covered(&[golden_read_domain_separator]),
        C::eip712Domain(_) => covered(&[golden_read_eip712_domain]),

        // direct reads
        C::name(_)
        | C::symbol(_)
        | C::decimals(_)
        | C::totalSupply(_)
        | C::balanceOf(_)
        | C::allowance(_)
        | C::supplyCap(_)
        | C::nonces(_)
        | C::contractURI(_)
        | C::hasRole(_)
        | C::getRoleAdmin(_) => covered(&[golden_read_metadata_and_supply]),

        // role / policy-id constants
        C::DEFAULT_ADMIN_ROLE(_)
        | C::MINT_ROLE(_)
        | C::BURN_ROLE(_)
        | C::BURN_BLOCKED_ROLE(_)
        | C::PAUSE_ROLE(_)
        | C::UNPAUSE_ROLE(_)
        | C::METADATA_ROLE(_)
        | C::TRANSFER_SENDER_POLICY(_)
        | C::TRANSFER_RECEIVER_POLICY(_)
        | C::TRANSFER_EXECUTOR_POLICY(_)
        | C::MINT_RECEIVER_POLICY(_) => covered(&[golden_read_role_and_policy_constants]),
    }

    match ext {
        // asset-specific reads
        SC::OPERATOR_ROLE(_) | SC::WAD_PRECISION(_) => {
            covered(&[golden_read_operator_role_and_wad])
        }
        SC::multiplier(_)
        | SC::toScaledBalance(_)
        | SC::toRawBalance(_)
        | SC::scaledBalanceOf(_) => covered(&[golden_read_multiplier_and_scaled_balances]),
        SC::isAnnouncementIdUsed(_) => covered(&[golden_read_is_announcement_id_used]),
        SC::extraMetadata(_) => covered(&[golden_read_extra_metadata]),

        // asset-specific mutations
        SC::updateMultiplier(_) => covered(&[
            golden_update_multiplier,
            golden_update_multiplier_reverts_zero,
            golden_update_multiplier_unprivileged_requires_role,
        ]),
        SC::batchMint(_) => covered(&[
            golden_batch_mint,
            golden_batch_mint_reverts_length_mismatch,
            golden_batch_mint_reverts_empty,
            golden_batch_mint_reverts_when_paused,
            golden_batch_mint_unprivileged_requires_role,
        ]),
        SC::updateExtraMetadata(_) => covered(&[
            golden_update_extra_metadata_set,
            golden_update_extra_metadata_remove,
            golden_update_extra_metadata_reverts_empty_key,
            golden_update_extra_metadata_unprivileged_requires_role,
        ]),
        SC::announce(_) => covered(&[
            golden_announce_emits_and_runs_internal_calls,
            golden_announce_reverts_id_already_used,
            golden_announce_reverts_internal_call_malformed,
            golden_announce_reverts_nested_announce,
            golden_announce_reverts_internal_call_failed,
            golden_announce_unprivileged_requires_role,
        ]),

        // ERC-8056 scheduled-multiplier surface: introduced at V2 (Cobalt). The frozen V1 (Beryl)
        // wire surface does not declare these selectors, so they stay unknown at V1; V2 behavior is
        // cross-validated by the base-std suite in live-precompile mode.
        SC::uiMultiplier(_)
        | SC::newUIMultiplier(_)
        | SC::effectiveAt(_)
        | SC::balanceOfUI(_)
        | SC::totalSupplyUI(_)
        | SC::updateUIMultiplier(_)
        | SC::cancelUIMultiplierUpdate(_)
        | SC::toUIAmount(_)
        | SC::fromUIAmount(_)
        | SC::MAX_UI_MULTIPLIER(_)
        | SC::supportsInterface(_) => covered(&[golden_v2_selectors_unknown_at_v1]),
    }
}
