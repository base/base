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
//! `inner_with_privilege`; the guard envelope (nonpayable / uninitialized / pre-Beryl) via the
//! full `dispatch_with_observer`.
//!
//! ## Blessing storage hashes
//! State-root constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_asset_v1_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, LogData, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    Asset, AssetAccounting, AssetV1, AssetVersion, AssetVersions, B20_MAX_SUPPLY_CAP, B20AssetInit,
    B20AssetStorage, B20AssetToken, B20PolicyType, B20TokenRole, FakePolicyAccounting, IB20,
    IB20Asset, NoopPrecompileCallObserver, PermitArgs, PolicyVersion, TokenAccounting,
};
use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};
use k256::ecdsa::SigningKey;

// --- fixtures ---------------------------------------------------------------

const TOKEN: Address = Address::repeat_byte(0x22);
const ADMIN: Address = Address::repeat_byte(0xAD);
const ALICE: Address = Address::repeat_byte(0xA1);
const BOB: Address = Address::repeat_byte(0xB0);
const CAROL: Address = Address::repeat_byte(0xCA);
const CHAIN_ID: u64 = 8453;
const NAME: &str = "Base Asset";
const SYMBOL: &str = "bASSET";
const DECIMALS: u8 = 6;
const MEMO: B256 = B256::repeat_byte(0x77);
const LOGIC: AssetV1 = AssetV1;

/// A concrete (non-sentinel) ALLOWLIST policy id (type byte = 1, counter = 7).
/// Unconfigured scopes default to the `ALWAYS_ALLOW_ID` (0) EVM zero-slot, so
/// blocking/executor guards are exercised against an explicit id like this one.
/// Under V1, ALLOWLIST (type = 1) authorizes exactly its members: `.allow(POLICY_ID, acct)`
/// grants, and unconfigured accounts are blocked.
const POLICY_ID: u64 = (1u64 << 56) | 7;

// Anvil/Hardhat account 0 — well-known test key, never used in production.
const PRIVATE_KEY: [u8; 32] =
    alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------

const ROOT_FRESH: B256 = b256!("de2b3de71e2d90dd57658394598aad6d034e71866e561ba6019ea023f427260a");
const ROOT_TRANSFER_PRIV: B256 =
    b256!("867ada5b7d66b25e23f7195e7457e55a4f523f6463759b9b05c71f702a98d22b");
const ROOT_TRANSFER_UNPRIV: B256 =
    b256!("c15c5cdd9aefed169bc0d6953d45e075134c43f7e35bbd9ad2c4d941b5ef7a1e");
const ROOT_TRANSFER_WITH_MEMO: B256 =
    b256!("5e2eab9a12124b87bb6978238e375e0374aba305821c7b97c2926664944effa8");
const ROOT_TRANSFER_FROM_FINITE: B256 =
    b256!("82cc44f94691083c0970a0bc614e394c20417629e840899a6ab718b0cce8a4ac");
const ROOT_TRANSFER_FROM_INFINITE: B256 =
    b256!("7dc5250931184f3730965dc873b7cce9cb855aaec380791db3df801bbeb3afaf");
const ROOT_TRANSFER_FROM_WITH_MEMO: B256 =
    b256!("b688a53c7d4b31cc758628a404d4b133f19d2f8726aba04cecb22b788b53ab35");
const ROOT_APPROVE: B256 =
    b256!("55e45e640ea3936ef2e0be30c99aea533c873386a2f5ab2b74b43dcac3d1c7bc");
const ROOT_MINT_PRIV: B256 =
    b256!("fbcefa77e7d1eb4ba9369ec91e403e42c415c3a42a8cca14b042098661383afa");
const ROOT_MINT_UNPRIV: B256 =
    b256!("5df5e927483a97d14c0e0bd65653acc70505d341dcc1b2b108b086161e53c183");
const ROOT_MINT_WITH_MEMO: B256 =
    b256!("79ab20102cd5d0904ad6d9cb47162e3fe7aa6d5256908dc0b08906699d068468");
const ROOT_BURN: B256 = b256!("284b6654388a63ba2883c24d8c4255ac8f4916d2aa2c278d3dfe9944c6b033a4");
const ROOT_BURN_WITH_MEMO: B256 =
    b256!("79283f419fbf8a643accd34969d09b6739c955be2be70074026e792d845d3a5a");
