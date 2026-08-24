//! Golden tests pinning Stablecoin **V2** behavior of the B-20 precompile (Cobalt).
//!
//! V2 activates at Cobalt as a behavior-identical copy of V1 (a scaffold seam for future
//! Cobalt-era changes) with one addition: `seizeWithMemo` (and its 3 common-surface getters)
//! move from the trait's default `reject_frozen_selector!()` body to a real implementation.
//! Every op that does not touch that addition carries V1's verbatim body, and storage is
//! append-only, so — following the same precedent as `b20_policy_v2_golden.rs` and
//! `b20_asset_v2_golden.rs` — the behavior-preserving ops below pin their own roots
//! independently: this suite locks V2's behavior on its own (a future edit to V2 that changes
//! state, events, or gas must re-bless these roots). Because these goldens run production Cobalt
//! storage features (see below) while the V1 suite runs Legacy, a behavior-preserving op's root
//! matches V1's only when the op does not trigger Cobalt's dynamic tail cleanup; ops that write
//! shrinking dynamic values (name/symbol/contract-URI) can legitimately diverge from V1's Legacy
//! pin. `seizeWithMemo` is genuinely new at V2 and gets its own fresh roots.
//!
//! Every op is driven through the **version-resolver-gated** dispatch path
//! (`BaseUpgrade::Cobalt` -> `StablecoinVersion::V2`) against the real EVM-backed
//! `B20StablecoinStorage` over `HashMapStorageProvider` configured with `StorageFeatures::Cobalt`
//! (the production storage config at Cobalt), with a `FakePolicyAccounting` for deterministic
//! allow/block decisions. Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting state (balances / supply / roles / allowances / storage),
//!   3. emitted events, and
//!   4. a per-case keccak storage **hash** snapshot (the frozen-manifest baseline).
//!
//! ## Blessing storage hashes
//! State-root constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_stablecoin_v2_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, U256, b256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    B20_MAX_SUPPLY_CAP, B20PolicyType, B20StablecoinInit, B20StablecoinStorage, B20StablecoinToken,
    B20TokenRole, FakePolicyAccounting, IB20, IB20Stablecoin, NoopPrecompileCallObserver,
    PolicyVersion, Stablecoin, StablecoinV2, StablecoinVersion, StablecoinVersions,
    TokenAccounting, UpgradeGatedStorageFeatures,
};
use base_precompile_storage::{BasePrecompileError, HashMapStorageProvider, StorageCtx};

mod common;
use common::{
    ADMIN, ALICE, BOB, CAROL, CHAIN_ID, MEMO, POLICY_ID, TOKEN, anvil_owner, bless_or_assert_gas,
    bless_or_assert_root, hash_token_state, ok_true, signed_permit, u,
};

// --- fixtures ---------------------------------------------------------------

const NAME: &str = "USD Coin";
const SYMBOL: &str = "USDC";
const CURRENCY: &str = "USD";
const LOGIC: StablecoinV2 = StablecoinV2;

/// A second policy id distinct from [`POLICY_ID`], used where a test needs both a
/// seize-holder and a seize-receiver scope configured simultaneously.
const POLICY_ID_2: u64 = (1u64 << 56) | 8;

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------
//
// These ops carry V1's unmodified body at V2 and storage is append-only, so their snapshots track
// V1's — but pinned independently here: these goldens run `StorageFeatures::Cobalt` while the V1
// suite runs Legacy, so an op that triggers Cobalt dynamic tail cleanup (name/symbol/contract-URI)
// can diverge from its V1 pin. Re-blessing reflects the true Cobalt snapshot for each.

const ROOT_FRESH: B256 = b256!("7f52ac593dc5c5de5e040f65148db8c081010c85db757516d9eb2c19e8903951");
const ROOT_TRANSFER_PRIV: B256 =
    b256!("55bdd0b008a5e28bd9dee4572766a7bce75b0147fb614c9b4874963fc18ef390");
const ROOT_TRANSFER_UNPRIV: B256 =
    b256!("dc5dfb01848c6061b25b98deee929c2bc9dd05191e1892186254aedef4445ace");
