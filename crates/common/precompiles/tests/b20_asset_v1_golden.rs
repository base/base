//! Golden tests pinning Asset **V1** behavior of the B-20 precompile (BOP-423).
//!
//! Every op (mutations, computed reads, direct/const reads) is driven through the
//! **version-resolver-gated** dispatch path (`BaseUpgrade::Beryl` -> `AssetVersion::V1`)
//! against the real EVM-backed `B20AssetStorage` over `HashMapStorageProvider`, with an
//! `InMemoryPolicy` for deterministic allow/block decisions. Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting state (balances / supply / roles / allowances / multiplier / metadata / storage),
//!   3. emitted events, and
//!   4. a per-case keccak storage **state-root** snapshot (the frozen-manifest baseline).
//!
//! Beyond the shared B-20 surface this also pins the asset-specific ops: `updateMultiplier`,
//! `updateExtraMetadata`, `batchMint`, `announce` (incl. its internal-call loop), and the
//! scaled-balance reads. Because the per-op suite resolves the version via
//! `AssetVersions::from_base_upgrade`, it breaks if dispatch routes to the wrong version.
//! Privileged behavior is exercised via `inner_with_privilege`; the guard envelope
//! (nonpayable / uninitialized / pre-Beryl) via the full `dispatch_with_observer`.
//!
//! ## Blessing state-roots
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_asset_v1_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    Asset, AssetAccounting, AssetV1, AssetVersion, AssetVersions, B20_MAX_SUPPLY_CAP, B20AssetInit,
    B20AssetStorage, B20AssetToken, B20PolicyType, B20TokenRole, IB20, IB20Asset, InMemoryPolicy,
    NoopPrecompileCallObserver, PermitArgs, TokenAccounting,
};
use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};
use k256::ecdsa::SigningKey;

// --- fixtures ---------------------------------------------------------------

const TOKEN: Address = Address::repeat_byte(0x21);
const ADMIN: Address = Address::repeat_byte(0xAD);
const ALICE: Address = Address::repeat_byte(0xA1);
const BOB: Address = Address::repeat_byte(0xB0);
const CAROL: Address = Address::repeat_byte(0xCA);
const CHAIN_ID: u64 = 8453;
const NAME: &str = "Real World Asset";
const SYMBOL: &str = "RWA";
const DECIMALS: u8 = 8;
const MEMO: B256 = B256::repeat_byte(0x77);
const LOGIC: AssetV1 = AssetV1;

/// A concrete (non-sentinel) policy id. Unconfigured scopes default to the
/// `ALWAYS_ALLOW_ID` (0) EVM zero-slot, so blocking/executor guards must be
/// exercised against an explicitly configured policy id like this one.
const POLICY_ID: u64 = 7;

// Anvil/Hardhat account 0 — well-known test key, never used in production.
const PRIVATE_KEY: [u8; 32] =
    alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

// --- pinned state-roots (bless with BLESS_GOLDEN=1; see module docs) --------

const ROOT_FRESH: B256 = b256!("ecd76a0f8f4f5c3d735866149f7ff14fd5df8dc68f646d16f57985b13aaceeda");
const ROOT_TRANSFER_PRIV: B256 =
    b256!("96136372a76712e6d2b146058285317ecbfe4ed254edfa9027cceb76b6f48c62");
const ROOT_TRANSFER_UNPRIV: B256 =
    b256!("481488b45b52011f054573a761b0bd97e30328f286acf8da4853478f8b7345bc");
const ROOT_TRANSFER_WITH_MEMO: B256 =
    b256!("96136372a76712e6d2b146058285317ecbfe4ed254edfa9027cceb76b6f48c62");
const ROOT_TRANSFER_FROM_FINITE: B256 =
    b256!("2cd788a8aff3e96627ea1d461f7c35af8064e09491e4a8f4332de3de001e6d15");
const ROOT_TRANSFER_FROM_INFINITE: B256 =
    b256!("863cfb8676e127c8a26f595029bb78ba24914925bc064275fb659b69fe882cd8");
const ROOT_TRANSFER_FROM_WITH_MEMO: B256 =
    b256!("40faf2e88c394b7f0de368b3e7bb9040b4ba8be83e482ec04be05da65739f275");