const ROOT_BURN_BLOCKED: B256 =
    b256!("b92c29f185e5834ccaac6e69810890d6b699f11fc85befe336b77fdf9705868f");
const ROOT_PAUSE: B256 = b256!("049d63d7409725d09f1bca25a06c6adaca95872064daed65ffd470387ec08eed");
const ROOT_UNPAUSE: B256 =
    b256!("1d21ba46e60dbc0842c9c88a0c979ec5782d5f47202dbe26cfae8fb9fd928604");
const ROOT_UPDATE_SUPPLY_CAP: B256 =
    b256!("a809d3acae7715263499970a826f2b8213be31e5a3517ef56529ff1fa414136e");
const ROOT_UPDATE_NAME: B256 =
    b256!("43d9c635d8026bfb0eee62958bb2f05d5abc6a25418f2c88da1f1a9eb7b0063b");
const ROOT_UPDATE_SYMBOL: B256 =
    b256!("a616996185a42251dcc639b8c90409546f7842f26002925a00972d0cb5a5347e");
const ROOT_UPDATE_CONTRACT_URI: B256 =
    b256!("22bf8ca8c0bb8f4959d8e997dd6ad431ae6b4ead260de245c3b192c31e664e0e");
const ROOT_GRANT_ROLE: B256 =
    b256!("88a7880af526c558fcf20ae35c08a190c8f85546fae8bd5551a0a35490c96a2c");
const ROOT_REVOKE_ROLE: B256 =
    b256!("46eee799676e49c4960a4bc08cad640d0b4902711a542fa51b05637902e3e5c0");
const ROOT_RENOUNCE_ROLE: B256 =
    b256!("a84c7df0a815abd5a4f7e917eae0c4336d448b528537f7b38d2c34dfc2abf6e0");
const ROOT_RENOUNCE_LAST_ADMIN: B256 =
    b256!("378f31165a3d136ad05016b9b3399e12e2107308d24ad00e9fb30581e1302583");
const ROOT_SET_ROLE_ADMIN: B256 =
    b256!("089db7709749c947b4f92e63421953eafa97e1243a87316b4c21c4a420666841");
const ROOT_UPDATE_POLICY: B256 =
    b256!("52f42a4bca134e5bfdcd41ef222ff39867f02fcfee9e8dde84a3eb6a865a0b64");
const ROOT_PERMIT: B256 = b256!("df45f185b938015154fc9529002ee4f3325e2983c2acbb9b0ac5fc3c606cb3ac");
const ROOT_GRANT_DEFAULT_ADMIN: B256 =
    b256!("7d6eb7b233dd2f680394081d74cae0c91517f98d870d0cf0ed47f6b9aa80387f");
const ROOT_GRANT_IDEMPOTENT: B256 =
    b256!("93b045c96b7252ebea310afd00cf812aaa07deb9283d278d6620196a1e63c618");
const ROOT_GRANT_UNCHECKED: B256 =
    b256!("345d3510df56d0313f0d5db98fd065a2caa8923435c976be644837474dda91e9");

// asset-specific (blessed against v1.1.1)
const ROOT_MULTIPLIER_SCALED: B256 =
    b256!("686201a1ea687d57677c2b0bbf82da29be8e43b7028935651294f77cbd4e32e2");
const ROOT_ANNOUNCE_ID_USED: B256 =
    b256!("78449cce19ec897e715b0c77abab3fb767d0fee4e5f10bf2e789a6a78ff0e9ca");
const ROOT_EXTRA_METADATA_READ: B256 =
    b256!("128ad2b0a1217597b897324c1ea84b7afb1031176bc98726511d29cac5505a96");
const ROOT_UPDATE_MULTIPLIER: B256 =
    b256!("797f188e28956f02934ecb539834a0a7577f20d46ba2e62cc3287cac6959b468");
const ROOT_BATCH_MINT: B256 =
    b256!("76770109f6850da0abbb4c4f68c3d9e8518084054dc3ac0d8fdc9f34e7e7f3ad");
const ROOT_METADATA_SET: B256 =
    b256!("4cab20f91770775eb2adbf1a09c956c65a04931727308c73bf38be693aec691e");
const ROOT_METADATA_REMOVE: B256 =
    b256!("f6a5309a3a75bf0dd2f824c503d866da37f0b00da5e9a17b21cb7fb4f5cf8eb8");
const ROOT_ANNOUNCE: B256 =
    b256!("bc541cb4aecf8c93e8bc0b495eb7d1011c614fcb65704b848a961bb7207dcb83");