const ROOT_TRANSFER_WITH_MEMO: B256 =
    b256!("8c9923a10e52e0dd795aed030a844bcff443ee66d4908caf897a525e1de4f867");
const ROOT_TRANSFER_FROM_FINITE: B256 =
    b256!("9f644119a7130cd4fabba18dae6980e4b9a48f5416819c1954e9e932e514e6e7");
const ROOT_TRANSFER_FROM_INFINITE: B256 =
    b256!("82e62dc5394bea0ebfe17dd63a093c52b4ceae8facf063b44ccc0597480cc49c");
const ROOT_TRANSFER_FROM_WITH_MEMO: B256 =
    b256!("982762526afaf9b37c8bc0090352cb27d2150603791690d7344ea19ae7143269");
const ROOT_APPROVE: B256 =
    b256!("9837570caf42d864a0bac32087df15d3666a0de714567d951564b145b2b5a41e");
const ROOT_MINT_PRIV: B256 =
    b256!("749a0f706e60853de51cd87c7312c104b0783c731b39d34016be07f9c76c0c50");
const ROOT_MINT_UNPRIV: B256 =
    b256!("1d5cf40eb04aafe96b4a32c9734f58a94ee0eca0ddbffbc3d6ab9f45db9cc587");
const ROOT_MINT_WITH_MEMO: B256 =
    b256!("aea0744daa897ae140dc5fdabbd66bd520815e87c75086f1caf5bd5d8db45455");
const ROOT_BURN: B256 = b256!("e292d12852ea52c48bf7869feac153e12aff28fdc301d0c641fa3629d258dcef");
const ROOT_BURN_WITH_MEMO: B256 =
    b256!("a261f181fb9c7b7143307339be3844de4584275bac4bc002a1cdbc2547757898");
const ROOT_BURN_BLOCKED: B256 =
    b256!("adc5a77aca0c7da11dd25ff69d2434badf8d0f035eacd2de7cdf5592efc31c2a");
const ROOT_PAUSE: B256 = b256!("8fc4e227c8dcc72faebe02a2f0154ff0834d5a99cf472e15ea6e49d742c299ef");
const ROOT_UNPAUSE: B256 =
    b256!("67f1ec70420578aafb490cdc86a5e450211342259aa79b0fb18944bffe3de1e8");
const ROOT_UPDATE_SUPPLY_CAP: B256 =
    b256!("18b9e262e9471a0013e0600b698ef9c74bcfccefcfcad83a46251c9f8e817e27");
const ROOT_UPDATE_NAME: B256 =
    b256!("a9b4b1d35935031022f5f9da53db1b75cca0f290cd0c477d88452806abfb802c");
const ROOT_UPDATE_SYMBOL: B256 =
    b256!("aad153e419c17753d3bf730d6183164458c858379a81e4eb35687b08005617ad");
const ROOT_UPDATE_CONTRACT_URI: B256 =
    b256!("2678f67a192fd017125a2b1b9616a894a156018956e7bb99a15ac8bdf475a7a1");
const ROOT_GRANT_ROLE: B256 =
    b256!("e8ec8239f7b10e736151fc068e82a2d0940a4f6ebf184bf71616d9058467570f");
const ROOT_REVOKE_ROLE: B256 =
    b256!("9cd346a450843658a0d04ec37a78709b1faf2a973cbdcf796b44f03643243bad");
const ROOT_RENOUNCE_ROLE: B256 =
    b256!("4de44c01372b636686247aea8724576df6e778f2a94535a2efd71b6b81625441");
const ROOT_RENOUNCE_LAST_ADMIN: B256 =
    b256!("143ade4c83f79a0ebc2bbc75c7d6e8a4ce7ace0235c0ffce003e5f6518276826");
const ROOT_SET_ROLE_ADMIN: B256 =
    b256!("fd229bb98a9695f489f482515f62a4389565473c86517c76de97e7731a60c5fe");
const ROOT_UPDATE_POLICY: B256 =
    b256!("b2c704ab3f2d4cb586548ef9374a83d1727515c33d599295904056fcecd97775");