const ROOT_APPROVE: B256 =
    b256!("da80ed7616e0890f42d85f49c7aabced174bafb7a0a019c76432ba855ec8b185");
const ROOT_MINT_PRIV: B256 =
    b256!("4e1ee427e3c44b48a6f87b7a49c2c770fcb75e2d2adbfbe4e590940a2066fdbd");
const ROOT_MINT_UNPRIV: B256 =
    b256!("9f2dc6d3b0f77e513ea7c500bb3b08235efcd95254bdcbb1058f29382b4646a0");
const ROOT_MINT_WITH_MEMO: B256 =
    b256!("54c114db87fb17d73e11b586fc68c213841f947675eee2d18afbf712eddb0e2c");
const ROOT_BURN: B256 = b256!("44d6f01a44eaf4649175dca71b8c7e361ae6b63206c3fbb60a8e1dddc4ad2564");
const ROOT_BURN_WITH_MEMO: B256 =
    b256!("44d6f01a44eaf4649175dca71b8c7e361ae6b63206c3fbb60a8e1dddc4ad2564");
const ROOT_BURN_BLOCKED: B256 =
    b256!("177e7b581f34fe13c680fbd3006195c47476eb7c00ac83c6ae05f823254be604");
const ROOT_PAUSE: B256 = b256!("5db18b351d26832b17e7c5e087e839d7b1c3f33ea940271790eff58aa9754eb6");
const ROOT_UNPAUSE: B256 =
    b256!("1b2266695094856d8d6ba8c3bfb4ddad06eb62e2e84f7a0b7aad73d7ced77c8b");
const ROOT_UPDATE_SUPPLY_CAP: B256 =
    b256!("c015947401c254de0048d23829260de53f8336e931303f6a834cfbadbc26cadf");
const ROOT_UPDATE_NAME: B256 =
    b256!("b85866e5c928bb1bc3bd6a1a37997e810b8f12a4f954ffa8be864201e768d408");
const ROOT_UPDATE_SYMBOL: B256 =
    b256!("4ceb55698e686f56e42acd2b520aa11d3a721fffad18b2c84b919d845b8ea05c");
const ROOT_UPDATE_CONTRACT_URI: B256 =
    b256!("105a00417a775dc6952d888da191665eea83bc0da04055ec399b23e0af4b784f");
const ROOT_GRANT_ROLE: B256 =
    b256!("466d162a8569f3ed273ea3689aff08281147acafe9880be40875abaa2f23da01");
const ROOT_REVOKE_ROLE: B256 =
    b256!("e02df46d329afcff5fe3d109b3a33184cdf8d87811bceb9c409014deb2f3f6e8");
const ROOT_RENOUNCE_ROLE: B256 =
    b256!("e02df46d329afcff5fe3d109b3a33184cdf8d87811bceb9c409014deb2f3f6e8");
const ROOT_RENOUNCE_LAST_ADMIN: B256 =
    b256!("bd0a4b40f729d855f33e56d59ab7d54caa9c17309b09283dd97a8adcfeb06bea");
const ROOT_SET_ROLE_ADMIN: B256 =
    b256!("29e24a05fe68557c945411e65b7a8b4fe28e6c3a0ea9a3ffd6c1b4ad4b4f83ff");
const ROOT_UPDATE_POLICY: B256 =
    b256!("2930532eb1606d19001dd17bbd6a48d00ec12fe72a1e6ae50ca00ea314b05b3b");
const ROOT_PERMIT: B256 = b256!("65cdbfcff466d847e7360f1d72ca45be64356af1e77eea83533faf425966e836");
const ROOT_UPDATE_MULTIPLIER: B256 =
    b256!("46294ac17788ce1755c5f9504a08af0c1d0689fc2bfb739032cbb335a9d9d904");
const ROOT_UPDATE_EXTRA_METADATA: B256 =
    b256!("4e461dd1b6f3383297f97e4d19848fd5484b7adb5adc780d721d149805fac16a");
const ROOT_BATCH_MINT: B256 =
    b256!("abeeb4354269e4994a293dc1decc3c33a61c566f8d8b8479a018c8a03bd744b0");
const ROOT_ANNOUNCE: B256 =
    b256!("18badab132a0ba9de303e277d69816df82091fdc024322ea600bdbd3a6147068");

// --- harness ----------------------------------------------------------------

