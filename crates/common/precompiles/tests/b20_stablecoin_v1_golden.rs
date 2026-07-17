//! Golden tests pinning Stablecoin **V1** behavior of the B-20 precompile (BOP-425).
//!
//! Every op (mutations, computed reads, direct/const reads) is driven through the
//! **version-resolver-gated** dispatch path (`BaseUpgrade::Beryl` -> `StablecoinVersion::V1`)
//! against the real EVM-backed `B20StablecoinStorage` over `HashMapStorageProvider`, with an
//! `InMemoryPolicy` for deterministic allow/block decisions. Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting state (balances / supply / roles / allowances / storage),
//!   3. emitted events, and
//!   4. a per-case keccak storage **hash** snapshot (the frozen-manifest baseline).
//!
//! Because the per-op suite resolves the version via `StablecoinVersions::from_base_upgrade`,
//! it breaks if dispatch ever routes to the wrong version. Privileged behavior is exercised via
//! `inner_with_privilege`; the guard envelope (nonpayable / uninitialized / pre-Beryl) via the
//! full `dispatch_with_observer`.
//!
//! ## Blessing storage hashes
//! State-root constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_stablecoin_v1_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, LogData, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    B20_MAX_SUPPLY_CAP, B20PolicyType, B20StablecoinInit, B20StablecoinStorage, B20StablecoinToken,
    B20TokenRole, IB20, IB20Stablecoin, InMemoryPolicy, NoopPrecompileCallObserver, PermitArgs,
    Stablecoin, StablecoinV1, StablecoinVersion, StablecoinVersions, TokenAccounting,
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
const NAME: &str = "USD Coin";
const SYMBOL: &str = "USDC";
const CURRENCY: &str = "USD";
const MEMO: B256 = B256::repeat_byte(0x77);
const LOGIC: StablecoinV1 = StablecoinV1;

/// A concrete (non-sentinel) policy id. Unconfigured scopes default to the
/// `ALWAYS_ALLOW_ID` (0) EVM zero-slot, so blocking/executor guards must be
/// exercised against an explicitly configured policy id like this one.
const POLICY_ID: u64 = 7;

// Anvil/Hardhat account 0 — well-known test key, never used in production.
const PRIVATE_KEY: [u8; 32] =
    alloy_primitives::hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------

const ROOT_FRESH: B256 = b256!("7f52ac593dc5c5de5e040f65148db8c081010c85db757516d9eb2c19e8903951");
const ROOT_TRANSFER_PRIV: B256 =
    b256!("55bdd0b008a5e28bd9dee4572766a7bce75b0147fb614c9b4874963fc18ef390");
const ROOT_TRANSFER_UNPRIV: B256 =
    b256!("77c291321163d01cb51fb226b7f25efd68819c8a60cc463f4863dbfccae49c03");
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
    b256!("9908f1eb41cd484b52fd364415296117e80ffb86c8222973f921032f28c85cbf");
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
const ROOT_GRANT_UNCHECKED: B256 =
    b256!("c83dd3df2a6f62c0775fb908d7a921f65c8e0e735bdb9b0adf7d1e489b657688");

// --- harness ----------------------------------------------------------------

/// `U256` from a small literal.
fn u(n: u64) -> U256 {
    U256::from(n)
}

/// The ABI encoding for a boolean-returning op (`transfer`/`approve`).
fn ok_true() -> Bytes {
    Bytes::from(true.abi_encode())
}

/// Fresh provider with an initialized `USD Coin` stablecoin at [`TOKEN`].
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new(CHAIN_ID);
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

/// Drives one op through the resolver-gated (`Beryl` -> V1) unprivileged path.
fn op(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    policy: InMemoryPolicy,
    calldata: Vec<u8>,
) -> Result<Bytes, BasePrecompileError> {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            policy,
        )
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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            policy,
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