const ROOT_PERMIT: B256 = b256!("7c710860355d6a906d9342a724549a59f7359b2d8aff6bf8b5039562c93c71a6");
const ROOT_GRANT_DEFAULT_ADMIN: B256 =
    b256!("c828bb784b6ca1d7a3a255a7e5264350ce4293acc38820a57bcc93853abea9f4");
const ROOT_GRANT_IDEMPOTENT: B256 =
    b256!("76f5d7e14530b4534e18e2e3c4a3a3035da857704c314ccfc7f9445ecfe90da8");

// --- pinned storage hashes: V2-only (bless with BLESS_GOLDEN=1) --------------
//
// `seizeWithMemo` is genuinely new at V2 (V1's default trait body rejects it), so it gets its
// own fresh pin.

const ROOT_SEIZE: B256 = b256!("949bcedb68b9804f2c5b939d7189ac09ecca6ef17e7897181c24891dbd628f5d");

// --- harness ----------------------------------------------------------------

/// Fresh provider with an initialized `USD Coin` stablecoin at [`TOKEN`].
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Cobalt),
    );
    StorageCtx::enter(&mut storage, |ctx| {
        let mut token = B20StablecoinStorage::from_address(TOKEN, ctx);
        token
            .initialize(B20StablecoinInit {
                name: NAME.into(),
                symbol: SYMBOL.into(),
                supply_cap: B20_MAX_SUPPLY_CAP,
                currency: CURRENCY.into(),
            })
            .expect("initialize stablecoin");
    });
    storage
}

/// Mutates raw token storage through the accounting port (test setup only).
fn seed(storage: &mut HashMapStorageProvider, f: impl FnOnce(&mut B20StablecoinStorage<'_>)) {
    StorageCtx::enter(storage, |ctx| {
        let mut token = B20StablecoinStorage::from_address(TOKEN, ctx);
        f(&mut token);
    });
}

/// Reads token state through the accounting port.
fn read<R>(
    storage: &mut HashMapStorageProvider,
    f: impl FnOnce(&B20StablecoinStorage<'_>) -> R,
) -> R {
    StorageCtx::enter(storage, |ctx| f(&B20StablecoinStorage::from_address(TOKEN, ctx)))
}

/// Drives one op through the resolver-gated (`Cobalt` -> V2) unprivileged path.
fn op(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        let version = StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt)
            .expect("Cobalt activates V2");
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V2,
        )
        .route(ctx, &calldata, version, false, NoopPrecompileCallObserver)
    })
}

/// Drives one op through V2 with factory-init privilege (guards skipped).
fn op_privileged(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V2,
        )
        .route(ctx, &calldata, StablecoinVersion::V2, true, NoopPrecompileCallObserver)
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
fn give_role(token: &mut B20StablecoinStorage<'_>, role: B256, who: Address) {
    token.set_role(role, who, true).unwrap();
    let next = token.role_member_count(role).unwrap() + U256::ONE;
    token.set_role_member_count(role, next).unwrap();
}

/// Credits `who` with `amount` and grows total supply to match (setup only).
fn fund(token: &mut B20StablecoinStorage<'_>, who: Address, amount: U256) {
    let balance = token.balance_of(who).unwrap();
    token.set_balance(who, balance + amount).unwrap();
    let supply = token.total_supply().unwrap();
    token.set_total_supply(supply + amount).unwrap();
}

/// The V2 EIP-712 domain separator for the token at [`TOKEN`] on [`CHAIN_ID`].
fn domain_separator(storage: &mut HashMapStorageProvider) -> B256 {
    StorageCtx::enter(storage, |ctx| {
        let token = B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V2,
        );
        LOGIC.domain_separator(&token, CHAIN_ID).unwrap()
    })
}

/// Configures `from` as seizable (not authorized by `SeizeHolder`) and `to` as an authorized
/// `SeizeReceiver`, using two distinct policy ids so the two scopes are independently exercised.
fn make_seizable(token: &mut B20StablecoinStorage<'_>) {
    token.set_policy_id(B20PolicyType::SeizeHolder.id(), POLICY_ID).unwrap();
    token.set_policy_id(B20PolicyType::SeizeReceiver.id(), POLICY_ID_2).unwrap();
}