/// `U256` from a small literal.
fn u(n: u64) -> U256 {
    U256::from(n)
}

/// One WAD (1e18) — the multiplier precision.
fn wad() -> U256 {
    B20AssetStorage::WAD
}

/// The asset operator role id (`keccak256("OPERATOR_ROLE")`); required for
/// `announce` / `updateMultiplier`. `AssetV1::OPERATOR_ROLE` is crate-private, so
/// it is recomputed here from the same preimage.
fn operator_role() -> B256 {
    keccak256("OPERATOR_ROLE")
}

/// The ABI encoding for a boolean-returning op (`transfer`/`approve`).
fn ok_true() -> Bytes {
    Bytes::from(true.abi_encode())
}

/// Fresh provider with an initialized `Real World Asset` token at [`TOKEN`] (1:1 multiplier).
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new(CHAIN_ID);
    StorageCtx::enter(&mut storage, |ctx| {
        let mut token = B20AssetStorage::from_address(TOKEN, ctx);
        token
            .initialize(B20AssetInit {
                name: NAME.into(),
                symbol: SYMBOL.into(),
                supply_cap: B20_MAX_SUPPLY_CAP,
                multiplier: B20AssetStorage::WAD,
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
    policy: InMemoryPolicy,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20AssetToken::with_storage_and_policy(B20AssetStorage::from_address(TOKEN, ctx), policy)
            .inner(ctx, &calldata, BaseUpgrade::Beryl)
    })
}

/// Drives one op through V1 with factory-init privilege (guards skipped).
fn op_privileged(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: InMemoryPolicy,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20AssetToken::with_storage_and_policy(B20AssetStorage::from_address(TOKEN, ctx), policy)
            .inner_with_privilege(ctx, &calldata, true)
    })
}

/// Topic-0 (signature hash) of the last event emitted by the token.
fn last_topic0(storage: &HashMapStorageProvider) -> B256 {
    storage.get_events(TOKEN).last().expect("an emitted event").topics()[0]
}

/// Deterministic keccak state-root over the sorted `(address, slot, value)` storage triples.
fn state_root(storage: HashMapStorageProvider) -> B256 {
    let mut triples: Vec<(Address, U256, U256)> = storage.into_storage().collect();
    triples.sort();
    let mut buf = Vec::with_capacity(triples.len() * 84);
    for (addr, slot, value) in triples {
        buf.extend_from_slice(addr.as_slice());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        buf.extend_from_slice(&value.to_be_bytes::<32>());
    }
    keccak256(&buf)
}

/// Asserts the storage state-root, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
fn assert_root(label: &str, storage: HashMapStorageProvider, expected: B256) {
    let got = state_root(storage);
    if std::env::var_os("BLESS_GOLDEN").is_some() {
        println!("GOLDEN_ROOT {label} = {got:#x}");
        return;
    }
    assert_eq!(got, expected, "V1 storage state-root drift for `{label}`");
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

/// Recovers the anvil account-0 address from [`PRIVATE_KEY`].
fn anvil_owner() -> Address {
    let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// The V1 EIP-712 domain separator for the token at [`TOKEN`] on [`CHAIN_ID`].
fn domain_separator(storage: &mut HashMapStorageProvider) -> B256 {
    StorageCtx::enter(storage, |ctx| {
        let token = B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            InMemoryPolicy::new(),
        );
        LOGIC.domain_separator(&token, CHAIN_ID).unwrap()
    })
}

/// Builds a validly-signed `permit` call for `owner`'s current nonce.
fn signed_permit(
    domain_sep: B256,
    nonce: U256,
    owner: Address,
    spender: Address,
    value: U256,
    deadline: U256,
) -> IB20::permitCall {
    let mut args =
        PermitArgs { owner, spender, value, deadline, v: 0, r: B256::ZERO, s: B256::ZERO };
    let signing_hash = args.signing_hash(domain_sep, nonce);
    let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
    let (sig, recid) = key.sign_prehash_recoverable(signing_hash.as_slice()).unwrap();
    let bytes = sig.to_bytes();
    args.r = B256::from_slice(&bytes[..32]);
    args.s = B256::from_slice(&bytes[32..]);
    args.v = if recid.is_y_odd() { 28 } else { 27 };
    IB20::permitCall {
        owner: args.owner,
        spender: args.spender,
        value: args.value,
        deadline: args.deadline,
        v: args.v,
        r: args.r,
        s: args.s,
    }
}

// ============================================================================
// Version resolver
// ============================================================================

#[test]
fn resolver_maps_forks_to_versions() {
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(AssetVersion::V1));
    assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(AssetVersion::V1));
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
        InMemoryPolicy::new(),
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
    let mut policy = InMemoryPolicy::new();
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
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
    });
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] }.abi_encode(),
    )
    .unwrap();
    let err = op_privileged(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        t.set_policy_id(B20PolicyType::TransferExecutor.id(), POLICY_ID).unwrap();
    });
    let err = op(
        &mut s,
        BOB,
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
    let mut policy = InMemoryPolicy::new();
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
    let mut policy = InMemoryPolicy::new();
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

    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let mut policy = InMemoryPolicy::new();
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
    let mut policy = InMemoryPolicy::new();
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
    let mut policy = InMemoryPolicy::new();
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

    let err =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20::burnCall { amount: u(1) }.abi_encode())
            .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: B20TokenRole::Burn.id(),
        })
    );

    seed(&mut s, |t| give_role(t, B20TokenRole::Burn.id(), ALICE));
    let out =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20::burnCall { amount: u(40) }.abi_encode())
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
        InMemoryPolicy::new(),
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
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
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
    let mut policy = InMemoryPolicy::new();
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20::unpauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    assert_eq!(last_topic0(&s), IB20::Unpaused::SIGNATURE_HASH);
    assert_root("unpause", s, ROOT_UNPAUSE);
}