// --- harness ----------------------------------------------------------------

/// `U256` from a small literal.
fn u(n: u64) -> U256 {
    U256::from(n)
}

/// The ABI encoding for a boolean-returning op (`transfer`/`approve`).
fn ok_true() -> Bytes {
    Bytes::from(true.abi_encode())
}

/// Fresh provider with an initialized `Base Asset` at [`TOKEN`] (multiplier = 1 WAD).
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
        .inner(ctx, &calldata, BaseUpgrade::Beryl)
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
        .inner_with_privilege(ctx, &calldata, true)
    })
}

/// Topic-0 (signature hash) of the last event emitted by the token.
fn last_topic0(storage: &HashMapStorageProvider) -> B256 {
    storage.get_events(TOKEN).last().expect("an emitted event").topics()[0]
}

/// Deterministic keccak hash of the per-case snapshot: the token's emitted events
/// (topics + data) followed by its sorted `(address, slot, value)` storage triples.
///
/// A plain content hash (not an MPT state root). Events are included so a regression in
/// an event's payload — indexed args or data not otherwise reflected in storage, e.g. a
/// `Memo`'s bytes — is pinned here even though logs are not storage.
fn hash_state(storage: HashMapStorageProvider) -> B256 {
    let events: Vec<LogData> = storage.get_events(TOKEN).clone();
    let mut triples: Vec<(Address, U256, U256)> = storage.into_storage().collect();
    triples.sort();
    let mut buf = Vec::with_capacity(triples.len() * 84 + events.len() * 64);
    for log in &events {
        for topic in log.topics() {
            buf.extend_from_slice(topic.as_slice());
        }
        buf.extend_from_slice(&log.data);
    }
    for (addr, slot, value) in triples {
        buf.extend_from_slice(addr.as_slice());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        buf.extend_from_slice(&value.to_be_bytes::<32>());
    }
    keccak256(&buf)
}

/// Asserts the storage hash, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
fn assert_root(label: &str, storage: HashMapStorageProvider, expected: B256) {
    let got = hash_state(storage);
    if std::env::var("BLESS_GOLDEN").ok().as_deref() == Some("1") {
        println!("GOLDEN_ROOT {label} = {got:#x}");
        return;
    }
    assert_eq!(got, expected, "V1 storage hash drift for `{label}`");
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
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
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
    let mut s = HashMapStorageProvider::new(CHAIN_ID);
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
// dispatch harness wrappers (no-observer dispatch, inner gating, factory bootstrap)
// ============================================================================

#[test]
fn golden_dispatch_no_observer_wrapper_reverts_uninitialized() {
    // Exercises the no-observer `dispatch()` wrapper + the is_initialized=false gate.
    let mut s = HashMapStorageProvider::new(CHAIN_ID);
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
    // Exercises the `inner` version-resolution None branch (pre-introduction fork).
    let mut s = fresh();
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let err = StorageCtx::enter(&mut s, |ctx| {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V1,
        )
        .inner(ctx, &calldata, BaseUpgrade::Azul)
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
        token.grant_role_unchecked(B20TokenRole::DefaultAdmin.id(), ADMIN, TOKEN).unwrap();
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
        .inner_with_privilege(ctx, &calldata, true)
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
    ];

    if std::env::var("BLESS_GOLDEN").ok().as_deref() == Some("1") {
        for (label, counts) in &actual {
            println!("GAS {label} = {counts:?}");
        }
        return;
    }
    assert_eq!(actual, expected, "storage-access footprint (sload, sstore, keccak256) drift");
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

        // pause / config / roles / policy / permit
        C::pause(_) => covered(&[
            golden_pause_sets_feature_bit,
            golden_pause_reverts_empty_feature_set,
            golden_pause_unprivileged_requires_role,
        ]),
        C::unpause(_) => covered(&[
            golden_unpause_clears_feature_bit,
            golden_unpause_reverts_empty_feature_set,
            golden_unpause_unprivileged_requires_role,
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
        ]),
        C::permit(_) => covered(&[
            golden_permit_sets_allowance_and_increments_nonce,
            golden_permit_reverts_when_expired,
        ]),

        // computed reads
        C::isPaused(_) | C::pausedFeatures(_) => {
            covered(&[golden_read_is_paused_and_paused_features])
        }
        C::policyId(_) => covered(&[golden_read_policy_id_and_unsupported_scope]),
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
    }
}