/// A `FakePolicyAccounting` that authorizes `who` under [`POLICY_ID_2`] (the seize-receiver
/// scope configured by [`make_seizable`]), leaving [`POLICY_ID`] (seize-holder) with no members.
fn seize_receiver_policy(who: Address) -> FakePolicyAccounting {
    let mut policy = FakePolicyAccounting::new();
    policy.allow(POLICY_ID_2, who);
    policy
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

/// A `FakePolicyAccounting` authorizing `who` under the default (0) scope.
fn allow0(who: Address) -> FakePolicyAccounting {
    let mut p = FakePolicyAccounting::new();
    p.allow(0, who);
    p
}

// ============================================================================
// version resolver / dispatch envelope
// ============================================================================

#[test]
fn resolver_activates_v2_at_cobalt() {
    assert_eq!(
        StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt),
        Some(StablecoinVersion::V2)
    );
}

#[test]
fn dispatch_rejects_nonzero_value() {
    let mut s = fresh();
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    s.set_call_value(U256::ONE);
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V2,
        )
        .dispatch_with_observer(
            ctx,
            &calldata,
            BaseUpgrade::Cobalt,
            NoopPrecompileCallObserver,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
    assert_eq!(out.bytes, Bytes::from(IB20::NonPayable {}.abi_encode()));
}

#[test]
fn dispatch_reverts_when_uninitialized() {
    let mut s = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Cobalt),
    );
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V2,
        )
        .dispatch_with_observer(
            ctx,
            &calldata,
            BaseUpgrade::Cobalt,
            NoopPrecompileCallObserver,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
    assert!(out.bytes.is_empty());
}

#[test]
fn golden_dispatch_no_observer_wrapper_reverts_uninitialized() {
    // Exercises the no-observer `dispatch()` wrapper + the is_initialized=false gate.
    let mut s = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(BaseUpgrade::Cobalt),
    );
    s.set_caller(ALICE);
    let calldata = IB20::balanceOfCall { account: ALICE }.abi_encode();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            FakePolicyAccounting::new(),
            PolicyVersion::V2,
        )
        .dispatch(ctx, &calldata, BaseUpgrade::Cobalt)
    })
    .expect("dispatch must not fatally error");
    assert!(out.is_revert());
}

// ============================================================================
// transfer (behavior-preserving: V1 roots reused)
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
fn golden_transfer_reverts_zero_sender() {
    let mut s = fresh();
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
// transferFrom (behavior-preserving: V1 roots reused)
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

// ============================================================================
// approve (behavior-preserving: V1 roots reused)
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

#[test]
fn golden_approve_reverts_zero_approver() {
    let mut s = fresh();
    let err = op(
        &mut s,
        Address::ZERO,
        FakePolicyAccounting::new(),
        IB20::approveCall { spender: BOB, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidApprover { approver: Address::ZERO }));
}

// ============================================================================
// mint (behavior-preserving: V1 roots reused)
// ============================================================================

#[test]
fn golden_mint_privileged_still_enforces_receiver_policy() {
    let mut s = fresh();
    let mut policy = FakePolicyAccounting::new();
    policy.allow(0, BOB);
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
// burn / burnBlocked (behavior-preserving: V1 roots reused)
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
        t.set_policy_id(B20PolicyType::TransferSender.id(), POLICY_ID).unwrap();
    });
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
    policy.allow(POLICY_ID, ALICE);
    let err = op_privileged(
        &mut s,
        ADMIN,
        policy,
        IB20::burnBlockedCall { from: ALICE, amount: u(1) }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotBlocked { account: ALICE }));
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

// ============================================================================
// seizeWithMemo (new at V2 — V1's default trait body rejects it)
// ============================================================================