/// Recovers the anvil account-0 address from [`PRIVATE_KEY`].
fn anvil_owner() -> Address {
    let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// The V1 EIP-712 domain separator for the token at [`TOKEN`] on [`CHAIN_ID`].
fn domain_separator(storage: &mut HashMapStorageProvider) -> B256 {
    StorageCtx::enter(storage, |ctx| {
        let token = B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
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
    assert_eq!(StablecoinVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    assert_eq!(
        StablecoinVersions::from_base_upgrade(BaseUpgrade::Beryl),
        Some(StablecoinVersion::V1)
    );
    assert_eq!(
        StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt),
        Some(StablecoinVersion::V1)
    );
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
    // Authorize sender + receiver under the configured policy => guards pass.
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
        // Configure a real sender policy that authorizes nobody => sender blocked.
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
    });
    seed(&mut s, |t| {
        t.set_policy_id(B20PolicyType::TransferExecutor.id(), POLICY_ID).unwrap();
    });
    // BOB (executor, != from) is not authorized under the executor policy => forbidden.
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
    // Missing MINT_ROLE => unauthorized.
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

    // With MINT_ROLE + authorized receiver => succeeds.
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
    // ALICE blocked; privileged skips the role check.
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
// computed reads
// ============================================================================

#[test]
fn golden_read_currency() {
    let mut s = fresh();
    let out =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20Stablecoin::currencyCall {}.abi_encode())
            .unwrap();
    assert_eq!(out, Bytes::from(CURRENCY.abi_encode()));
    assert_root("read_currency", s, ROOT_FRESH);
}

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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
    let err =
        op(&mut s, ALICE, InMemoryPolicy::new(), IB20::burnCall { amount: u(50) }.abi_encode())
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
    let err = op(&mut s, ALICE, InMemoryPolicy::new(), calldata).unwrap_err();
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        InMemoryPolicy::new(),
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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            InMemoryPolicy::new(),
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
        B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            InMemoryPolicy::new(),
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
        let mut token = B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(TOKEN, ctx),
            InMemoryPolicy::new(),
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
// gas: storage-access footprint per op
// ============================================================================
//
// `gas_deducted` is 0 under the test gas schedule, so we pin the deterministic,
// schedule-independent signal instead: the SLOAD / SSTORE / KECCAK256 op counts a
// call performs. These are the storage-access footprint that drives real gas, so a
// change here (e.g. an extra SLOAD in V1) is caught even when bytes/state/events match.

/// Runs `calldata` privileged after `setup`, returning `(sload, sstore, keccak256)` counts.
fn gas(
    setup: impl FnOnce(&mut B20StablecoinStorage<'_>),
    caller: Address,
    policy: InMemoryPolicy,
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
        )
        .inner_with_privilege(ctx, &calldata, true)
    })
    .expect("gas-footprint op must succeed");
    (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
}

/// An `InMemoryPolicy` authorizing `who` under the default (0) scope.
fn allow0(who: Address) -> InMemoryPolicy {
    let mut p = InMemoryPolicy::new();
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
                InMemoryPolicy::new(),
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
                InMemoryPolicy::new(),
                IB20::transferFromCall { from: ALICE, to: BOB, amount: u(30) }.abi_encode(),
            ),
        ),
        (
            "approve",
            gas(
                |_t| {},
                ALICE,
                InMemoryPolicy::new(),
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
                InMemoryPolicy::new(),
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
                InMemoryPolicy::new(),
                IB20::burnBlockedCall { from: ALICE, amount: u(40) }.abi_encode(),
            ),
        ),
        (
            "pause",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::pauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
            ),
        ),
        (
            "unpause",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::unpauseCall { features: vec![IB20::PausableFeature::MINT] }.abi_encode(),
            ),
        ),
        (
            "update_supply_cap",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::updateSupplyCapCall { newSupplyCap: u(1_000) }.abi_encode(),
            ),
        ),
        (
            "update_name",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::updateNameCall { newName: "New Name".into() }.abi_encode(),
            ),
        ),
        (
            "update_symbol",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::updateSymbolCall { newSymbol: "USDX".into() }.abi_encode(),
            ),
        ),
        (
            "update_contract_uri",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::updateContractURICall { newURI: "ipfs://x".into() }.abi_encode(),
            ),
        ),
        (
            "grant_role",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
                IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
            ),
        ),
        (
            "revoke_role",
            gas(
                |t| give_role(t, B20TokenRole::Mint.id(), ALICE),
                ADMIN,
                InMemoryPolicy::new(),
                IB20::revokeRoleCall { role: B20TokenRole::Mint.id(), account: ALICE }.abi_encode(),
            ),
        ),
        (
            "set_role_admin",
            gas(
                |_t| {},
                ADMIN,
                InMemoryPolicy::new(),
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
                    let mut p = InMemoryPolicy::new();
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
///   * add an op to the ABI (a new `IB20Calls` / `IB20StablecoinCalls` variant) → the
///     wildcard-free match fails to build until an arm (and thus a golden) is added;
///   * rename or remove a golden `#[test]` fn → the `covered(&[...])` reference fails
///     to build.
///
/// Because Stablecoin V1 is **frozen**, this checklist is NOT expected to ever be
/// updated: a compile error here means the frozen V1 op surface changed, which must be
/// reviewed.
#[allow(dead_code)]
fn v1_op_coverage_checklist(call: IB20::IB20Calls, ext: IB20Stablecoin::IB20StablecoinCalls) {
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
        SC::currency(_) => covered(&[golden_read_currency]),
    }
}