#[test]
fn golden_pause_reverts_empty_feature_set() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
        IB20::updateSymbolCall { newSymbol: "RWX".into() }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.symbol().unwrap(), "RWX"));
    assert_eq!(last_topic0(&s), IB20::SymbolUpdated::SIGNATURE_HASH);
    assert_root("update_symbol", s, ROOT_UPDATE_SYMBOL);
}

#[test]
fn golden_update_contract_uri() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
    let out = op(&mut s, ADMIN, InMemoryPolicy::new(), IB20::renounceLastAdminCall {}.abi_encode())
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
    let err = op(&mut s, ADMIN, InMemoryPolicy::new(), IB20::renounceLastAdminCall {}.abi_encode())
        .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::NotSoleAdmin {}));
}

#[test]
fn golden_set_role_admin() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
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
    let mut policy = InMemoryPolicy::new();
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
        InMemoryPolicy::new(),
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
    let out = op(&mut s, owner, InMemoryPolicy::new(), call.abi_encode()).unwrap();

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
    let err = op(&mut s, owner, InMemoryPolicy::new(), call.abi_encode()).unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::ExpiredSignature { deadline: u(10) }));
}

// ============================================================================
// asset-specific: updateMultiplier
// ============================================================================

#[test]
fn golden_update_multiplier() {
    let mut s = fresh();
    let target = wad() * u(2);
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::updateMultiplierCall { newMultiplier: target }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(AssetAccounting::multiplier(t).unwrap(), target));
    assert_eq!(last_topic0(&s), IB20Asset::MultiplierUpdated::SIGNATURE_HASH);
    assert_root("update_multiplier", s, ROOT_UPDATE_MULTIPLIER);
}

#[test]
fn golden_update_multiplier_requires_operator_role() {
    let mut s = fresh();
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::updateMultiplierCall { newMultiplier: wad() }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: operator_role(),
        })
    );
}

#[test]
fn golden_update_multiplier_rejects_zero() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::updateMultiplierCall { newMultiplier: U256::ZERO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InvalidMultiplier {}));
}

// ============================================================================
// asset-specific: updateExtraMetadata
// ============================================================================

#[test]
fn golden_update_extra_metadata() {
    let mut s = fresh();
    let out = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::updateExtraMetadataCall { key: "category".into(), value: "fund".into() }
            .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert_eq!(t.extra_metadata("category").unwrap(), "fund"));
    assert_eq!(last_topic0(&s), IB20Asset::ExtraMetadataUpdated::SIGNATURE_HASH);
    assert_root("update_extra_metadata", s, ROOT_UPDATE_EXTRA_METADATA);
}