#[test]
fn golden_seize_moves_balance_emits_transfer_memo_seized() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        make_seizable(t);
    });
    let out = op(
        &mut s,
        ADMIN,
        seize_receiver_policy(BOB),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(40), memo: MEMO }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty(), "seizeWithMemo is a void admin op (no bool return)");
    read(&mut s, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(60));
        assert_eq!(t.balance_of(BOB).unwrap(), u(40));
        assert_eq!(t.total_supply().unwrap(), u(100), "seize is a transfer, not a burn");
    });
    let events = s.get_events(TOKEN);
    assert_eq!(events[events.len() - 3].topics()[0], IB20::Transfer::SIGNATURE_HASH);
    assert_eq!(events[events.len() - 2].topics()[0], IB20::Memo::SIGNATURE_HASH);
    assert_eq!(
        events[events.len() - 1],
        IB20::Seized { caller: ADMIN, from: ALICE, to: BOB, amount: u(40) }.encode_log_data()
    );
    assert_root("seize", s, ROOT_SEIZE);
}

#[test]
fn golden_seize_reverts_missing_role() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        make_seizable(t);
    });
    let err = op(
        &mut s,
        ADMIN,
        seize_receiver_policy(BOB),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ADMIN,
            neededRole: B20TokenRole::Seize.id(),
        })
    );
}

#[test]
fn golden_seize_reverts_zero_receiver() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        make_seizable(t);
    });
    let err = op(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::seizeWithMemoCall { from: ALICE, to: Address::ZERO, amount: u(1), memo: MEMO }
            .abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO }));
}

#[test]
fn golden_seize_reverts_account_not_seizable() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        // ALICE authorized under SeizeHolder => not seizable.
        t.set_policy_id(B20PolicyType::SeizeHolder.id(), POLICY_ID).unwrap();
    });
    let mut policy = FakePolicyAccounting::new();
    policy.allow(POLICY_ID, ALICE);
    let err = op(
        &mut s,
        ADMIN,
        policy,
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(err, BasePrecompileError::revert(IB20::AccountNotSeizable { account: ALICE }));
}

#[test]
fn golden_seize_reverts_insufficient_balance() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(10));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        make_seizable(t);
    });
    let err = op(
        &mut s,
        ADMIN,
        seize_receiver_policy(BOB),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(50), memo: MEMO }.abi_encode(),
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
fn golden_seize_reverts_when_paused() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        make_seizable(t);
    });
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::SEIZE] }.abi_encode(),
    )
    .unwrap();
    let err = op(
        &mut s,
        ADMIN,
        seize_receiver_policy(BOB),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::ContractPaused { feature: IB20::PausableFeature::SEIZE })
    );
}

#[test]
fn golden_seize_enforces_receiver_policy() {
    let mut s = fresh();
    seed(&mut s, |t| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Seize.id(), ADMIN);
        make_seizable(t);
    });
    // No FakePolicyAccounting authorization for BOB under POLICY_ID_2 (SeizeReceiver) => blocked.
    let err = op(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::PolicyForbids {
            policyScope: B20PolicyType::SeizeReceiver.id(),
            policyId: POLICY_ID_2,
        })
    );
}

#[test]
fn golden_seize_privileged_still_enforces_role_and_seizable() {
    // seize_with_memo never accepts a `privileged` bypass: factory-privileged dispatch still
    // enforces both the role and the seizability guard.
    let mut s = fresh();
    seed(&mut s, |t| fund(t, ALICE, u(100)));
    let err = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(1), memo: MEMO }.abi_encode(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
            account: ADMIN,
            neededRole: B20TokenRole::Seize.id(),
        })
    );
}

#[test]
fn golden_read_seize_role_and_policy_constants() {
    let mut s = fresh();
    let cases: Vec<(Vec<u8>, B256)> = vec![
        (IB20::SEIZE_ROLECall {}.abi_encode(), B20TokenRole::Seize.id()),
        (IB20::SEIZE_HOLDER_POLICYCall {}.abi_encode(), B20PolicyType::SeizeHolder.id()),
        (IB20::SEIZE_RECEIVER_POLICYCall {}.abi_encode(), B20PolicyType::SeizeReceiver.id()),
    ];
    for (calldata, expected) in cases {
        let out = op(&mut s, ALICE, FakePolicyAccounting::new(), calldata).unwrap();
        assert_eq!(out, Bytes::from(expected.abi_encode()));
    }
    assert_root("read_seize_constants", s, ROOT_FRESH);
}

// ============================================================================
// pause / unpause (behavior-preserving: V1 roots reused)
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

#[test]
fn golden_read_is_paused_and_paused_features_includes_seize() {
    let mut s = fresh();
    op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::pauseCall { features: vec![IB20::PausableFeature::SEIZE] }.abi_encode(),
    )
    .unwrap();

    let paused_seize = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::isPausedCall { feature: IB20::PausableFeature::SEIZE }.abi_encode(),
    )
    .unwrap();
    assert_eq!(paused_seize, Bytes::from(true.abi_encode()));

    let features =
        op(&mut s, ALICE, FakePolicyAccounting::new(), IB20::pausedFeaturesCall {}.abi_encode())
            .unwrap();
    assert_eq!(features, Bytes::from(vec![IB20::PausableFeature::SEIZE].abi_encode()));
}

// ============================================================================
// config / metadata (behavior-preserving: V1 roots reused)
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

// ============================================================================
// roles (behavior-preserving: V1 roots reused)
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
fn golden_grant_role_unprivileged_no_admin_reverts() {
    // No admin exists yet => the admin-availability guard reverts.
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
    // An admin exists, but ALICE is not the role's admin => the role check reverts.
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
    // ALICE already holds MINT_ROLE => grant is a no-op (no event, count unchanged).
    let out = op_privileged(
        &mut s,
        ADMIN,
        FakePolicyAccounting::new(),
        IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
    )
    .unwrap();

    assert!(out.is_empty());
    read(&mut s, |t| assert!(t.has_role(B20TokenRole::Mint.id(), ALICE).unwrap()));
    assert_root("grant_idempotent", s, ROOT_GRANT_IDEMPOTENT);
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
fn golden_revoke_role_noop_when_not_held() {
    let mut s = fresh();
    // ALICE does not hold MINT_ROLE => revoke is a no-op; state stays at fresh.
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

// ============================================================================
// policy (behavior-preserving: V1 roots reused)
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

#[test]
fn golden_update_policy_unprivileged_requires_role() {
    assert_unprivileged_requires_role(
        IB20::updatePolicyCall { policyScope: B20PolicyType::TransferSender.id(), newPolicyId: 1 }
            .abi_encode(),
        B20TokenRole::DefaultAdmin.id(),
    );
}

// ============================================================================
// permit (behavior-preserving: V1 roots reused)
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
// computed + direct + constant reads (behavior-preserving: V1 roots reused)
// ============================================================================

#[test]
fn golden_read_currency() {
    let mut s = fresh();
    let out = op(
        &mut s,
        ALICE,
        FakePolicyAccounting::new(),
        IB20Stablecoin::currencyCall {}.abi_encode(),
    )
    .unwrap();
    assert_eq!(out, Bytes::from(CURRENCY.abi_encode()));
    assert_root("read_currency", s, ROOT_FRESH);
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
// gas: storage-access footprint per op
// ============================================================================
//
// `gas_deducted` is 0 under the test gas schedule, so we pin the deterministic,
// schedule-independent signal instead: the SLOAD / SSTORE / KECCAK256 op counts a
// call performs. These are the storage-access footprint that drives real gas, so a
// change here (e.g. an extra SLOAD in V2) is caught even when bytes/state/events match.

/// Runs `calldata` privileged after `setup`, returning `(sload, sstore, keccak256)` counts.
fn gas(
    setup: impl FnOnce(&mut B20StablecoinStorage<'_>),
    caller: Address,
    policy: FakePolicyAccounting,
    calldata: Vec<u8>,
) -> (u64, u64, u64) {
    let mut s = fresh();
    seed(&mut s, setup);
    s.set_caller(caller);
    s.reset_counters();
    StorageCtx::enter(&mut s, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            policy,
            PolicyVersion::V2,
        )
        .route(ctx, &calldata, StablecoinVersion::V2, true, NoopPrecompileCallObserver)
    })
    .expect("gas-footprint op must succeed");
    (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
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
            "seize",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    give_role(t, B20TokenRole::Seize.id(), ADMIN);
                    make_seizable(t);
                },
                ADMIN,
                seize_receiver_policy(BOB),
                IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(40), memo: MEMO }
                    .abi_encode(),
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
            "transfer_with_memo",
            gas(
                |t| fund(t, ALICE, u(100)),
                ALICE,
                FakePolicyAccounting::new(),
                IB20::transferWithMemoCall { to: BOB, amount: u(30), memo: MEMO }.abi_encode(),
            ),
        ),
        (
            "transfer_from_with_memo",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    t.set_allowance(ALICE, BOB, u(40)).unwrap();
                },
                BOB,
                FakePolicyAccounting::new(),
                IB20::transferFromWithMemoCall { from: ALICE, to: BOB, amount: u(30), memo: MEMO }
                    .abi_encode(),
            ),
        ),
        (
            "mint_with_memo",
            gas(
                |_t| {},
                ADMIN,
                allow0(BOB),
                IB20::mintWithMemoCall { to: BOB, amount: u(100), memo: MEMO }.abi_encode(),
            ),
        ),
        (
            "burn_with_memo",
            gas(
                |t| {
                    fund(t, ALICE, u(100));
                    give_role(t, B20TokenRole::Burn.id(), ALICE);
                },
                ALICE,
                FakePolicyAccounting::new(),
                IB20::burnWithMemoCall { amount: u(40), memo: MEMO }.abi_encode(),
            ),
        ),
        (
            "renounce_role",
            gas(
                |t| give_role(t, B20TokenRole::Mint.id(), ALICE),
                ALICE,
                FakePolicyAccounting::new(),
                IB20::renounceRoleCall { role: B20TokenRole::Mint.id(), callerConfirmation: ALICE }
                    .abi_encode(),
            ),
        ),
        (
            "renounce_last_admin",
            gas(
                |t| give_role(t, B20TokenRole::DefaultAdmin.id(), ADMIN),
                ADMIN,
                FakePolicyAccounting::new(),
                IB20::renounceLastAdminCall {}.abi_encode(),
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
        ("seize", (6, 2, 0)),
        ("pause", (1, 1, 0)),
        ("unpause", (1, 1, 0)),
        ("update_supply_cap", (2, 1, 0)),
        ("update_name", (1, 1, 0)),
        ("update_symbol", (1, 1, 0)),
        ("update_contract_uri", (1, 1, 0)),
        ("grant_role", (1, 1, 0)),
        ("revoke_role", (1, 1, 0)),
        ("set_role_admin", (1, 1, 0)),
        ("update_policy", (2, 1, 0)),
        ("transfer_with_memo", (3, 2, 0)),
        ("transfer_from_with_memo", (4, 3, 0)),
        ("mint_with_memo", (5, 2, 0)),
        ("burn_with_memo", (4, 2, 0)),
        ("renounce_role", (1, 1, 0)),
        ("renounce_last_admin", (4, 2, 0)),
        ("permit", (3, 2, 0)),
    ];

    bless_or_assert_gas(&actual, expected);
}