#[test]
fn golden_update_extra_metadata_rejects_empty_key() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::updateExtraMetadataCall { key: String::new(), value: "x".into() }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InvalidMetadataKey {}));
}

// ============================================================================
// asset-specific: batchMint
// ============================================================================

#[test]
fn golden_batch_mint() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, B20TokenRole::Mint.id(), ALICE));
    let out = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::batchMintCall { recipients: vec![BOB, CAROL], amounts: vec![u(100), u(200)] }
            .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(t.balance_of(BOB).unwrap(), u(100));
        assert_eq!(t.balance_of(CAROL).unwrap(), u(200));
        assert_eq!(t.total_supply().unwrap(), u(300));
    });
    assert_root("batch_mint", s, ROOT_BATCH_MINT);
}

#[test]
fn golden_batch_mint_reverts_length_mismatch() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::batchMintCall { recipients: vec![BOB, CAROL], amounts: vec![u(100)] }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20Asset::LengthMismatch { leftLen: u(2), rightLen: u(1) })
    );
}

#[test]
fn golden_batch_mint_reverts_empty_batch() {
    let mut s = fresh();
    let err = op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20Asset::batchMintCall { recipients: vec![], amounts: vec![] }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::EmptyBatch {}));
}

// ============================================================================
// asset-specific: announce
// ============================================================================

#[test]
fn golden_announce_runs_internal_calls_and_brackets_events() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let target = wad() * u(2);
    let internal =
        Bytes::from(IB20Asset::updateMultiplierCall { newMultiplier: target }.abi_encode());
    let out = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::announceCall {
            internalCalls: vec![internal],
            id: "2026-Q1-split".into(),
            description: "quarterly split".into(),
            uri: "ipfs://a".into(),
        }
        .abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| {
        assert_eq!(AssetAccounting::multiplier(t).unwrap(), target);
        assert!(t.is_announcement_id_used("2026-Q1-split").unwrap());
    });
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 3].topics()[0], IB20Asset::Announcement::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 2].topics()[0], IB20Asset::MultiplierUpdated::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 1].topics()[0], IB20Asset::EndAnnouncement::SIGNATURE_HASH);
    assert_root("announce", s, ROOT_ANNOUNCE);
}

#[test]
fn golden_announce_reverts_on_reused_id() {
    let mut s = fresh();
    seed(&mut s, |t| {
        give_role(t, operator_role(), ALICE);
        t.mark_announcement_id_used("dup").unwrap();
    });
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
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

#[test]
fn golden_announce_reverts_on_nested_announce() {
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
        InMemoryPolicy::new(),
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
fn golden_announce_wraps_failing_internal_call() {
    let mut s = fresh();
    // ALICE has OPERATOR_ROLE (for announce) but not MINT_ROLE (for the inner mint).
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let inner = Bytes::from(IB20::mintCall { to: BOB, amount: U256::ONE }.abi_encode());
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::announceCall {
            internalCalls: vec![inner.clone()],
            id: "split".into(),
            description: String::new(),
            uri: String::new(),
        }
        .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20Asset::InternalCallFailed { call: inner }));
}

#[test]
fn golden_announce_reverts_on_malformed_internal_call() {
    let mut s = fresh();
    seed(&mut s, |t| give_role(t, operator_role(), ALICE));
    let malformed = Bytes::from(vec![0x01, 0x02]);
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::announceCall {
            internalCalls: vec![malformed.clone()],
            id: "split".into(),
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

// ============================================================================
// computed reads
// ============================================================================

#[test]
fn golden_read_is_paused_and_paused_features() {
    let mut s = fresh();
    op_privileged(
        &mut s,
        ADMIN,
        InMemoryPolicy::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
    )
    .unwrap();

    let paused_mint = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20::isPausedCall { feature: IB20::PausableFeature::MINT }.abi_encode(),
    )
    .unwrap();
    assert_eq!(paused_mint, Bytes::from(true.abi_encode()));

    let paused_transfer = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20::isPausedCall { feature: IB20::PausableFeature::TRANSFER }.abi_encode(),
    )
    .unwrap();
    assert_eq!(paused_transfer, Bytes::from(false.abi_encode()));

    let features =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20::pausedFeaturesCall {}.abi_encode()).unwrap();
    assert_eq!(features, Bytes::from(vec![IB20::PausableFeature::MINT].abi_encode()));
}