/// `emit_memo` only pushes a `Memo` event — no SLOAD/SSTORE/KECCAK256 — so every `*WithMemo` op's
/// storage-access footprint must exactly match its base op's. Checked as a live relationship
/// (not just two pinned tuples that happen to agree): a future change that makes `emit_memo`
/// storage-touching would fail this even if both `golden_gas_footprints` pins were updated
/// consistently but incorrectly.
#[test]
fn golden_gas_footprint_memo_variants_add_no_storage_cost() {
    let transfer = gas(
        |t| fund(t, ALICE, u(100)),
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(30) }.abi_encode(),
    );
    let transfer_with_memo = gas(
        |t| fund(t, ALICE, u(100)),
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferWithMemoCall { to: BOB, amount: u(30), memo: MEMO }.abi_encode(),
    );
    assert_eq!(transfer, transfer_with_memo, "transferWithMemo must cost exactly transfer");

    let mint_setup = |_t: &mut B20StablecoinStorage<'_>| {};
    let mint = gas(
        mint_setup,
        ADMIN,
        allow0(BOB),
        IB20::mintCall { to: BOB, amount: u(100) }.abi_encode(),
    );
    let mint_with_memo = gas(
        mint_setup,
        ADMIN,
        allow0(BOB),
        IB20::mintWithMemoCall { to: BOB, amount: u(100), memo: MEMO }.abi_encode(),
    );
    assert_eq!(mint, mint_with_memo, "mintWithMemo must cost exactly mint");

    let burn_setup = |t: &mut B20StablecoinStorage<'_>| {
        fund(t, ALICE, u(100));
        give_role(t, B20TokenRole::Burn.id(), ALICE);
    };
    let burn = gas(
        burn_setup,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnCall { amount: u(40) }.abi_encode(),
    );
    let burn_with_memo = gas(
        burn_setup,
        ALICE,
        FakePolicyAccounting::new(),
        IB20::burnWithMemoCall { amount: u(40), memo: MEMO }.abi_encode(),
    );
    assert_eq!(burn, burn_with_memo, "burnWithMemo must cost exactly burn");
}

/// `seize_with_memo` shares `move_balance` with `transfer` — both write exactly the two balance
/// slots and nothing else — so their SSTORE counts must match even though `seize` performs
/// strictly more SLOADs (role + seizable + receiver-policy guards on top of the balance reads).
/// A future change that made seize write anything beyond the balance move (e.g. an extra
/// per-account counter) would silently escape `golden_gas_footprints`' independent pins but
/// fail this relationship.
#[test]
fn golden_gas_footprint_seize_writes_no_more_than_transfer() {
    let transfer = gas(
        |t| fund(t, ALICE, u(100)),
        ALICE,
        FakePolicyAccounting::new(),
        IB20::transferCall { to: BOB, amount: u(30) }.abi_encode(),
    );
    let seize = gas(
        |t| {
            fund(t, ALICE, u(100));
            give_role(t, B20TokenRole::Seize.id(), ADMIN);
            make_seizable(t);
        },
        ADMIN,
        seize_receiver_policy(BOB),
        IB20::seizeWithMemoCall { from: ALICE, to: BOB, amount: u(30), memo: MEMO }.abi_encode(),
    );
    assert_eq!(seize.1, transfer.1, "seize and transfer must write the same number of slots");
    assert!(seize.0 >= transfer.0, "seize must read at least as much as transfer (extra guards)");
}

// ============================================================================
// meta: op coverage checklist
// ============================================================================

/// Compile-time coverage checklist — never called; it exists only for its two
/// exhaustive `match`es (no `_` arm), each arm naming the golden `#[test]` fn(s) that
/// pin the op via [`covered`].
///
/// This gives two compile-time guarantees:
///   * add an op to the ABI (a new `IB20Calls` / `IB20StablecoinCalls` variant) → the
///     wildcard-free match fails to build until an arm (and thus a golden) is added;
///   * rename or remove a golden `#[test]` fn → the `covered(&[...])` reference fails
///     to build.
///
/// Unlike V1's frozen checklist, V2 is the live version: new selectors are expected to
/// land here over time, each pulling in a new golden test.
#[allow(dead_code)]
fn v2_op_coverage_checklist(call: IB20::IB20Calls, ext: IB20Stablecoin::IB20StablecoinCalls) {
    use IB20::IB20Calls as C;
    use IB20Stablecoin::IB20StablecoinCalls as SC;

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

        // seize (new at V2)
        C::seizeWithMemo(_) => covered(&[
            golden_seize_moves_balance_emits_transfer_memo_seized,
            golden_seize_reverts_missing_role,
            golden_seize_reverts_zero_receiver,
            golden_seize_reverts_account_not_seizable,
            golden_seize_reverts_insufficient_balance,
            golden_seize_reverts_when_paused,
            golden_seize_enforces_receiver_policy,
            golden_seize_privileged_still_enforces_role_and_seizable,
        ]),
        C::SEIZE_ROLE(_) | C::SEIZE_HOLDER_POLICY(_) | C::SEIZE_RECEIVER_POLICY(_) => {
            covered(&[golden_read_seize_role_and_policy_constants])
        }

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
            covered(&[golden_read_is_paused_and_paused_features_includes_seize])
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
        SC::currency(_) => covered(&[golden_read_currency]),
    }
}