#[test]
fn golden_read_policy_id_and_unsupported_scope() {
    let mut s = fresh();
    let ok = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20::policyIdCall { policyScope: B20PolicyType::TransferSender.id() }.abi_encode(),
    )
    .unwrap();
    assert_eq!(ok, Bytes::from(0u64.abi_encode()));

    let bad_scope = B256::repeat_byte(0xEE);
    let err = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
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
    let out = op(&mut s, ALICE, InMemoryPolicy::new(), IB20::DOMAIN_SEPARATORCall {}.abi_encode())
        .unwrap();
    assert_eq!(out, Bytes::from(expected.abi_encode()));
    assert_root("read_domain_separator", s, ROOT_FRESH);
}

#[test]
fn golden_read_eip712_domain() {
    let mut s = fresh();
    let out =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20::eip712DomainCall {}.abi_encode()).unwrap();
    let decoded = IB20::eip712DomainCall::abi_decode_returns(&out).unwrap();
    assert_eq!(decoded.name, NAME);
    assert_eq!(decoded.version, "1");
    assert_eq!(decoded.chainId, U256::from(CHAIN_ID));
    assert_eq!(decoded.verifyingContract, TOKEN);
    assert_eq!(decoded.fields, alloy_primitives::FixedBytes::<1>::from([0x0f]));
}

// ============================================================================
// asset-specific reads (multiplier / scaled balances / metadata / announce ids)
// ============================================================================

#[test]
fn golden_read_asset_constants_and_multiplier() {
    let mut s = fresh();
    let cases: Vec<(Vec<u8>, Bytes)> = vec![
        (IB20Asset::multiplierCall {}.abi_encode(), Bytes::from(wad().abi_encode())),
        (IB20Asset::WAD_PRECISIONCall {}.abi_encode(), Bytes::from(wad().abi_encode())),
        (IB20Asset::OPERATOR_ROLECall {}.abi_encode(), Bytes::from(operator_role().abi_encode())),
        (
            IB20Asset::extraMetadataCall { key: "category".into() }.abi_encode(),
            Bytes::from(String::new().abi_encode()),
        ),
        (
            IB20Asset::isAnnouncementIdUsedCall { id: "x".into() }.abi_encode(),
            Bytes::from(false.abi_encode()),
        ),
    ];
    for (calldata, expected) in cases {
        let out = op(&mut s, ALICE, InMemoryPolicy::new(), calldata).unwrap();
        assert_eq!(out, expected);
    }
    assert_root("read_asset_constants", s, ROOT_FRESH);
}

#[test]
fn golden_read_scaled_balances_with_multiplier() {
    let mut s = fresh();
    // 2x multiplier: scaled = raw * 2; raw = scaled / 2.
    seed(&mut s, |t| {
        t.set_multiplier(wad() * u(2)).unwrap();
        t.set_balance(ALICE, u(50)).unwrap();
    });

    let scaled = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::toScaledBalanceCall { rawBalance: u(50) }.abi_encode(),
    )
    .unwrap();
    assert_eq!(scaled, Bytes::from(u(100).abi_encode()));

    let raw = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::toRawBalanceCall { scaledBalance: u(100) }.abi_encode(),
    )
    .unwrap();
    assert_eq!(raw, Bytes::from(u(50).abi_encode()));

    let scaled_of = op(
        &mut s,
        ALICE,
        InMemoryPolicy::new(),
        IB20Asset::scaledBalanceOfCall { account: ALICE }.abi_encode(),
    )
    .unwrap();
    assert_eq!(scaled_of, Bytes::from(u(100).abi_encode()));
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
        (IB20::decimalsCall {}.abi_encode(), Bytes::from(u(DECIMALS as u64).abi_encode())),
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
            Bytes::from(B20TokenRole::DefaultAdmin.id().abi_encode()),
        ),
    ];
    for (calldata, expected) in cases {
        let out = op(&mut s, ALICE, InMemoryPolicy::new(), calldata).unwrap();
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
        let out = op(&mut s, ALICE, InMemoryPolicy::new(), calldata).unwrap();
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
            InMemoryPolicy::new(),
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
            InMemoryPolicy::new(),
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
    let mut s = HashMapStorageProvider::new(CHAIN_ID);
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            InMemoryPolicy::new(),
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
